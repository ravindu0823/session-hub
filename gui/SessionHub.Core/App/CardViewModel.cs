using System.ComponentModel;
using System.Globalization;
using SessionHub.Core.Protocol;
using SessionHub.Core.ViewModels;

namespace SessionHub.Core.App;

/// <summary>Colour family of a chip; the view maps each to a soft-tone pill of the theme.</summary>
public enum ChipTone
{
    Teal,
    Violet,
    Amber,
    Accent,
    Grey,
    Red,
    Green,
}

public sealed record Chip(string Text, ChipTone Tone);

/// <summary>Card outline: amber when items wait on you, red when an approval is open.</summary>
public enum CardEdge
{
    None,
    Amber,
    Red,
}

/// <summary>Colour of the send state line.</summary>
public enum SendTone
{
    None,
    Pending,
    Delivered,
    Failed,
}

/// <summary>One message of a card's "Last exchange". <paramref name="FromOwner"/> sits on the right.</summary>
public sealed record ExchangeLine(bool FromOwner, string Meta, string Text);

/// <summary>
/// One session card of the panel: the session row plus what only the view needs (chips, age, last
/// exchange, send state). Lives as long as its <see cref="SessionViewModel"/>.
/// </summary>
public sealed class CardViewModel : ObservableObject
{
    // Avatar fills, all dark enough for white initials (AA for bold 13 px text).
    private static readonly string[] Palette =
        ["#2f6fb8", "#7a4fd1", "#23825a", "#c4561f", "#b8432f", "#6b5fa8", "#2f7f8f", "#8a5a1e"];

    private Chip statusChip = new("idle", ChipTone.Grey);
    private CardEdge edge;
    private bool isUnknown;
    private long updatedAt;
    private string subText = "";
    private string phaseText = "";
    private IReadOnlyList<string> remainingLines = [];
    private int progress;
    private bool hasProgress;
    private IReadOnlyList<ExchangeLine> lastExchange = [];
    private string? latestReplyUuid;
    private SendTone sendTone;
    private bool sendBusy;
    private DateTimeOffset now;
    private int revision;
    private bool isRequesting;
    private string listSubText = "";
    private Chip? listBadge;

    public CardViewModel(SessionViewModel session)
    {
        ArgumentNullException.ThrowIfNull(session);
        Session = session;
        Answers = new AnswerFormViewModel(session);
        var isPersonal = session.Profile.Contains("personal", StringComparison.OrdinalIgnoreCase);
        ProfileChip = new Chip(
            session.Profile.StartsWith('.') ? session.Profile : "." + session.Profile,
            isPersonal ? ChipTone.Violet : ChipTone.Teal);
        AvatarColor = Palette[(int)(StableHash(session.SessionId) % (uint)Palette.Length)];
        session.PropertyChanged += OnSessionChanged;
        // An edited answer box changes what would be sent, as an edited message does.
        session.AnswerChanged += (_, _) => SendBox.TextEdited();
        Recompute();
    }

    public SessionViewModel Session { get; }

    public SendBoxViewModel SendBox { get; } = new();

    /// <summary>The answer form of the Send views (active while the session is live and has waiting items).</summary>
    public AnswerFormViewModel Answers { get; }

    public Chip ProfileChip { get; }

    public string AvatarColor { get; }

    /// <summary>First two letters of the name, upper case, whole characters only; spaces are skipped.</summary>
    public string Initials
    {
        get
        {
            var e = StringInfo.GetTextElementEnumerator(Session.Name);
            var text = "";
            for (var n = 0; n < 2 && e.MoveNext();)
            {
                var element = e.GetTextElement();
                if (string.IsNullOrWhiteSpace(element)) continue;
                text += element;
                n++;
            }
            return text.Length == 0 ? "?" : text.ToUpperInvariant();
        }
    }

    /// <summary>Window list row: ".claude · Build" (profile and phase).</summary>
    public string ListSubText
    {
        get => listSubText;
        private set => SetField(ref listSubText, value);
    }

    /// <summary>Window list row badge: "approve" (red) for an open approval, the waiting count (amber), or none.</summary>
    public Chip? ListBadge
    {
        get => listBadge;
        private set => SetField(ref listBadge, value);
    }

    public Chip StatusChip
    {
        get => statusChip;
        private set => SetField(ref statusChip, value);
    }

    public CardEdge Edge
    {
        get => edge;
        private set => SetField(ref edge, value);
    }

