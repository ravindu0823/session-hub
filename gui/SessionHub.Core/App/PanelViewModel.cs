using System.Collections.ObjectModel;
using SessionHub.Core.Bridge;
using SessionHub.Core.Protocol;
using SessionHub.Core.ViewModels;

namespace SessionHub.Core.App;

public enum PanelTab
{
    Home,
    Sessions,
    Waiting,
    Send,
}

/// <summary>What the header pill says about the link to the hub.</summary>
public enum HubLink
{
    /// <summary>No snapshot yet.</summary>
    Connecting,
    Live,
    DaemonStopped,
    Reconnecting,
}

/// <summary>
/// Everything the panel shows, without WPF. Snapshots merge into the same card instances; the shown
/// list keeps its order between snapshots (see <see cref="StableList"/>) and is put in snapshot order
/// again only by <see cref="Refresh"/> with reorder (panel opened, tab or filter changed).
/// Call on the UI thread.
/// </summary>
public sealed class PanelViewModel : ObservableObject
{
    private readonly Dictionary<SessionViewModel, CardViewModel> cards = new(ReferenceEqualityComparer.Instance);
    private PanelTab tab = PanelTab.Sessions;
    private PanelFilter filter = PanelFilter.All;
    private string query = "";
    private BridgeHealth? health;
    private bool hasSnapshot;
    private DateTimeOffset now = DateTimeOffset.UnixEpoch;
    private CardViewModel? sendTarget;
    private string summary = "";
    private bool isEmpty;
    private string footerText = "";
    private string footerWarning = "";

    public PanelViewModel(HubViewModel hub)
    {
        ArgumentNullException.ThrowIfNull(hub);
        Hub = hub;
        hub.DismissedChanged += OnDismissedChanged;
    }

    /// <summary>Raised when an open card needs its last exchange (opened, or a new reply arrived).</summary>
    public event Action<CardViewModel>? ExchangeWanted;

    public HubViewModel Hub { get; }

    /// <summary>The cards of the current tab (unused on <see cref="PanelTab.Send"/>).</summary>
    public ObservableCollection<CardViewModel> Items { get; } = [];

    /// <summary>Live sessions for the Send tab's picker.</summary>
    public ObservableCollection<CardViewModel> SendTargets { get; } = [];

    public PanelTab Tab
    {
        get => tab;
        private set
        {
            if (!SetField(ref tab, value)) return;
            Raise(nameof(Lede));
            Raise(nameof(ShowSearch));
            Raise(nameof(ShowFilters));
            Raise(nameof(IsSendTab));
        }
    }

    public PanelFilter Filter
    {
        get => filter;
        set
        {
            if (SetField(ref filter, value)) Refresh(reorder: true);
        }
    }

    public string Query
    {
        get => query;
        set
        {
            if (SetField(ref query, value ?? "")) Refresh(reorder: true);
        }
    }

    public CardViewModel? SendTarget
    {
        get => sendTarget;
        set => SetField(ref sendTarget, value);
    }

    public bool ShowSearch => tab is PanelTab.Sessions or PanelTab.Waiting;

    public bool ShowFilters => tab == PanelTab.Sessions;

    public bool IsSendTab => tab == PanelTab.Send;

    public string Lede => tab switch
    {
        PanelTab.Home => "What needs you right now, across both profiles.",
        PanelTab.Sessions => "Every live Claude Code session. Click one for its status.",
        PanelTab.Waiting => "Questions and permission prompts waiting on you.",
        _ => "Send an info-only message to a session.",
    };

    /// <summary>Badge of the Waiting tab: open items (dismissed ones left out) plus open approvals.</summary>
    public int WaitingTotal
    {
        get
        {
            var t = Hub.EffectiveTotals;
            return t.WaitingItems + t.NeedsApproval;
        }
    }

    /// <summary>
    /// "1 relay message reached the wrong session (see relay.log)" while the daemon counts any such
    /// delivery; empty otherwise.
    /// </summary>
    public string RelayStrayText
    {
        get
        {
            var n = Hub.Meta.RelayStrayCount;
            if (n <= 0) return "";
            return n == 1
                ? "1 relay message reached the wrong session (see relay.log)"
                : $"{n.ToString(System.Globalization.CultureInfo.InvariantCulture)} relay messages reached the wrong session (see relay.log)";
        }
    }

    public string Summary
    {
        get => summary;
        private set => SetField(ref summary, value);
    }

