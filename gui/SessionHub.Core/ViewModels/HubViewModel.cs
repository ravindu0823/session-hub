using System.Collections.ObjectModel;
using SessionHub.Core.App;
using SessionHub.Core.Protocol;
using SessionHub.Core.Settings;

namespace SessionHub.Core.ViewModels;

public enum PanelFilter
{
    All,
    Waiting,
    Working,
    Idle,
    Ended,
}

/// <summary>
/// The panel's data. <see cref="Apply"/> merges a snapshot into <see cref="Sessions"/> with
/// Insert, Move and RemoveAt only, so existing rows keep their instance (bindings, focus, drafts).
/// Call it on the UI thread.
/// </summary>
public sealed class HubViewModel : ObservableObject
{
    private Meta meta = new(0, null, [], "stopped");
    private Totals totals = new(0, 0, 0);

    /// <summary>A draft is dropped when its session has not been in a snapshot for this long.</summary>
    public static readonly TimeSpan DraftRetention = TimeSpan.FromDays(7);

    /// <summary>How often refreshed last-seen times are worth a save when no draft was edited.</summary>
    public static readonly TimeSpan SeenSaveInterval = TimeSpan.FromDays(1);

    private readonly Dictionary<string, string> drafts;
    private readonly Dictionary<string, DateTimeOffset> seen = [];

    // Dismissed items as AnswerComposer.DismissKey values, oldest first.
    private readonly List<string> dismissed = [];
    private readonly TimeProvider time;
    private DateTimeOffset? lastSeenSave;
    private bool seenSaveDue;
    private bool dismissedSaveDue;
    private bool applying;

    /// <summary>
    /// Saved drafts (by session id, and answers by <c>&lt;session id&gt;#&lt;label&gt;</c>) seed new rows, so a
    /// row that comes back gets its text back. <paramref name="savedSeen"/> holds when each draft's session
    /// was last seen; a draft without an entry (a gui.json from before that field), or with a time in the
    /// future, counts as seen now. <paramref name="savedDismissed"/> holds dismissed items
    /// (<see cref="AnswerComposer.DismissKey"/>); one applies to its row while the row's status reply is the
    /// one in the key.
    /// </summary>
    public HubViewModel(
        IReadOnlyDictionary<string, string>? savedDrafts = null,
        IReadOnlyDictionary<string, DateTimeOffset>? savedSeen = null,
        TimeProvider? time = null,
        IEnumerable<string>? savedDismissed = null)
    {
        this.time = time ?? TimeProvider.System;
        drafts = savedDrafts is null ? [] : new Dictionary<string, string>(savedDrafts);
        var now = this.time.GetUtcNow();
        foreach (var id in drafts.Keys)
        {
            seen[id] = savedSeen is not null && savedSeen.TryGetValue(id, out var at) && at < now ? at : now;
        }

        foreach (var key in savedDismissed ?? [])
        {
            if (AnswerComposer.TryParseDismissKey(key, out _, out _, out _) && !dismissed.Contains(key)) dismissed.Add(key);
        }

        TrimDismissed();
    }

    /// <summary>
    /// Raised when the owner dismisses an item or undoes it (not for the pruning a snapshot does). The app
    /// saves and the views recount then.
    /// </summary>
    public event Action? DismissedChanged;

    /// <summary>A copy of the dismissed items (<see cref="AnswerComposer.DismissKey"/>), oldest first.</summary>
    public IReadOnlyList<string> Dismissed => [.. dismissed];

    /// <summary>
    /// <see cref="Totals"/> with the dismissed items that are not answered taken off the waiting items:
    /// the number the icon, the badges and the headers show.
    /// </summary>
    public Totals EffectiveTotals
    {
        get
        {
            var off = Sessions.Where(s => s.IsLive).Sum(s => s.WaitingCount - s.EffectiveWaitingCount);
            return off == 0 ? totals : totals with { WaitingItems = Math.Max(0, totals.WaitingItems - off) };
        }
    }

    /// <summary>The item with this draft label is dismissed in that session (for its current status reply).</summary>
    public bool IsDismissed(string sessionId, string label) =>
        Sessions.FirstOrDefault(s => s.SessionId == sessionId)?.IsDismissed(label) == true;

    /// <summary>True, once, after a snapshot dropped dismissals (a new status reply, or the session gone).</summary>
    public bool TakeDismissedSaveDue()
    {
        var due = dismissedSaveDue;
        dismissedSaveDue = false;
        return due;
    }

    /// <summary>
    /// Raised with the draft key and new text when a row's draft (key: session id) or answer (key:
    /// <c>&lt;session id&gt;#&lt;label&gt;</c>) changes; empty text: cleared. Also raised with empty text
    /// when a draft expires. The app saves then.
    /// </summary>
    public event Action<string, string>? DraftChanged;

    public ObservableCollection<SessionViewModel> Sessions { get; } = [];

