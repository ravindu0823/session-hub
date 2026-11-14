using SessionHub.Core.Bridge;
using SessionHub.Core.Protocol;
using SessionHub.Core.ViewModels;

namespace SessionHub.Core.Tray;

public enum IconKind
{
    Running,
    Waiting,
    Stopped,
    Error,

    /// <summary>No snapshot yet: the app tile (as <see cref="Running"/>), with a "starting" tooltip.</summary>
    Starting,
}

/// <summary>What the tray icon shows. <see cref="Count"/> is the badge number for <see cref="IconKind.Waiting"/>.</summary>
public sealed record IconState(IconKind Kind, int Count)
{
    /// <summary>Windows tray tooltips hold at most 127 characters.</summary>
    public const int MaxTooltipLength = 127;

    /// <param name="totals">The effective totals (<see cref="HubViewModel.EffectiveTotals"/>): dismissed items left out.</param>
    /// <param name="hasSnapshot">
    /// False until the first snapshot arrives: the meta is then a placeholder, not "daemon stopped".
    /// </param>
    public static IconState From(Meta meta, Totals totals, BridgeHealth health, bool hasSnapshot = true)
    {
        ArgumentNullException.ThrowIfNull(meta);
        ArgumentNullException.ThrowIfNull(totals);
        if (health is BridgeHealth.Failed) return new IconState(IconKind.Error, 0);
        if (!hasSnapshot) return new IconState(IconKind.Starting, 0);
        if (meta.Daemon == "stopped") return new IconState(IconKind.Stopped, 0);
        var count = totals.WaitingItems + totals.NeedsApproval;
        return count > 0 ? new IconState(IconKind.Waiting, count) : new IconState(IconKind.Running, 0);
    }

    /// <summary>The tooltip text, not yet cut to <see cref="MaxTooltipLength"/>: pass it to <see cref="Truncate"/>.</summary>
    public static string Tooltip(Meta meta, Totals totals, BridgeHealth health, IReadOnlyList<SessionViewModel> sessions, bool hasSnapshot = true)
    {
        var state = From(meta, totals, health, hasSnapshot);
        switch (state.Kind)
        {
            case IconKind.Error:
                return "Session Hub: reconnecting to hub.exe";
            case IconKind.Starting:
                return "Session Hub: starting";
            case IconKind.Stopped:
                return "Session Hub: daemon stopped";
            case IconKind.Waiting:
                var names = sessions.Where(s => s.IsLive && s.Group == SessionGroup.Waiting).Select(Describe).ToList();
                return names.Count > 0
                    ? $"Session Hub: {string.Join(", ", names)} waiting on you"
                    : $"Session Hub: {state.Count} waiting on you";
            default:
                return $"Session Hub: {totals.Live} live {(totals.Live == 1 ? "session" : "sessions")}, nothing waiting";
        }
    }

    /// <summary>Cuts <paramref name="text"/> to <paramref name="max"/> characters, ending in an ellipsis, without splitting a surrogate pair.</summary>
    public static string Truncate(string text, int max = MaxTooltipLength)
    {
        ArgumentNullException.ThrowIfNull(text);
        if (text.Length <= max) return text;
        var keep = max - 1;
        if (keep > 0 && char.IsHighSurrogate(text[keep - 1])) keep--;
        return text[..keep] + "…";
    }

    private static string Describe(SessionViewModel s)
    {
        var n = s.EffectiveWaitingCount;
        if (n > 0 && s.NeedsApproval) return $"{s.Name} ({n}, approval)";
        return n > 0 ? $"{s.Name} ({n})" : $"{s.Name} (approval)";
    }
}
