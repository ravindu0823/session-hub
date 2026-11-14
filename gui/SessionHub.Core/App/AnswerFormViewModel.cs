using System.Collections.ObjectModel;
using System.ComponentModel;
using System.Globalization;
using SessionHub.Core.Protocol;
using SessionHub.Core.ViewModels;

namespace SessionHub.Core.App;

/// <summary>
/// What one send took: the text, and the boxes as they were, so only those are cleared after the
/// bridge accepts it. <paramref name="Answers"/> is empty for a plain reply (no waiting items).
/// <paramref name="Message"/> is null when the send did not take the message box (one item's own Send).
/// </summary>
public sealed record SendPayload(string Text, string? Message, IReadOnlyDictionary<string, string> Answers, bool FromForm);

/// <summary>One waiting item of the answer form, with its answer box.</summary>
public sealed class AnswerItemViewModel : ObservableObject
{
    public const string AnsweredTag = "Answered";
    public const string DismissedTag = "Dismissed";

    public const string AnsweredToolTip =
        "You answered this in the session's terminal or through the hub after this list was posted.";

    public const string DismissedToolTip =
        "You dismissed this item here. It no longer counts as waiting on you; Undo brings it back.";

    private readonly SessionViewModel session;
    private bool isAnswered;

    internal AnswerItemViewModel(SessionViewModel session, WaitingItem item, string label, string key)
    {
        this.session = session;
        Label = label;
        Key = key;
        Id = string.IsNullOrWhiteSpace(item.Id) ? null : item.Id;
        IsNoId = Id is null;
        Text = item.Text;
        Children = item.Children;
        isAnswered = item.Answered;
    }

    /// <summary>The session this item waits in (the per-item Send finds its card by it).</summary>
    public string SessionId => session.SessionId;

    /// <summary>The item's ID, or "Item n" (shown muted) when it has none.</summary>
    public string Label { get; }

    /// <summary>What the answer draft is kept under (<see cref="AnswerComposer.DraftLabels"/>).</summary>
    public string Key { get; }

    /// <summary>The item's own ID; null when it has none.</summary>
    public string? Id { get; }

    public bool IsNoId { get; }

    public string Text { get; }

    public IReadOnlyList<string> Children { get; }

    public bool HasChildren => Children.Count > 0;

    public string Placeholder => $"Your answer to {Label}… (optional)";

    /// <summary>The box text; stored on the session as a draft under <see cref="Key"/>.</summary>
    public string Answer
    {
        get => session.GetAnswer(Key);
        set => session.SetAnswer(Key, value);
    }

    /// <summary>The box has text: the card gets the amber edge.</summary>
    public bool IsFilled => !string.IsNullOrWhiteSpace(Answer);

    /// <summary>The daemon saw an answer naming this item after the list was posted.</summary>
    public bool IsAnswered => isAnswered;

    /// <summary>The owner dismissed this item in the tray (for the current status reply).</summary>
    public bool IsDismissed => session.IsDismissed(Key);

    /// <summary>Answered or dismissed: shown dimmed with a tag, and left out of every count.</summary>
    public bool IsSettled => isAnswered || IsDismissed;

    /// <summary>"Answered", "Dismissed" or empty.</summary>
    public string TagText => isAnswered ? AnsweredTag : IsDismissed ? DismissedTag : "";

    public string TagToolTip => isAnswered ? AnsweredToolTip : IsDismissed ? DismissedToolTip : "";

    /// <summary>The "Dismiss" link shows on items that are still open.</summary>
    public bool CanDismiss => !IsSettled;

    /// <summary>The "Undo" link shows on a dismissed item the daemon has not seen answered.</summary>
    public bool CanUndo => IsDismissed && !isAnswered;

    /// <summary>What this item's own Send carries: <c>Answers:</c> and its line (empty while the box is blank).</summary>
    public string SendText => AnswerComposer.ComposeOne(Label, Answer);

    /// <summary>The item's own Send: a filled box, a live session, within the relay's limit.</summary>
    public bool CanSend => session.IsLive && IsFilled && SendText.Length <= AnswerComposer.MaxChars;

    public void Dismiss() => session.SetDismissed(Key, true);

    public void Undo() => session.SetDismissed(Key, false);

    internal void Changed()
    {
        Raise(nameof(Answer));
        Raise(nameof(IsFilled));
        Raise(nameof(SendText));
        Raise(nameof(CanSend));
    }

