using System.Collections.ObjectModel;
using System.ComponentModel;
using SessionHub.Core.Layout;
using SessionHub.Core.ViewModels;

namespace SessionHub.Core.App;

public enum WindowTab
{
    Status,
    History,
    Send,
}

/// <summary>One group of the window's session list ("Waiting on you · 3").</summary>
public sealed class WindowGroup : ObservableObject
{
    private bool isExpanded;

    public WindowGroup(SessionGroup kind, string title)
    {
        Kind = kind;
        Title = title;
        isExpanded = kind != SessionGroup.Ended;
        Items.CollectionChanged += (_, _) =>
        {
            Raise(nameof(Header));
            Raise(nameof(HasItems));
            Raise(nameof(ShowItems));
        };
    }

    public SessionGroup Kind { get; }

    public string Title { get; }

    /// <summary>Only the ended group folds (spec §8.4: collapsed "Ended (7 days)").</summary>
    public bool Collapsible => Kind == SessionGroup.Ended;

    public ObservableCollection<CardViewModel> Items { get; } = [];

    public string Header => $"{Title} · {Items.Count}";

    public bool HasItems => Items.Count > 0;

    public bool IsExpanded
    {
        get => isExpanded;
        internal set
        {
            if (SetField(ref isExpanded, value)) Raise(nameof(ShowItems));
        }
    }

    public bool ShowItems => isExpanded && Items.Count > 0;
}

/// <summary>
/// The window (design 2): the grouped session list with its search, the selected session, the tab,
/// the header and status bar texts and the width breakpoints. Built on the panel's cards, so drafts,
/// send states and the last exchange are the same objects in both. Call on the UI thread.
/// </summary>
public sealed class WindowViewModel : ObservableObject
{
    private string query = "";
    private CardViewModel? selected;
    private WindowTab tab = WindowTab.Status;
    private double listWidth = WindowLayout.ListWidth(WindowPlacements.DefaultWidth);
    private bool showSubtitles = WindowLayout.ShowSubtitles(WindowPlacements.DefaultWidth);
    private bool stackStatusBoxes = WindowLayout.StackStatusBoxes(WindowPlacements.DefaultWidth);
    private (long UpdatedAt, string? Reply) selectedActivity;
    private bool selectedLive;

    public WindowViewModel(PanelViewModel panel)
    {
        ArgumentNullException.ThrowIfNull(panel);
        Panel = panel;
        Groups =
        [
            new WindowGroup(SessionGroup.Waiting, "Waiting on you"),
            new WindowGroup(SessionGroup.Working, "Working"),
            new WindowGroup(SessionGroup.Idle, "Idle"),
            new WindowGroup(SessionGroup.Ended, "Ended (7 days)"),
        ];
        panel.PropertyChanged += OnPanelChanged;
        // After the panel's own handler (it was built first), so every row has its card.
        panel.Hub.DismissedChanged += () =>
        {
            if (panel.Hub.Sessions.All(s => panel.Card(s.SessionId) is not null)) Refresh(reorder: false);
        };
    }

    /// <summary>The selection moved (null: no session left). The window loads that session's history and exchange.</summary>
    public event Action<CardViewModel?>? SelectionChanged;

    /// <summary>
    /// The selected session has new activity (a new reply, or a moved <c>updatedAt</c>, which also
    /// covers new owner and peer messages): its history and last exchange are fetched again.
    /// Raised at most once per <see cref="Refresh"/>.
    /// </summary>
    public event Action<CardViewModel>? NewActivity;

    public PanelViewModel Panel { get; }

    public IReadOnlyList<WindowGroup> Groups { get; }

    /// <summary>List search over name, folder and question ids and texts.</summary>
    public string Query
    {
        get => query;
        set
        {
            if (SetField(ref query, value ?? "")) Refresh(reorder: true);
        }
    }

    public CardViewModel? Selected
    {
        get => selected;
        private set
        {
            var old = selected;
            if (!SetField(ref selected, value)) return;
            if (old is not null) old.PropertyChanged -= OnSelectedChanged;
            if (value is not null) value.PropertyChanged += OnSelectedChanged;
            selectedActivity = Activity(value);
            selectedLive = value?.Session.IsLive ?? false;
            Raise(nameof(HasSelection));
            Raise(nameof(HeaderSub));
            SelectionChanged?.Invoke(value);
        }
    }

    public bool HasSelection => selected is not null;

    public WindowTab Tab
    {
        get => tab;
        set => SetField(ref tab, value);
    }

    /// <summary>"D:\work · .claude · 3 waiting on you".</summary>
    public string HeaderSub => selected is null
        ? ""
        : $"{selected.Session.Cwd} · {selected.ProfileChip.Text} · {selected.StatusChip.Text}";

    /// <summary>Title-bar chip: "8 waiting on you", empty when nothing waits.</summary>
    public string WaitingChip => Panel.WaitingTotal > 0 ? $"{Panel.WaitingTotal} waiting on you" : "";