    public bool IsEmpty
    {
        get => isEmpty;
        private set => SetField(ref isEmpty, value);
    }

    public string EmptyTitle => NoSessionsAtAll ? "No live sessions" : tab switch
    {
        PanelTab.Sessions => "No sessions match",
        _ => "All clear",
    };

    public string EmptyText => NoSessionsAtAll ? "Start a Claude Code session and it shows up here." : tab switch
    {
        PanelTab.Home => "When a session asks you a question or waits on a permission prompt, it shows up here.",
        PanelTab.Waiting => "Nothing waits on you. New questions show up here the moment a session writes them.",
        _ => "Try another filter or clear the search.",
    };

    // Send, or Sessions with no search and the All filter, while no session is live.
    private bool NoSessionsAtAll =>
        (tab == PanelTab.Send || (tab == PanelTab.Sessions && filter == PanelFilter.All && query.Trim().Length == 0))
        && !Hub.Sessions.Any(s => s.IsLive);

    public HubLink Link
    {
        get
        {
            if (health is BridgeHealth.Restarting or BridgeHealth.Failed) return HubLink.Reconnecting;
            if (!hasSnapshot) return HubLink.Connecting;
            return Hub.Meta.Daemon == "running" ? HubLink.Live : HubLink.DaemonStopped;
        }
    }

    public string LinkText => Link switch
    {
        HubLink.Live => $"Live · {Hub.Totals.Live} {(Hub.Totals.Live == 1 ? "session" : "sessions")}",
        HubLink.DaemonStopped => "Daemon stopped · last data",
        HubLink.Reconnecting => "Reconnecting to hub…",
        _ => "Connecting…",
    };

    public bool ShowBanner => Link is HubLink.DaemonStopped or HubLink.Reconnecting;

    public bool BannerIsError => Link == HubLink.Reconnecting;

    public string BannerText => health switch
    {
        BridgeHealth.Restarting r when Link == HubLink.Reconnecting =>
            $"Lost the link to hub.exe. Retrying (attempt {r.Attempt}, next in {Math.Max(1, (int)Math.Round(r.Next.TotalSeconds))} s).",
        BridgeHealth.Failed f when Link == HubLink.Reconnecting =>
            $"Lost the link to hub.exe. {f.Attempts} starts failed in a row; still retrying.",
        _ => Link == HubLink.DaemonStopped ? "The daemon is not running. Data may be old." : "",
    };

    public bool CanStartDaemon => Link == HubLink.DaemonStopped;

    /// <summary>Sends need a connected bridge and a running daemon.</summary>
    public bool CanSend => Link == HubLink.Live;

    /// <summary>"Fri, Nov 13 · updated 10 s ago".</summary>
    public string FooterText
    {
        get => footerText;
        private set => SetField(ref footerText, value);
    }

    /// <summary>Shown in the footer while drafts cannot be written to gui.json; empty otherwise.</summary>
    public string FooterWarning
    {
        get => footerWarning;
        private set => SetField(ref footerWarning, value);
    }

    /// <summary>The settings store failed to write (true) or wrote again (false).</summary>
    public void SetSaveFailed(bool failed) => FooterWarning = failed ? "Drafts could not be saved" : "";

    /// <summary>Time of the latest snapshot or tick.</summary>
    public DateTimeOffset Now => now;

    /// <summary>Every card, in snapshot order (the window groups them).</summary>
    public IReadOnlyList<CardViewModel> AllCards => [.. Hub.Sessions.Select(s => cards[s])];

    public CardViewModel? Card(string sessionId) =>
        cards.Values.FirstOrDefault(c => c.Session.SessionId == sessionId);

    public void Apply(HubSnapshot snapshot, DateTimeOffset at)
    {
        ArgumentNullException.ThrowIfNull(snapshot);
        now = at;
        Hub.Apply(snapshot);
        var wasSnapshot = hasSnapshot;
        hasSnapshot = true;

        var views = new Dictionary<string, SessionView>();
        foreach (var v in snapshot.Sessions) views.TryAdd(v.SessionId, v);

        var alive = new HashSet<SessionViewModel>(Hub.Sessions, ReferenceEqualityComparer.Instance);
        foreach (var gone in cards.Keys.Where(k => !alive.Contains(k)).ToList()) cards.Remove(gone);

        foreach (var row in Hub.Sessions)
        {
            var isNew = !cards.TryGetValue(row, out var card);
            card ??= cards[row] = new CardViewModel(row);
            var before = card.LatestReplyUuid;
            card.Update(views[row.SessionId], at);
            if (!isNew && row.IsExpanded && before != card.LatestReplyUuid) ExchangeWanted?.Invoke(card);
        }

        RaiseLink();
        Raise(nameof(WaitingTotal));
        Raise(nameof(RelayStrayText));
        Refresh(reorder: !wasSnapshot);
    }