    internal void SetAnswered(bool value)
    {
        if (!SetField(ref isAnswered, value, nameof(IsAnswered))) return;
        SettledChanged();
    }

    internal void SettledChanged()
    {
        Raise(nameof(IsDismissed));
        Raise(nameof(IsSettled));
        Raise(nameof(TagText));
        Raise(nameof(TagToolTip));
        Raise(nameof(CanDismiss));
        Raise(nameof(CanUndo));
    }

    internal void LiveChanged() => Raise(nameof(CanSend));
}

/// <summary>
/// The Send views' answer form for a live session with "Waiting on you" items: one answer box per
/// item plus the free message (the session's <see cref="SessionViewModel.Draft"/>), composed into
/// one message by <see cref="AnswerComposer"/>. Without waiting items the plain reply box is used
/// and <see cref="Capture"/> sends the draft as typed.
/// </summary>
public sealed class AnswerFormViewModel : ObservableObject
{
    private readonly SessionViewModel session;
    private IReadOnlyList<string> keys = [];
    private bool isActive;
    private int total;
    private int answeredCount;
    private string composed = "";
    private string footerText = "";
    private string sendLabel = "";
    private bool isTooLong;
    private string tooLongText = "";

    public AnswerFormViewModel(SessionViewModel session)
    {
        ArgumentNullException.ThrowIfNull(session);
        this.session = session;
        session.PropertyChanged += OnSessionChanged;
        session.AnswerChanged += OnAnswerChanged;
        session.DismissedChanged += OnDismissedChanged;
        Rebuild();
    }

    public ObservableCollection<AnswerItemViewModel> Items { get; } = [];

    /// <summary>The session is live and has waiting items: the form replaces the plain reply box.</summary>
    public bool IsActive
    {
        get => isActive;
        private set => SetField(ref isActive, value);
    }

    /// <summary>Items still open: answered and dismissed ones are left out.</summary>
    public int Total
    {
        get => total;
        private set => SetField(ref total, value);
    }

    /// <summary>Open items whose box is filled.</summary>
    public int AnsweredCount
    {
        get => answeredCount;
        private set => SetField(ref answeredCount, value);
    }

    /// <summary>"1 of 3 answered + message · Ctrl+Enter sends" (open items only).</summary>
    public string FooterText
    {
        get => footerText;
        private set => SetField(ref footerText, value);
    }

    /// <summary>"Send answers" when any answer is filled, else "Send message".</summary>
    public string SendLabel
    {
        get => sendLabel;
        private set => SetField(ref sendLabel, value);
    }

    /// <summary>The text the send would carry (empty: nothing to send).</summary>
    public string Composed
    {
        get => composed;
        private set
        {
            if (!SetField(ref composed, value)) return;
            Raise(nameof(PreviewText));
            Raise(nameof(HasPreview));
            Raise(nameof(CanSend));
        }
    }

    /// <summary>What the session will receive: the relay's header and the composed text.</summary>
    public string PreviewText => composed.Length == 0 ? "" : AnswerComposer.Preview(composed);

    public bool HasPreview => composed.Length > 0;

    public bool IsTooLong
    {
        get => isTooLong;
        private set
        {
            if (SetField(ref isTooLong, value)) Raise(nameof(CanSend));
        }
    }

    /// <summary>"Too long to send: 20,431 of 20,000 characters. Shorten it first."</summary>
    public string TooLongText
    {
        get => tooLongText;
        private set => SetField(ref tooLongText, value);
    }

    /// <summary>Something to send, within the limit.</summary>
    public bool CanSend => composed.Length > 0 && !isTooLong;

    /// <summary>The text to send now and the boxes it came from.</summary>
    public SendPayload Capture()
    {
        if (!IsActive) return new SendPayload(session.Draft, session.Draft, new Dictionary<string, string>(), FromForm: false);
        var answers = new Dictionary<string, string>(StringComparer.Ordinal);
        foreach (var item in Items)
        {
            if (item.IsFilled) answers[item.Key] = item.Answer;
        }

        return new SendPayload(Compose(), session.Draft, answers, FromForm: true);
    }

    /// <summary>
    /// The text one item's own Send carries and its box as it is now; the message box is not taken. Empty
    /// text when the item is gone or its box is blank.
    /// </summary>
    public SendPayload CaptureOne(string key)
    {
        ArgumentNullException.ThrowIfNull(key);
        var item = Items.FirstOrDefault(i => i.Key == key);
        if (item is null || !item.IsFilled) return new SendPayload("", null, new Dictionary<string, string>(), FromForm: false);
        var answers = new Dictionary<string, string>(StringComparer.Ordinal) { [key] = item.Answer };
        return new SendPayload(item.SendText, null, answers, FromForm: false);
    }