    /// <summary>
    /// A copy of every known draft, including those of sessions not in the latest snapshot, until they
    /// have been missing for <see cref="DraftRetention"/>.
    /// </summary>
    public IReadOnlyDictionary<string, string> Drafts => new Dictionary<string, string>(drafts);

    /// <summary>A copy of when each draft's session was last seen in a snapshot, or the draft last edited.</summary>
    public IReadOnlyDictionary<string, DateTimeOffset> DraftsSeen => new Dictionary<string, DateTimeOffset>(seen);

    public Meta Meta
    {
        get => meta;
        private set => SetField(ref meta, value);
    }

    /// <summary>False until the first <see cref="Apply"/>: <see cref="Meta"/> is a placeholder until then.</summary>
    public bool HasSnapshot { get; private set; }

    public Totals Totals
    {
        get => totals;
        private set => SetField(ref totals, value);
    }

    public void Apply(HubSnapshot snapshot)
    {
        ArgumentNullException.ThrowIfNull(snapshot);

        HasSnapshot = true;
        applying = true;
        try
        {
            ApplyCore(snapshot);
        }
        finally
        {
            applying = false;
        }
    }

    private void ApplyCore(HubSnapshot snapshot)
    {

        // Meta and totals first, so a collection handler never sees new rows with old totals.
        if (!MetaEquals(meta, snapshot.Meta)) Meta = snapshot.Meta;
        Totals = snapshot.Totals;

        var wanted = new Dictionary<string, SessionView>();
        var order = new List<string>();
        foreach (var s in snapshot.Sessions)
        {
            if (wanted.TryAdd(s.SessionId, s)) order.Add(s.SessionId);
        }

        var expired = ExpireDrafts(snapshot, wanted);

        for (var i = Sessions.Count - 1; i >= 0; i--)
        {
            if (wanted.ContainsKey(Sessions[i].SessionId)) continue;
            Sessions[i].PropertyChanged -= OnRowChanged;
            Sessions.RemoveAt(i);
        }

        for (var i = 0; i < order.Count; i++)
        {
            var view = wanted[order[i]];
            if (i < Sessions.Count && Sessions[i].SessionId == view.SessionId)
            {
                Sessions[i].Update(view);
                continue;
            }

            var from = -1;
            for (var j = i + 1; j < Sessions.Count; j++)
            {
                if (Sessions[j].SessionId != view.SessionId) continue;
                from = j;
                break;
            }

            if (from >= 0)
            {
                Sessions.Move(from, i);
                Sessions[i].Update(view);
            }
            else
            {
                var row = new SessionViewModel(view);
                if (drafts.TryGetValue(view.SessionId, out var saved)) row.Draft = saved;
                var prefix = AnswerComposer.DraftKey(view.SessionId, "");
                foreach (var (key, text) in drafts)
                {
                    if (key.StartsWith(prefix, StringComparison.Ordinal)) row.SetAnswer(key[prefix.Length..], text);
                }

                if (view.Status?.ReplyUuid is { } reply)
                {
                    foreach (var key in dismissed)
                    {
                        if (AnswerComposer.TryParseDismissKey(key, out var sid, out var label, out var uuid) && sid == view.SessionId && uuid == reply)
                            row.SetDismissed(label, true);
                    }
                }

                row.PropertyChanged += OnRowChanged;
                row.AnswerChanged += (label, text) => OnAnswerChanged(row, label, text);
                row.DismissedChanged += () => OnDismissedChanged(row);
                Sessions.Insert(i, row);
            }
        }

        PruneDismissed(snapshot, wanted);
        foreach (var id in expired) DraftChanged?.Invoke(id, "");
    }

    /// <summary>
    /// Drops dismissals whose session moved on to another status reply, and, on a trusted snapshot (as
    /// for drafts), those whose session is gone.
    /// </summary>
    private void PruneDismissed(HubSnapshot snapshot, Dictionary<string, SessionView> wanted)
    {
        var trusted = snapshot.Meta.Daemon == "running" && wanted.Count > 0;
        var removed = dismissed.RemoveAll(key =>
        {
            if (!AnswerComposer.TryParseDismissKey(key, out var sid, out _, out var uuid)) return true;
            if (wanted.TryGetValue(sid, out var view)) return view.Status?.ReplyUuid != uuid;
            return trusted;
        });
        if (removed > 0) dismissedSaveDue = true;
    }

    /// <summary>Mirrors a row's dismissed labels into the saved keys.</summary>
    private void OnDismissedChanged(SessionViewModel row)
    {
        var prefix = AnswerComposer.DraftKey(row.SessionId, "");
        dismissed.RemoveAll(k => k.StartsWith(prefix, StringComparison.Ordinal));
        if (row.ReplyUuid is { } reply)
        {
            foreach (var label in row.DismissedLabels) dismissed.Add(AnswerComposer.DismissKey(row.SessionId, label, reply));
        }

        TrimDismissed();
        if (applying)
        {
            dismissedSaveDue = true;
            return;
        }

        DismissedChanged?.Invoke();
    }