    /// <summary>Jump to terminal works only while the session runs: an ended one has no window to show.</summary>
    public bool CanJump => Session.IsLive;

    /// <summary>The session's state is <c>unknown</c>: shown as idle with an extra "unknown" chip.</summary>
    public bool IsUnknown
    {
        get => isUnknown;
        private set => SetField(ref isUnknown, value);
    }

    public long UpdatedAt
    {
        get => updatedAt;
        private set => SetField(ref updatedAt, value);
    }

    /// <summary>"D:\work · 2 min ago".</summary>
    public string SubText
    {
        get => subText;
        private set => SetField(ref subText, value);
    }

    /// <summary>"Phase: Build · updated 2 min ago".</summary>
    public string PhaseText
    {
        get => phaseText;
        private set => SetField(ref phaseText, value);
    }

    public IReadOnlyList<string> RemainingLines
    {
        get => remainingLines;
        private set => SetField(ref remainingLines, value);
    }

    public bool HasRemaining => remainingLines.Count > 0;

    public bool HasWaiting => Session.Waiting.Count > 0;

    /// <summary>Some waiting item is neither answered nor dismissed: the "Waiting on you" box gets the amber edge.</summary>
    public bool HasOpenWaiting => Session.OpenItemCount > 0;

    public bool HasProgress
    {
        get => hasProgress;
        private set => SetField(ref hasProgress, value);
    }

    /// <summary>0 to 100.</summary>
    public int Progress
    {
        get => progress;
        private set => SetField(ref progress, value);
    }

    public string ProgressLabel => $"{progress}%";

    public IReadOnlyList<ExchangeLine> LastExchange
    {
        get => lastExchange;
        private set => SetField(ref lastExchange, value);
    }

    public bool HasExchange => lastExchange.Count > 0;

    /// <summary>The reply id of the newest assistant reply; when it changes an open card fetches its exchange again.</summary>
    public string? LatestReplyUuid
    {
        get => latestReplyUuid;
        private set => SetField(ref latestReplyUuid, value);
    }

    public SendTone SendTone
    {
        get => sendTone;
        private set => SetField(ref sendTone, value);
    }

    /// <summary>A send is pending or held: the view shows a spinner.</summary>
    public bool SendBusy
    {
        get => sendBusy;
        private set => SetField(ref sendBusy, value);
    }

    /// <summary>The send request of this card is out (set by <see cref="SendCoordinator"/>): Send is disabled.</summary>
    public bool IsRequesting
    {
        get => isRequesting;
        internal set => SetField(ref isRequesting, value);
    }

    /// <summary>Goes up each time a later snapshot brings a newer update of this session; the view flashes the card.</summary>
    public int Revision
    {
        get => revision;
        private set => SetField(ref revision, value);
    }

    /// <summary>Takes the snapshot values the session row does not keep. Call after <see cref="HubViewModel.Apply"/>.</summary>
    public void Update(SessionView view, DateTimeOffset at)
    {
        ArgumentNullException.ThrowIfNull(view);
        now = at;
        if (updatedAt != 0 && view.UpdatedAt != updatedAt) Revision++;
        UpdatedAt = view.UpdatedAt;
        LatestReplyUuid = view.LatestReply?.ReplyUuid;
        Recompute();
    }

    /// <summary>Refreshes the "… ago" texts.</summary>
    public void Tick(DateTimeOffset at)
    {
        now = at;
        RecomputeTimes();
    }

    /// <summary>Shows the last two messages of a history page (oldest first).</summary>
    public void SetExchange(IReadOnlyList<MessageRow> rows, TimeZoneInfo zone)
    {
        ArgumentNullException.ThrowIfNull(rows);
        LastExchange = [.. rows.Skip(Math.Max(0, rows.Count - 2)).Select(r => Line(r, zone))];
        Raise(nameof(HasExchange));
    }

    /// <summary>Shows a send state (from a reply or a <c>send</c> event).</summary>
    public void ShowSend(string state, string? detail)
    {
        SendBox.Show(state, detail);
        SendBusy = state is "pending" or "held";
        SendTone = state switch
        {
            "pending" or "held" => SendTone.Pending,
            "delivered" => SendTone.Delivered,
            _ => SendTone.Failed,
        };
    }

