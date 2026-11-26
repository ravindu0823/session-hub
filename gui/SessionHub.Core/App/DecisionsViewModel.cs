using System.Collections.ObjectModel;
using System.Globalization;
using SessionHub.Core.Protocol;
using SessionHub.Core.ViewModels;

namespace SessionHub.Core.App;

/// <summary>
/// One decision card: the question, its options and recommendation, and the owner's answer draft.
/// An answer goes to the bridge once; it is stored and mailed to the decision's role, and it answers
/// no permission prompt. Call on the UI thread.
/// </summary>
public sealed class DecisionCardViewModel : ObservableObject
{
    private DecisionView view;
    private string draft = "";
    private string status = "";
    private bool statusIsError;
    private bool canReplace;
    private bool isBusy;

    public DecisionCardViewModel(DecisionView view)
    {
        ArgumentNullException.ThrowIfNull(view);
        this.view = view;
    }

    public string Id => view.Id;

    public string Role => view.Role;

    public bool IsOpen => view.State == "open";

    /// <summary>"D-0001 · open · alpha".</summary>
    public string Header => $"{view.Id} · {view.State} · {view.Role}";

    public string Question => view.Question;

    /// <summary>"Options: yes | no", or empty.</summary>
    public string OptionsText => view.Options.Count == 0 ? "" : "Options: " + string.Join(" | ", view.Options);

    /// <summary>"Recommended: yes", or empty.</summary>
    public string RecommendationText => view.Recommendation is null ? "" : "Recommended: " + view.Recommendation;

    /// <summary>"Answered: yes (cli). 1 other answer was not applied or was replaced.", or empty while open.</summary>
    public string AnswerText
    {
        get
        {
            if (view.Answer is null) return "";
            var text = $"Answered: {view.Answer} ({view.AnsweredVia ?? "?"}).";
            if (view.Conflicts <= 0) return text;
            var n = view.Conflicts.ToString(CultureInfo.InvariantCulture);
            return text + (view.Conflicts == 1
                ? " 1 other answer was not applied or was replaced."
                : $" {n} other answers were not applied or were replaced.");
        }
    }

    public string Draft
    {
        get => draft;
        set
        {
            if (!SetField(ref draft, value ?? "")) return;
            Raise(nameof(CanAnswer));
        }
    }

    /// <summary>What happened to the last answer from here; empty before one.</summary>
    public string Status
    {
        get => status;
        private set => SetField(ref status, value);
    }

    public bool StatusIsError
    {
        get => statusIsError;
        private set => SetField(ref statusIsError, value);
    }

    /// <summary>The decision has a different answer already; Replace sends the draft in its place.</summary>
    public bool CanReplace
    {
        get => canReplace;
        private set => SetField(ref canReplace, value);
    }

    public bool IsBusy
    {
        get => isBusy;
        private set
        {
            if (!SetField(ref isBusy, value)) return;
            Raise(nameof(CanAnswer));
        }
    }

    public bool CanAnswer => !isBusy && draft.Trim().Length > 0;

    internal void Update(DecisionView next)
    {
        var before = view;
        view = next;
        if (before == next) return;
        Raise(nameof(Role));
        Raise(nameof(IsOpen));
        Raise(nameof(Header));
        Raise(nameof(Question));
        Raise(nameof(OptionsText));
        Raise(nameof(RecommendationText));
        Raise(nameof(AnswerText));
    }

    internal void Begin()
    {
        IsBusy = true;
        Status = "Sending…";
        StatusIsError = false;
    }

    internal void Finish(string text, bool isError, bool replace, bool clearDraft)
    {
        IsBusy = false;
        Status = text;
        StatusIsError = isError;
        CanReplace = replace;
        if (clearDraft) Draft = "";
    }
}

/// <summary>
/// The owner's decision list in the panel: open decisions, answered ones with a conflict, and notices
/// about mail that waits for a role. Answers go through the bridge (one request, never retried).
/// Call on the UI thread.
/// </summary>
public sealed class DecisionsViewModel : ObservableObject
{
    /// <summary>How long an answer waits for the bridge's reply.</summary>
    public static readonly TimeSpan AnswerTimeout = TimeSpan.FromSeconds(15);

    private readonly Func<BridgeRequest, TimeSpan, Task<Reply>> request;
    private int openCount;
    private bool hasAny;

    /// <param name="request">The bridge client's <c>RequestAsync</c>; it never throws.</param>
    public DecisionsViewModel(Func<BridgeRequest, TimeSpan, Task<Reply>> request)
    {
        ArgumentNullException.ThrowIfNull(request);
        this.request = request;
    }

    public ObservableCollection<DecisionCardViewModel> Items { get; } = [];