    /// <summary>The owner dismissed an item or undid it: recount, and move the session between groups.</summary>
    private void OnDismissedChanged()
    {
        Raise(nameof(WaitingTotal));
        // Only between snapshots, once every row has its card.
        if (hasSnapshot && Hub.Sessions.All(cards.ContainsKey)) Refresh(reorder: false);
    }

    public void SetHealth(BridgeHealth value)
    {
        ArgumentNullException.ThrowIfNull(value);
        health = value;
        RaiseLink();
    }

    public void Tick(DateTimeOffset at)
    {
        now = at;
        foreach (var c in cards.Values) c.Tick(at);
        UpdateFooter();
    }

    public void SelectTab(PanelTab value)
    {
        Tab = value;
        Refresh(reorder: true);
    }

    /// <summary>Opens one card and closes the others; a second toggle closes it.</summary>
    public void Toggle(CardViewModel card)
    {
        ArgumentNullException.ThrowIfNull(card);
        var open = !card.Session.IsExpanded;
        foreach (var c in cards.Values)
        {
            if (!ReferenceEquals(c, card)) c.Session.IsExpanded = false;
        }

        card.Session.IsExpanded = open;
        if (open) ExchangeWanted?.Invoke(card);
    }

    /// <summary>Shows one session open in the Sessions tab (balloon click, Home card).</summary>
    public void FocusSession(string sessionId)
    {
        var card = Card(sessionId);
        if (card is null) return;
        filter = card.Session.IsLive ? PanelFilter.All : PanelFilter.Ended;
        Raise(nameof(Filter));
        query = "";
        Raise(nameof(Query));
        SelectTab(PanelTab.Sessions);
        if (!card.Session.IsExpanded) Toggle(card);
    }

    /// <summary>Recomputes the shown list. Without reorder, cards already shown keep their place.</summary>
    public void Refresh(bool reorder)
    {
        var all = Hub.Sessions.Select(s => cards[s]).ToList();
        IReadOnlyList<CardViewModel> wanted = tab switch
        {
            PanelTab.Home => [.. all.Where(c => c.Session.IsLive && c.Session.Group == SessionGroup.Waiting)],
            PanelTab.Waiting => [.. Hub.Filter(PanelFilter.Waiting, query).Select(s => cards[s])],
            PanelTab.Sessions => [.. Hub.Filter(filter, query).Select(s => cards[s])],
            _ => [],
        };
        StableList.Sync(Items, wanted, reorder);

        var live = all.Where(c => c.Session.IsLive).ToList();
        StableList.Sync(SendTargets, live, reorder);
        if (sendTarget is null || !live.Contains(sendTarget)) SendTarget = live.FirstOrDefault();

        IsEmpty = tab == PanelTab.Send ? live.Count == 0 : Items.Count == 0;
        Raise(nameof(EmptyTitle));
        Raise(nameof(EmptyText));
        Summary = tab switch
        {
            PanelTab.Home => HomeSummary(Items.Count),
            PanelTab.Send => "",
            _ => $"{Items.Count} of {Hub.Totals.Live} live sessions · {WaitingTotal} waiting on you"
                 + (Link == HubLink.Live ? " · live" : ""),
        };
        UpdateFooter();
    }

    private string HomeSummary(int sessions)
    {
        var t = WaitingTotal;
        return $"{t} {(t == 1 ? "item waits" : "items wait")} on you across {sessions} {(sessions == 1 ? "session" : "sessions")}.";
    }

    private void UpdateFooter()
    {
        var day = TimeText.Day(now.ToLocalTime());
        var newest = cards.Values.Select(c => c.UpdatedAt).DefaultIfEmpty(0).Max();
        FooterText = newest > 0 ? $"{day} · updated {TimeText.Ago(newest, now)}" : day;
    }

    private void RaiseLink()
    {
        Raise(nameof(Link));
        Raise(nameof(LinkText));
        Raise(nameof(ShowBanner));
        Raise(nameof(BannerIsError));
        Raise(nameof(BannerText));
        Raise(nameof(CanStartDaemon));
        Raise(nameof(CanSend));
    }
}
