using SessionHub.Core.Protocol;
using SessionHub.Core.ViewModels;

namespace SessionHub.Core.App;

/// <summary>Which send-box action started a send.</summary>
public enum SendMode
{
    /// <summary>Enter or the Send button.</summary>
    Normal,

    /// <summary>"Send anyway" after <c>send-in-flight</c> (<see cref="SendCoordinator.OverrideAsync"/>).</summary>
    OverrideInFlight,

    /// <summary>"Send anyway" after <c>looks-secret</c> (<see cref="SendCoordinator.SendSecretAnywayAsync"/>).</summary>
    AllowSecret,
}

/// <summary>
/// Sends a card's draft through the bridge and routes <c>send</c> events back to the card. A send
/// that times out is never retried (it may have gone out); <c>send-in-flight</c> waits for the owner's
/// "Send anyway", which sends again with override. Only the newest send of a card updates it. Sends
/// the bridge can no longer report on (it restarted, or <see cref="TrackLimit"/> passed) are settled
/// with the "may have been sent" text, so nothing spins forever. Call on the UI thread.
/// </summary>
/// <remarks>
/// A <c>send</c> event can arrive before the reply that names its id has been handled (a relay that
/// fails at once): such events are kept briefly (<see cref="EarlyLimit"/>, <see cref="EarlyMaxAge"/>)
/// and replayed when the id is registered. A <c>failed</c> send is still watched until
/// <see cref="TrackLimit"/>: the relay can fail after the message went out, and a later
/// <c>delivered</c> then shows on the card (so the owner does not send it twice).
/// </remarks>
public sealed class SendCoordinator
{
    /// <summary>The bridge replies as soon as the send row exists, so this is generous.</summary>
    public static readonly TimeSpan Timeout = TimeSpan.FromSeconds(15);

    /// <summary>
    /// The bridge tracks a send for 31 minutes (SEND_TRACK_MS: the 30-minute delivery window plus two
    /// expiry ticks); after that no event comes.
    /// </summary>
    public static readonly TimeSpan TrackLimit = TimeSpan.FromMinutes(31);

    /// <summary>At most this many events for ids not registered yet are kept.</summary>
    public const int EarlyLimit = 32;

    /// <summary>An event for an id not registered yet is kept this long.</summary>
    public static readonly TimeSpan EarlyMaxAge = TimeSpan.FromMinutes(1);

    private const string TimeoutCode = "timeout";

    private readonly Func<BridgeRequest, TimeSpan, Task<Reply>> request;
    private readonly Func<DateTimeOffset> clock;
    private readonly Dictionary<int, Entry> open = [];
    private readonly HashSet<CardViewModel> requesting = new(ReferenceEqualityComparer.Instance);
    private readonly List<(SendUpdate Update, DateTimeOffset At)> early = [];

    /// <summary>Goes up on every <see cref="SettleAll"/> (a bridge restart).</summary>
    private int restarts;

    public SendCoordinator(Func<BridgeRequest, TimeSpan, Task<Reply>> request, Func<DateTimeOffset>? clock = null)
    {
        ArgumentNullException.ThrowIfNull(request);
        this.request = request;
        this.clock = clock ?? (() => DateTimeOffset.UtcNow);
    }

    /// <summary>A send of this app session is not final yet (or its request is still out).</summary>
    public bool HasPending => requesting.Count > 0 || open.Values.Any(e => e.Busy);

    /// <summary>A send is still watched for events (including a failed one that may yet be delivered).</summary>
    public bool IsWatching => requesting.Count > 0 || open.Count > 0;

    /// <summary>
    /// Sends the card's answers and message (<see cref="AnswerFormViewModel.Capture"/>), or its draft when
    /// nothing waits on the owner. Ignored while that card's previous request is still out (a double Enter).
    /// <paramref name="allowSecret"/> skips the bridge's secret check; only <see cref="SendSecretAnywayAsync"/>
    /// and <see cref="OverrideAsync"/> set it, for text the owner already chose to send.
    /// </summary>
    /// <param name="itemKey">
    /// Set for one item's own Send: only that item's answer goes (<see cref="AnswerFormViewModel.CaptureOne"/>),
    /// and only its box is cleared afterwards.
    /// </param>
    public async Task SendAsync(CardViewModel card, bool overrideInFlight = false, bool allowSecret = false, string? itemKey = null)
    {
        ArgumentNullException.ThrowIfNull(card);
        var payload = Capture(card, itemKey);
        var text = payload.Text;
        if (string.IsNullOrWhiteSpace(text)) return;
        if (text.Length > AnswerComposer.MaxChars)
        {
            card.ShowSendError("too-long", null);
            return;
        }

        if (!requesting.Add(card)) return;

        card.IsRequesting = true;
        var restartsBefore = restarts;
        Reply reply;
        try
        {
            card.ShowSend("pending", null);
            reply = await request(
                new SendRequest(0, card.Session.SessionId, text, overrideInFlight ? true : null, allowSecret ? true : null),
                Timeout);
        }
        finally
        {
            requesting.Remove(card);
            card.IsRequesting = false;
        }

        if (!reply.Ok)
        {
            card.ShowSendError(reply.Error?.Code ?? "unknown", reply.Error?.Message, new SendAttempt(text, overrideInFlight, allowSecret, itemKey));

            // Answers or message typed while the request was out were not checked: "Send anyway" must not cover them.
            if (Capture(card, itemKey).Text != text) card.SendBox.TextEdited();
            return;
        }

        var result = Json.DecodeSend(reply);
        if (result is null)
        {
            card.ShowSendError("bad-reply", "the bridge sent an unreadable reply");
            return;
        }

        // Text typed while the request was out stays in its box.
        card.Answers.ClearSent(payload);

        // The newest send owns the card: an older one still open can no longer change its state line.
        foreach (var old in open.Where(o => ReferenceEquals(o.Value.Card, card)).Select(o => o.Key).ToList()) open.Remove(old);
        open[result.SendId] = new Entry(card, clock());
        Show(result.SendId, result.State, null);

        // Events that came before this registration, in the order they came.
        PruneEarly();
        foreach (var e in early.Where(e => e.Update.Send.Id == result.SendId).ToList())
        {
            early.Remove(e);
            Show(e.Update.Send.Id, e.Update.Send.State, e.Update.Send.Detail);
        }

        // The bridge restarted while the request was out: no event will come for this send.
        if (restarts != restartsBefore) Settle(result.SendId);
    }