    /// <summary>Shows a failed <c>send</c> reply.</summary>
    public void ShowSendError(string code, string? message, SendAttempt? attempt = null)
    {
        SendBox.ShowReplyError(code, message, attempt);
        SendBusy = false;
        SendTone = SendTone.Failed;
    }

    private static ExchangeLine Line(MessageRow r, TimeZoneInfo zone)
    {
        var owner = r.Role == "owner";
        var who = r.Role switch
        {
            "owner" => "You",
            "assistant" => "Claude",
            _ => string.IsNullOrEmpty(r.FromName) ? r.Role : r.FromName,
        };
        var clock = TimeText.Clock(r.Ts, zone);
        return new ExchangeLine(owner, clock.Length == 0 ? who : $"{who} · {clock}", Cap(r.Text));
    }

    /// <summary>Longest message text a card shows; the window's History shows the rest.</summary>
    public const int MaxExchangeChars = 2000;

    /// <summary>Cuts to <see cref="MaxExchangeChars"/> plus "…", never splitting a surrogate pair.</summary>
    private static string Cap(string text)
    {
        if (text.Length <= MaxExchangeChars) return text;
        var keep = MaxExchangeChars;
        if (char.IsHighSurrogate(text[keep - 1])) keep--;
        return text[..keep] + "…";
    }

    private void OnSessionChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(SessionViewModel.Name):
                Raise(nameof(Initials));
                break;
            case nameof(SessionViewModel.Waiting):
                Raise(nameof(HasWaiting));
                break;
            case nameof(SessionViewModel.OpenItemCount):
                Raise(nameof(HasOpenWaiting));
                break;
            case nameof(SessionViewModel.EffectiveWaitingCount):
                // A dismissal or an undo in the tray changes the chips at once, not at the next snapshot.
                Recompute();
                break;
            case nameof(SessionViewModel.IsLive):
                Raise(nameof(CanJump));
                break;
            case nameof(SessionViewModel.Draft):
                SendBox.TextEdited();
                break;
        }
    }

    private void Recompute()
    {
        var s = Session;
        IsUnknown = s.IsLive && s.State == "unknown";
        StatusChip = s.NeedsApproval ? new Chip("needs approval", ChipTone.Red)
            : !s.IsLive ? new Chip("ended", ChipTone.Grey)
            : s.EffectiveWaitingCount > 0 ? new Chip($"{s.EffectiveWaitingCount} waiting on you", ChipTone.Amber)
            : s.State == "busy" ? new Chip("working", ChipTone.Accent)
            : new Chip("idle", ChipTone.Grey);
        Edge = s.NeedsApproval ? CardEdge.Red : s.IsLive && s.EffectiveWaitingCount > 0 ? CardEdge.Amber : CardEdge.None;
        ListBadge = s.NeedsApproval ? new Chip("approve", ChipTone.Red)
            : s.IsLive && s.EffectiveWaitingCount > 0 ? new Chip(s.EffectiveWaitingCount.ToString(CultureInfo.InvariantCulture), ChipTone.Amber)
            : null;
        ListSubText = $"{ProfileChip.Text} · {(string.IsNullOrEmpty(s.Phase) ? "no phase yet" : s.Phase)}";

        var pct = s.ProgressPct;
        HasProgress = pct is not null;
        var clamped = Math.Clamp(pct ?? 0, 0, 100);
        if (SetField(ref progress, clamped, nameof(Progress))) Raise(nameof(ProgressLabel));

        var lines = s.Remaining.Select((r, i) => $"{i + 1}. {r}").ToList();
        if (!lines.SequenceEqual(remainingLines))
        {
            RemainingLines = lines;
            Raise(nameof(HasRemaining));
        }

        RecomputeTimes();
    }

    private void RecomputeTimes()
    {
        var ago = updatedAt > 0 ? TimeText.Ago(updatedAt, now) : "";
        SubText = ago.Length == 0 ? Session.Cwd : $"{Session.Cwd} · {ago}";
        var phase = string.IsNullOrEmpty(Session.Phase) ? "Phase: none yet" : $"Phase: {Session.Phase}";
        PhaseText = ago.Length == 0 ? phase : $"{phase} · updated {ago}";
    }

    // FNV-1a: the same colour for a session on every start (string.GetHashCode is randomized).
    private static uint StableHash(string text)
    {
        var hash = 2166136261u;
        foreach (var c in text)
        {
            hash ^= c;
            hash *= 16777619u;
        }

        return hash;
    }
}