    /// <summary>"Mail waits for role docs (1 queued): start a session named docs." and the like.</summary>
    public ObservableCollection<string> Notices { get; } = [];

    /// <summary>Open decisions: they count toward the Waiting badge.</summary>
    public int OpenCount
    {
        get => openCount;
        private set => SetField(ref openCount, value);
    }

    /// <summary>A card or a notice to show.</summary>
    public bool HasAny
    {
        get => hasAny;
        private set => SetField(ref hasAny, value);
    }

    /// <summary>
    /// Merges a snapshot's mail section. Cards keep their instance (and draft) by decision id; a
    /// decision that is no longer open and has no conflict leaves the list. Null clears everything.
    /// </summary>
    public void Apply(MailSummary? mail)
    {
        var shown = mail is null
            ? []
            : mail.Decisions.Where(d => d.State == "open" || d.Conflicts > 0).ToList();
        var byId = Items.ToDictionary(c => c.Id, StringComparer.Ordinal);
        var wanted = new List<DecisionCardViewModel>(shown.Count);
        foreach (var d in shown)
        {
            if (byId.TryGetValue(d.Id, out var card)) card.Update(d);
            else card = new DecisionCardViewModel(d);
            wanted.Add(card);
        }

        // No reorder: a Move would rebuild the card under the answer box's caret.
        StableList.Sync(Items, wanted, reorder: false);
        SyncNotices(mail is null ? [] : NoticesOf(mail));
        OpenCount = shown.Count(d => d.State == "open");
        HasAny = Items.Count > 0 || Notices.Count > 0;
    }

    /// <summary>Sends the card's draft once. An empty draft or a card already sending does nothing.</summary>
    public async Task AnswerAsync(DecisionCardViewModel card, bool replace)
    {
        ArgumentNullException.ThrowIfNull(card);
        var text = card.Draft.Trim();
        if (text.Length == 0 || card.IsBusy) return;
        card.Begin();
        var reply = await request(new AnswerRequest(0, card.Id, text, replace ? true : null), AnswerTimeout)
            .ConfigureAwait(true);
        if (!reply.Ok)
        {
            var code = reply.Error?.Code;
            var message = reply.Error?.Message ?? "unknown error";
            // A timeout or a bridge restart after the write: the answer may be stored. Never retried.
            var text2 = code is "timeout" or "bridge-restarted"
                ? "No reply from the hub in time; the answer may have been stored. Check this list before you answer again."
                : $"Not sent: {message}";
            card.Finish(text2, isError: true, replace: false, clearDraft: false);
            return;
        }

        var result = Json.DecodeAnswer(reply);
        var role = result?.Role ?? card.Role;
        switch (result?.Result)
        {
            case "answered":
                card.Finish($"Answered; mailed to {role}.", isError: false, replace: false, clearDraft: true);
                break;
            case "replaced":
                card.Finish($"Answer changed; mailed to {role}.", isError: false, replace: false, clearDraft: true);
                break;
            case "same":
                card.Finish("Already answered with the same text; nothing new was mailed.", isError: false, replace: false, clearDraft: true);
                break;
            case "conflict":
                card.Finish(
                    "Already answered with a different answer; yours was not applied. Replace sends yours in its place.",
                    isError: true, replace: true, clearDraft: false);
                break;
            case "closed":
                card.Finish($"{card.Id} is closed; the answer was not stored.", isError: true, replace: false, clearDraft: false);
                break;
            case "not-found":
                card.Finish($"{card.Id} no longer exists; the answer was not stored.", isError: true, replace: false, clearDraft: false);
                break;
            default:
                card.Finish("The hub sent a reply this version does not understand; check this list.", isError: true, replace: false, clearDraft: false);
                break;
        }
    }

    private static List<string> NoticesOf(MailSummary mail)
    {
        var inv = CultureInfo.InvariantCulture;
        var list = mail.WaitingRoles
            .Select(r => $"Mail waits for role {r.Role} ({r.Queued.ToString(inv)} queued): start a session named {r.Role}.")
            .ToList();
        if (mail.Flagged > 0)
        {
            list.Add(mail.Flagged == 1
                ? "1 delivered mail has no ack after the timeout; its sender was told."
                : $"{mail.Flagged.ToString(inv)} delivered mails have no ack after the timeout; their senders were told.");
        }

        if (mail.OwnerOpen > 0)
        {
            list.Add($"{mail.OwnerOpen.ToString(inv)} open mail for you: run hub mail inbox outside Claude Code.");
        }

        return list;
    }

    private void SyncNotices(List<string> next)
    {
        if (Notices.SequenceEqual(next, StringComparer.Ordinal)) return;
        Notices.Clear();
        foreach (var n in next) Notices.Add(n);
    }
}