    /// <summary>
    /// One item's own Send: <c>Answers:</c> and that item's line only, through the same checks, refusals
    /// and state line as <see cref="SendAsync(CardViewModel, bool, bool, string?)"/>. Ignored while the card's
    /// previous request is still out.
    /// </summary>
    public Task SendItemAsync(CardViewModel card, string itemKey)
    {
        ArgumentNullException.ThrowIfNull(itemKey);
        return SendAsync(card, itemKey: itemKey);
    }

    /// <summary>Starts the send a send-box action asks for.</summary>
    public Task SendAsync(CardViewModel card, SendMode mode) => mode switch
    {
        SendMode.OverrideInFlight => OverrideAsync(card),
        SendMode.AllowSecret => SendSecretAnywayAsync(card),
        _ => SendAsync(card),
    };

    /// <summary>
    /// The owner's "Send anyway" after a <c>looks-secret</c> reply: sends exactly the refused text (answers
    /// and message) again with allowSecret, keeping an override that was set. Does nothing without a
    /// current offer, and withdraws the offer when the text to send is no longer the refused one (a box
    /// was edited, or the waiting list changed). Never called by itself.
    /// </summary>
    public Task SendSecretAnywayAsync(CardViewModel card)
    {
        ArgumentNullException.ThrowIfNull(card);
        var box = card.SendBox;
        if (!box.CanSendAnyway || box.Refused is not { } refused) return Task.CompletedTask;
        if (refused.Text != Capture(card, refused.ItemKey).Text)
        {
            box.TextEdited();
            return Task.CompletedTask;
        }

        return SendAsync(card, refused.Override, allowSecret: true, refused.ItemKey);
    }

    /// <summary>
    /// The owner's "Send anyway" after a <c>send-in-flight</c> reply: sends with override. allowSecret is
    /// kept only when the refused request had it and the text to send (answers and message) is unchanged.
    /// </summary>
    public Task OverrideAsync(CardViewModel card)
    {
        ArgumentNullException.ThrowIfNull(card);
        var refused = card.SendBox.Refused;
        var itemKey = refused?.ItemKey;
        var allowSecret = refused is { AllowSecret: true } && refused.Text == Capture(card, itemKey).Text;
        return SendAsync(card, overrideInFlight: true, allowSecret, itemKey);
    }

    /// <summary>What a send takes now: the whole form, or one item's answer.</summary>
    private static SendPayload Capture(CardViewModel card, string? itemKey) =>
        itemKey is null ? card.Answers.Capture() : card.Answers.CaptureOne(itemKey);

    public void OnSendChanged(SendUpdate update)
    {
        ArgumentNullException.ThrowIfNull(update);
        if (open.ContainsKey(update.Send.Id))
        {
            Show(update.Send.Id, update.Send.State, update.Send.Detail);
            return;
        }

        // Maybe a send whose reply is still on its way: keep it for its registration.
        PruneEarly();
        if (early.Count >= EarlyLimit) early.RemoveAt(0);
        early.Add((update, clock()));
    }

    /// <summary>The bridge restarted: its relay children died with it, so no open send will report again.</summary>
    public void SettleAll()
    {
        restarts++;
        foreach (var id in open.Keys.ToList()) Settle(id);
    }

    /// <summary>Settles the sends tracked longer than <see cref="TrackLimit"/>. Call now and then.</summary>
    public void SettleExpired()
    {
        var now = clock();
        foreach (var (id, entry) in open.ToList())
        {
            if (now - entry.Since >= TrackLimit) Settle(id);
        }

        PruneEarly();
    }

    /// <summary>Stops watching a send; one still pending or held gets the "may have been sent" text.</summary>
    private void Settle(int sendId)
    {
        if (!open.Remove(sendId, out var entry)) return;
        if (entry.Busy) entry.Card.ShowSendError(TimeoutCode, null);
    }

    private void Show(int sendId, string state, string? detail)
    {
        if (!open.TryGetValue(sendId, out var entry)) return;
        entry.Card.ShowSend(state, detail);
        entry.Busy = state is "pending" or "held";

        // delivered and expired are final; failed is not (a later delivered can still come).
        if (state is "delivered" or "expired") open.Remove(sendId);
    }

    private void PruneEarly()
    {
        var cutoff = clock() - EarlyMaxAge;
        early.RemoveAll(e => e.At < cutoff);
    }

    private sealed class Entry(CardViewModel card, DateTimeOffset since)
    {
        public CardViewModel Card { get; } = card;

        public DateTimeOffset Since { get; } = since;

        /// <summary>Pending or held: not final, and the owner should not quit or resend yet.</summary>
        public bool Busy { get; set; } = true;
    }
}
