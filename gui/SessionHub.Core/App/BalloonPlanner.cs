using SessionHub.Core.Protocol;
using SessionHub.Core.Tray;

namespace SessionHub.Core.App;

/// <summary>A balloon to show: the session it opens (null: the panel's home), its title and one line of detail.</summary>
public sealed record BalloonNote(string? SessionId, string Title, string Text);

/// <summary>
/// Compares each snapshot with the previous one and asks <see cref="BalloonGate"/> about every
/// live session whose waiting items or approval rose. The first snapshot only sets the baseline, so
/// starting the tray never shows a burst of balloons. Windows shows one balloon at a time, so one
/// snapshot gives at most one: several sessions that rose together share one combined balloon (and
/// the gate counts it as shown for each of them). Title and text are never empty.
/// </summary>
public sealed class BalloonPlanner
{
    /// <summary>The line shown when a waiting item has no text.</summary>
    public const string DefaultText = "Open Session Hub to see it.";

    private Dictionary<string, (int Items, bool Approval)>? last;
    private HubSnapshot? lastSnapshot;

    /// <summary>The balloon for this snapshot: none or one.</summary>
    /// <param name="isDismissed">
    /// Whether the owner dismissed the item with this draft label in that session (session id, label). A
    /// dismissed item that is not answered is taken off the session's count, and never named in a balloon.
    /// </param>
    public IReadOnlyList<BalloonNote> Plan(HubSnapshot snapshot, BalloonGate gate, DateTimeOffset now, Func<string, string, bool>? isDismissed = null)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        ArgumentNullException.ThrowIfNull(gate);
        var current = new Dictionary<string, (int Items, bool Approval)>();
        var notes = new List<BalloonNote>();
        var risen = new List<SessionView>();
        foreach (var s in snapshot.Sessions)
        {
            var live = s.EndedAt is null;
            var state = StateOf(s, isDismissed, out var open);
            if (!current.TryAdd(s.SessionId, state) || last is null || !live) continue;
            var before = last.TryGetValue(s.SessionId, out var b) ? b : (Items: 0, Approval: false);
            if (gate.ShouldShow(s.SessionId, before.Items, state.Items, before.Approval, state.Approval, now))
            {
                notes.Add(Note(s, open, state.Items, state.Approval && !before.Approval && state.Items <= before.Items));
                risen.Add(s);
            }
        }

        last = current;
        lastSnapshot = snapshot;
        return notes.Count <= 1 ? notes : [Combined(risen)];
    }

    /// <summary>
    /// After the owner dismissed or restored an item: takes the last snapshot's counts as the new
    /// baseline without showing anything, so restoring an item never pops a balloon for it.
    /// </summary>
    public void Rebase(Func<string, string, bool>? isDismissed)
    {
        if (last is null || lastSnapshot is null) return;
        var current = new Dictionary<string, (int Items, bool Approval)>();
        foreach (var s in lastSnapshot.Sessions) current.TryAdd(s.SessionId, StateOf(s, isDismissed, out _));
        last = current;
    }

    private static (int Items, bool Approval) StateOf(SessionView s, Func<string, string, bool>? isDismissed, out List<WaitingItem> open)
    {
        var live = s.EndedAt is null;
        open = OpenItems(s, isDismissed, out var dismissedOpen);
        return (Items: live ? Math.Max(0, s.WaitingCount - dismissedOpen) : 0, Approval: live && s.State == "waiting");
    }

    /// <summary>The items neither answered nor dismissed, and how many unanswered ones are dismissed.</summary>
    private static List<WaitingItem> OpenItems(SessionView s, Func<string, string, bool>? isDismissed, out int dismissedOpen)
    {
        dismissedOpen = 0;
        var waiting = s.Status?.Waiting ?? [];
        var labels = isDismissed is null || waiting.Count == 0 ? null : AnswerComposer.DraftLabels(waiting);
        var open = new List<WaitingItem>(waiting.Count);
        for (var i = 0; i < waiting.Count; i++)
        {
            if (waiting[i].Answered) continue;
            if (labels is not null && isDismissed!(s.SessionId, labels[i])) dismissedOpen++;
            else open.Add(waiting[i]);
        }

        return open;
    }

    private static BalloonNote Note(SessionView s, List<WaitingItem> open, int items, bool approvalOnly)
    {
        if (approvalOnly)
            return new BalloonNote(s.SessionId, $"{NameOf(s)} needs approval", "A permission prompt is open in its terminal.");
        var newest = open.Count > 0 ? open[^1] : null;
        var text = newest is null || string.IsNullOrWhiteSpace(newest.Text)
            ? DefaultText
            : newest.Id is null ? newest.Text : $"{newest.Id}: {newest.Text}";
        return new BalloonNote(s.SessionId, $"{NameOf(s)} is waiting on you ({items})", text);
    }

    /// <summary>One balloon for several sessions; a click opens the panel's home, which lists them all.</summary>
    private static BalloonNote Combined(List<SessionView> sessions)
    {
        var names = sessions.Select(NameOf).ToList();
        var text = names.Count <= 3
            ? string.Join(", ", names)
            : $"{string.Join(", ", names.Take(3))} and {names.Count - 3} more";
        return new BalloonNote(null, $"{sessions.Count} sessions are waiting on you", text);
    }

    private static string NameOf(SessionView s) => string.IsNullOrWhiteSpace(s.Name) ? "A session" : s.Name;
}