    /// <summary>Status bar: "live · last update 10 s ago" (the view draws the dot).</summary>
    public string StatusText
    {
        get
        {
            var state = Panel.Link switch
            {
                HubLink.Live => "live",
                HubLink.DaemonStopped => "daemon stopped",
                HubLink.Reconnecting => "reconnecting",
                _ => "connecting…",
            };
            if (Panel.Link == HubLink.Connecting) return state;
            var newest = Panel.AllCards.Select(c => c.UpdatedAt).DefaultIfEmpty(0).Max();
            return newest > 0 ? $"{state} · last update {TimeText.Ago(newest, Panel.Now)}" : state;
        }
    }

    /// <summary>Status bar warning after <see cref="StatusText"/>: " · 1 relay message reached the wrong session (see relay.log)".</summary>
    public string StrayText => Panel.RelayStrayText.Length == 0 ? "" : " · " + Panel.RelayStrayText;

    public double ListWidth
    {
        get => listWidth;
        private set => SetField(ref listWidth, value);
    }

    public bool ShowSubtitles
    {
        get => showSubtitles;
        private set => SetField(ref showSubtitles, value);
    }

    public bool StackStatusBoxes
    {
        get => stackStatusBoxes;
        private set => SetField(ref stackStatusBoxes, value);
    }

    /// <summary>Applies the breakpoints of <see cref="WindowLayout"/> for a window this wide (DIP).</summary>
    public void SetWidth(double widthDip)
    {
        ListWidth = WindowLayout.ListWidth(widthDip);
        ShowSubtitles = WindowLayout.ShowSubtitles(widthDip);
        StackStatusBoxes = WindowLayout.StackStatusBoxes(widthDip);
    }

    public void Select(CardViewModel? card) => Selected = card;

    /// <summary>Selects a session by id (opening the ended group for an ended one). False when it is not known.</summary>
    public bool Select(string sessionId)
    {
        var card = Panel.Card(sessionId);
        if (card is null) return false;
        if (!card.Session.IsLive) Groups[3].IsExpanded = true;
        Selected = card;
        return true;
    }

    public static void ToggleGroup(WindowGroup group)
    {
        ArgumentNullException.ThrowIfNull(group);
        if (group.Collapsible) group.IsExpanded = !group.IsExpanded;
    }

    /// <summary>Recomputes the groups after a snapshot (no reorder) or a search change (reorder).</summary>
    public void Refresh(bool reorder)
    {
        var all = Panel.AllCards;
        var filters = new[] { PanelFilter.Waiting, PanelFilter.Working, PanelFilter.Idle, PanelFilter.Ended };
        for (var i = 0; i < Groups.Count; i++)
        {
            var wanted = Panel.Hub.Filter(filters[i], query).Select(s => Panel.Card(s.SessionId)!).ToList();
            StableList.Sync(Groups[i].Items, wanted, reorder);
        }

        if (selected is null || !all.Contains(selected))
        {
            Selected = Groups.SelectMany(g => Panel.Hub.Filter(Filter(g.Kind), null))
                .Select(s => Panel.Card(s.SessionId))
                .FirstOrDefault();
        }
        else
        {
            // The selected session just ended: open its group once, so its row stays in sight.
            var live = selected.Session.IsLive;
            if (selectedLive && !live) Groups[3].IsExpanded = true;
            selectedLive = live;

            var activity = Activity(selected);
            if (activity != selectedActivity)
            {
                selectedActivity = activity;
                NewActivity?.Invoke(selected);
            }
        }

        Raise(nameof(HeaderSub));
        Raise(nameof(WaitingChip));
        Raise(nameof(StatusText));
        Raise(nameof(StrayText));
    }

    /// <summary>Refreshes the "… ago" text of the status bar.</summary>
    public void Tick() => Raise(nameof(StatusText));

    private static (long, string?) Activity(CardViewModel? card) => (card?.UpdatedAt ?? 0, card?.LatestReplyUuid);

    private static PanelFilter Filter(SessionGroup g) => g switch
    {
        SessionGroup.Waiting => PanelFilter.Waiting,
        SessionGroup.Working => PanelFilter.Working,
        SessionGroup.Idle => PanelFilter.Idle,
        _ => PanelFilter.Ended,
    };

    private void OnPanelChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(PanelViewModel.Link):
                Raise(nameof(StatusText));
                break;
            case nameof(PanelViewModel.WaitingTotal):
                Raise(nameof(WaitingChip));
                break;
            case nameof(PanelViewModel.RelayStrayText):
                Raise(nameof(StrayText));
                break;
        }
    }

    private void OnSelectedChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(CardViewModel.StatusChip):
            case nameof(CardViewModel.SubText):
                Raise(nameof(HeaderSub));
                break;
        }
    }
}