    /// <summary>
    /// After the bridge accepted <paramref name="sent"/>: clears the boxes that still hold what was
    /// sent (an edit made meanwhile stays), and drops answer drafts whose item is no longer waiting.
    /// </summary>
    public void ClearSent(SendPayload sent)
    {
        ArgumentNullException.ThrowIfNull(sent);
        if (sent.Message is not null && session.Draft == sent.Message) session.Draft = "";
        foreach (var (key, text) in sent.Answers)
        {
            if (session.GetAnswer(key) == text) session.SetAnswer(key, "");
        }

        if (!sent.FromForm) return;
        foreach (var key in session.Answers.Keys.Where(k => !keys.Contains(k)).ToList()) session.SetAnswer(key, "");
    }

    /// <summary>The Clear button: every answer box and the message.</summary>
    public void Clear()
    {
        foreach (var item in Items) item.Answer = "";
        session.Draft = "";
    }

    private string Compose() =>
        AnswerComposer.Compose(session.Waiting, [.. keys.Select(session.GetAnswer)], session.Draft);

    private void OnSessionChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(SessionViewModel.Waiting):
                Rebuild();
                break;
            case nameof(SessionViewModel.IsLive):
                Rebuild();
                foreach (var item in Items) item.LiveChanged();
                break;
            case nameof(SessionViewModel.Draft):
                Recompute();
                break;
        }
    }

    private void OnAnswerChanged(string key, string text)
    {
        foreach (var item in Items)
        {
            if (item.Key == key) item.Changed();
        }

        Recompute();
    }

    private void OnDismissedChanged()
    {
        foreach (var item in Items) item.SettledChanged();
        Recompute();
    }

    /// <summary>
    /// Follows the waiting list. An item whose label, key, text and sub-items are unchanged keeps its view
    /// model (and so its box, focus and caret); only changed positions are replaced.
    /// </summary>
    private void Rebuild()
    {
        var waiting = session.Waiting;
        var labels = AnswerComposer.Labels(waiting);
        keys = AnswerComposer.DraftLabels(waiting);
        var reusable = Items.ToList();
        for (var i = 0; i < waiting.Count; i++)
        {
            var item = waiting[i];
            var kept = reusable.FirstOrDefault(v =>
                v.Key == keys[i] && v.Label == labels[i] && v.Text == item.Text && v.Children.SequenceEqual(item.Children));
            if (kept is not null) reusable.Remove(kept);
            if (kept is null)
            {
                Items.Insert(i, new AnswerItemViewModel(session, item, labels[i], keys[i]));
                continue;
            }

            kept.SetAnswered(item.Answered);
            // Dismissals are per status reply: a kept item may have lost its dismissal to a new reply.
            kept.SettledChanged();

            // A kept item is never before i (positions before i are final), so it moves up or stays.
            var at = Items.IndexOf(kept);
            if (at != i) Items.Move(at, i);
        }

        // Positions 0..n-1 now hold the new list; what was pushed past them is gone.
        while (Items.Count > waiting.Count) Items.RemoveAt(Items.Count - 1);
        IsActive = session.IsLive && waiting.Count > 0;
        Recompute();
    }

    private void Recompute()
    {
        Total = Items.Count(i => !i.IsSettled);
        AnsweredCount = Items.Count(i => i.IsFilled && !i.IsSettled);
        var hasMessage = !string.IsNullOrWhiteSpace(session.Draft);
        var counted = Total > 0 || Items.Count == 0
            ? $"{AnsweredCount.ToString(CultureInfo.InvariantCulture)} of {Total.ToString(CultureInfo.InvariantCulture)} answered"
            : "All items answered or dismissed";
        FooterText = $"{counted}{(hasMessage ? " + message" : "")} · Ctrl+Enter sends";
        // Settled items with a filled box are still sent, so they count for the label.
        SendLabel = Items.Any(i => i.IsFilled) ? "Send answers" : "Send message";
        var text = Compose();
        IsTooLong = text.Length > AnswerComposer.MaxChars;
        TooLongText = IsTooLong
            ? $"Too long to send: {text.Length.ToString("N0", CultureInfo.InvariantCulture)} of {AnswerComposer.MaxChars.ToString("N0", CultureInfo.InvariantCulture)} characters. Shorten it first."
            : "";
        Composed = text;
    }
}