    /// <summary>Keeps the newest <see cref="GuiSettings.MaxDismissed"/>.</summary>
    private void TrimDismissed()
    {
        if (dismissed.Count > GuiSettings.MaxDismissed) dismissed.RemoveRange(0, dismissed.Count - GuiSettings.MaxDismissed);
    }

    /// <summary>
    /// True, once, when the last-seen times have moved on enough to be worth saving although no draft
    /// changed: at the first trusted snapshot after start, then at most once per
    /// <see cref="SeenSaveInterval"/>, and only while there are drafts. The app then saves
    /// <see cref="Drafts"/> and <see cref="DraftsSeen"/>, so a tray that never quits cleanly does not
    /// leave old times behind that would expire a draft early after the next start.
    /// </summary>
    public bool TakeSeenSaveDue()
    {
        var due = seenSaveDue;
        seenSaveDue = false;
        return due;
    }

    /// <summary>
    /// Marks the drafts of sessions in the snapshot as seen now and drops those whose session has been
    /// missing for <see cref="DraftRetention"/>. Only a trusted snapshot expires anything: the daemon
    /// is running and the snapshot lists at least one session. A stopped daemon's list may be
    /// incomplete, and an empty list is what the bridge sends when the index cannot be opened or read,
    /// so neither says a session is gone. Returns the dropped session ids.
    /// </summary>
    private List<string> ExpireDrafts(HubSnapshot snapshot, Dictionary<string, SessionView> wanted)
    {
        var now = time.GetUtcNow();
        var expired = new List<string>();
        var trusted = snapshot.Meta.Daemon == "running" && wanted.Count > 0;
        foreach (var id in drafts.Keys.ToList())
        {
            if (wanted.ContainsKey(SessionOf(id))) seen[id] = now;
            else if (trusted && now - seen[id] >= DraftRetention) expired.Add(id);
        }

        if (trusted && (lastSeenSave is null || now - lastSeenSave.Value >= SeenSaveInterval))
        {
            lastSeenSave = now;
            if (drafts.Count > expired.Count) seenSaveDue = true;
        }

        foreach (var id in expired)
        {
            drafts.Remove(id);
            seen.Remove(id);
        }

        return expired;
    }

    private void OnRowChanged(object? sender, System.ComponentModel.PropertyChangedEventArgs e)
    {
        if (e.PropertyName != nameof(SessionViewModel.Draft) || sender is not SessionViewModel row) return;
        if (row.Draft.Length == 0)
        {
            drafts.Remove(row.SessionId);
            seen.Remove(row.SessionId);
        }
        else
        {
            drafts[row.SessionId] = row.Draft;
            seen[row.SessionId] = time.GetUtcNow();
        }

        DraftChanged?.Invoke(row.SessionId, row.Draft);
    }

    private void OnAnswerChanged(SessionViewModel row, string label, string text)
    {
        var key = AnswerComposer.DraftKey(row.SessionId, label);
        if (text.Length == 0)
        {
            drafts.Remove(key);
            seen.Remove(key);
        }
        else
        {
            drafts[key] = text;
            seen[key] = time.GetUtcNow();
        }

        DraftChanged?.Invoke(key, text);
    }

    /// <summary>The session a draft key belongs to: the key itself, or the part before "#" of an answer key.</summary>
    private static string SessionOf(string key)
    {
        var i = key.IndexOf('#', StringComparison.Ordinal);
        return i < 0 ? key : key[..i];
    }

    /// <summary>
    /// Sessions for a panel tab. <see cref="PanelFilter.All"/> leaves out ended ones. The query matches
    /// name, cwd, waiting item ids and texts, ignoring case.
    /// </summary>
    public IReadOnlyList<SessionViewModel> Filter(PanelFilter filter, string? query)
    {
        var q = query?.Trim() ?? "";
        return [.. Sessions.Where(s => InGroup(s, filter) && (q.Length == 0 || Matches(s, q)))];
    }

    private static bool InGroup(SessionViewModel s, PanelFilter f) => f switch
    {
        PanelFilter.All => s.Group != SessionGroup.Ended,
        PanelFilter.Waiting => s.Group == SessionGroup.Waiting,
        PanelFilter.Working => s.Group == SessionGroup.Working,
        PanelFilter.Idle => s.Group == SessionGroup.Idle,
        PanelFilter.Ended => s.Group == SessionGroup.Ended,
        _ => false,
    };

    private static bool Matches(SessionViewModel s, string q) =>
        Has(s.Name, q) || Has(s.Cwd, q) || s.Waiting.Any(w => Has(w.Id, q) || Has(w.Text, q));

    private static bool Has(string? text, string q) => text?.Contains(q, StringComparison.OrdinalIgnoreCase) == true;

    private static bool MetaEquals(Meta a, Meta b) =>
        a.Daemon == b.Daemon && a.UnknownCount == b.UnknownCount && a.UnknownVersion == b.UnknownVersion
        && a.RelayStrayCount == b.RelayStrayCount && a.Stale.SequenceEqual(b.Stale);
}
