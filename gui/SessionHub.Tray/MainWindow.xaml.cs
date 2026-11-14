using System;
using System.ComponentModel;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media;
using System.Windows.Threading;
using SessionHub.Core.App;
using SessionHub.Core.Layout;
using SessionHub.Core.Protocol;
using SessionHub.Core.Settings;
using SessionHub.Core.ViewModels;

namespace SessionHub.Tray;

/// <summary>Commands of the window's session list. The parameter is the card or the group.</summary>
public static class WindowCommands
{
    public static readonly RoutedUICommand Select = new("Select session", nameof(Select), typeof(WindowCommands));
    public static readonly RoutedUICommand ToggleGroup = new("Show or hide", nameof(ToggleGroup), typeof(WindowCommands));
}

/// <summary>What the window needs from the app. <see cref="TrayController"/> is the real one; the render check has its own.</summary>
internal interface IWindowHost
{
    /// <summary>False while rendering offscreen: no placement is read or saved then.</summary>
    bool Interactive { get; }

    /// <summary>The saved placement (px), or null.</summary>
    WindowPlacement? Placement { get; }

    void SavePlacement(WindowPlacement placement);

    FocusResult JumpTo(SessionViewModel session);

    Task SendAsync(CardViewModel card, SendMode mode);

    /// <summary>Sends one waiting item's answer on its own.</summary>
    Task SendItemAsync(CardViewModel card, string itemKey);

    /// <summary>True when the daemon runs afterwards.</summary>
    Task<bool> TryStartDaemonAsync();

    Task<Reply> RequestAsync(BridgeRequest request, TimeSpan timeout);

    /// <summary>Fetches the card's last exchange (the Status tab shows it).</summary>
    void FetchExchange(CardViewModel card);

    void WindowClosed();
}

/// <summary>
/// The window (design 2): grouped session list and the selected session's Status, History and Send
/// tabs. One instance at a time; closing it drops the window, the view models stay with the tray.
/// Placement is saved as px of its monitor and restored onto a monitor that still exists.
/// </summary>
internal sealed partial class MainWindow : Window
{
    private static readonly TimeSpan NoticeTime = TimeSpan.FromSeconds(5);

    private readonly IWindowHost host;
    private readonly WindowViewModel vm;
    private readonly HistoryViewModel history;
    private readonly DispatcherTimer noticeTimer;
    private bool syncing;
    private bool placed;
    private WindowState restoreState = WindowState.Normal;

    public MainWindow(IWindowHost host, WindowViewModel vm)
    {
        this.host = host;
        this.vm = vm;
        InitializeComponent();
        DataContext = vm;
        history = new HistoryViewModel(host.RequestAsync, TimeZoneInfo.Local, id => vm.Panel.Card(id)?.Session.Name);
        HistoryPane.DataContext = history;
        noticeTimer = new DispatcherTimer(NoticeTime, DispatcherPriority.Normal, (_, _) => HideNotice(), Dispatcher);
        noticeTimer.Stop();

        CommandBindings.Add(new CommandBinding(WindowCommands.Select, (_, e) => On<CardViewModel>(e, vm.Select)));
        CommandBindings.Add(new CommandBinding(WindowCommands.ToggleGroup, (_, e) => On<WindowGroup>(e, WindowViewModel.ToggleGroup)));
        CommandBindings.Add(new CommandBinding(
            PanelCommands.Jump,
            (_, e) => On<CardViewModel>(e, Jump),
            (_, e) => e.CanExecute = e.Parameter is CardViewModel { CanJump: true }));
        CommandBindings.Add(new CommandBinding(
            PanelCommands.Send,
            (_, e) => On<CardViewModel>(e, c => _ = host.SendAsync(c, SendMode.Normal)),
            (_, e) => e.CanExecute = CanSend(e.Parameter)));
        CommandBindings.Add(new CommandBinding(
            PanelCommands.SendAnyway,
            (_, e) => On<CardViewModel>(e, c => _ = host.SendAsync(c, SendMode.OverrideInFlight)),
            (_, e) => e.CanExecute = CanSend(e.Parameter)));
        CommandBindings.Add(new CommandBinding(
            PanelCommands.SendSecretAnyway,
            (_, e) => On<CardViewModel>(e, c => _ = host.SendAsync(c, SendMode.AllowSecret)),
            (_, e) => e.CanExecute = CanSend(e.Parameter) && e.Parameter is CardViewModel { SendBox.CanSendAnyway: true }));
        CommandBindings.Add(new CommandBinding(
            PanelCommands.SendItem,
            (_, e) => On<AnswerItemViewModel>(e, i =>
            {
                if (vm.Panel.Card(i.SessionId) is { } c) _ = host.SendItemAsync(c, i.Key);
            }),
            (_, e) => e.CanExecute = CanSendItem(e.Parameter)));

        vm.PropertyChanged += OnViewModelChanged;
        vm.Panel.PropertyChanged += OnPanelChanged;
        vm.SelectionChanged += OnSelection;
        vm.NewActivity += OnNewActivity;
        SizeChanged += (_, _) =>
        {
            vm.SetWidth(ActualWidth);
            SavePlacement();
        };
        LocationChanged += (_, _) => SavePlacement();
        StateChanged += OnStateChanged;
        DpiChanged += (_, _) => ApplyMaximizedMargin();
        PreviewKeyDown += OnKey;
        Closed += OnClosed;

        vm.SetWidth(Width);
        ApplyLayout();
        SyncTab();
        OnSelection(vm.Selected);
    }

    public HistoryViewModel History => history;

    /// <summary>The History tab (the render check scrolls it).</summary>
    public Controls.HistoryView HistoryControl => HistoryPane;

    /// <summary>
    /// Shows the window on a session (null keeps the selection). The first time it is placed where it
    /// was last time, on a monitor that still exists; later it is brought back and focused.
    /// </summary>
    public void ShowOn(string? sessionId)
    {
        if (sessionId is not null) vm.Select(sessionId);
        if (!IsVisible)
        {
            var hwnd = new WindowInteropHelper(this).EnsureHandle();
            var saved = host.Placement;
            var (areas, primaryScale) = Native.WorkAreas();
            var rect = WindowPlacements.Initial(saved, areas, primaryScale);

            // Twice: the first move may cross monitors and change the window's DPI, which resizes it.
            Native.PlaceWindow(hwnd, rect);
            Show();
            Native.PlaceWindow(hwnd, rect);
            if (saved?.Maximized == true) WindowState = WindowState.Maximized;
            placed = true;
            SavePlacement();
        }
        else if (WindowState == WindowState.Minimized)
        {
            WindowState = restoreState;
        }

        Activate();
    }

    public void ShowNotice(string text)
    {
        NoticeText.Text = text;
        Notice.Visibility = Visibility.Visible;
        noticeTimer.Stop();
        noticeTimer.Start();
    }

    private void HideNotice()
    {
        noticeTimer.Stop();
        Notice.Visibility = Visibility.Collapsed;
    }

    private bool CanSend(object? parameter) =>
        vm.Panel.CanSend && parameter is CardViewModel { IsRequesting: false, Session.IsLive: true } card
        && (!card.Answers.IsActive || card.Answers.CanSend);

    /// <summary>One item's Send: its box filled and within the limit, and its card free to send (as the main button).</summary>
    private bool CanSendItem(object? parameter) =>
        parameter is AnswerItemViewModel { CanSend: true } item
        && vm.Panel.CanSend
        && vm.Panel.Card(item.SessionId) is { IsRequesting: false, Session.IsLive: true };

    /// <summary>The Send tab's first box: the first answer box when questions wait, else the reply box.</summary>
    private void FocusSendInput()
    {
        if (vm.Selected?.Answers.IsActive == true) WindowAnswerForm.FocusInput();
        else WindowSendBox.FocusInput();
    }

    private static void On<T>(ExecutedRoutedEventArgs e, Action<T> action)
        where T : class
    {
        if (e.Parameter is T value) action(value);
        e.Handled = true;
    }

    private void Jump(CardViewModel card)
    {
        var result = host.JumpTo(card.Session);
        if (result == FocusResult.Focused) return;
        ShowNotice(result == FocusResult.Denied
            ? $"Windows did not let Session Hub bring {card.Session.Name} to the front."
            : $"No terminal window found for {card.Session.Name}.");
    }

    /// <summary>Restore bounds (DIP) times this monitor's scale; debounced by the settings store.</summary>
    private void SavePlacement()
    {
        if (!placed || !host.Interactive || WindowState == WindowState.Minimized) return;
        var b = RestoreBounds;
        if (b.IsEmpty || !double.IsFinite(b.Width) || !double.IsFinite(b.Height)) return;
        var scale = VisualTreeHelper.GetDpi(this).DpiScaleX;
        host.SavePlacement(WindowPlacements.ToSaved(b.Left, b.Top, b.Width, b.Height, scale, WindowState == WindowState.Maximized));
    }

    private void OnStateChanged(object? sender, EventArgs e)
    {
        var max = WindowState == WindowState.Maximized;
        if (WindowState != WindowState.Minimized) restoreState = WindowState;

        ApplyMaximizedMargin();
        MaxGlyph.Visibility = max ? Visibility.Collapsed : Visibility.Visible;
        RestoreGlyph.Visibility = max ? Visibility.Visible : Visibility.Collapsed;
        MaxButton.ToolTip = max ? "Restore" : "Maximize";
        SavePlacement();
    }

    /// <summary>
    /// A maximized chrome-less window reaches past its monitor by the resize frame; that frame is
    /// measured at this window's DPI (not the primary's), so the edge is right on every monitor.
    /// </summary>
    private void ApplyMaximizedMargin()
    {
        if (WindowState != WindowState.Maximized)
        {
            Root.Margin = new Thickness(0);
            return;
        }

        var scale = VisualTreeHelper.GetDpi(this).DpiScaleX;
        var (x, y) = Native.MaximizedFrame(scale);
        Root.Margin = new Thickness(x / scale, y / scale, x / scale, y / scale);
    }

    private void OnSelection(CardViewModel? card)
    {
        _ = Guard("history-show", () => history.ShowAsync(card?.Session.SessionId));
        if (card is not null) host.FetchExchange(card);
        CommandManager.InvalidateRequerySuggested();
    }

    private void OnNewActivity(CardViewModel card)
    {
        _ = Guard("history-refresh", history.RefreshAsync);
        host.FetchExchange(card);
    }

    private static async Task Guard(string where, Func<Task> action)
    {
        try
        {
            await action();
        }
        catch (Exception e)
        {
            TrayLog.Error(where, e);
        }
    }

    private void OnViewModelChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(WindowViewModel.ListWidth):
            case nameof(WindowViewModel.StackStatusBoxes):
                ApplyLayout();
                break;
            case nameof(WindowViewModel.Tab):
                SyncTab();
                break;
        }
    }

    private void OnPanelChanged(object? sender, PropertyChangedEventArgs e)
    {
        if (e.PropertyName == nameof(PanelViewModel.CanSend)) CommandManager.InvalidateRequerySuggested();
    }

    /// <summary>Breakpoints of <see cref="WindowLayout"/>: list width and the two status boxes side by side or stacked.</summary>
    private void ApplyLayout()
    {
        ListColumn.Width = new GridLength(vm.ListWidth);
        var stacked = vm.StackStatusBoxes;
        Grid.SetColumn(WaitingBox, stacked ? 0 : 2);
        Grid.SetRow(WaitingBox, stacked ? 2 : 0);
        StatusGap.Width = new GridLength(stacked ? 0 : 14);
        StatusRight.Width = stacked ? new GridLength(0) : new GridLength(1, GridUnitType.Star);
        StatusStackGap.Height = new GridLength(stacked ? 14 : 0);
    }

    private void SyncTab()
    {
        syncing = true;
        try
        {
            (vm.Tab switch
            {
                WindowTab.History => TabHistory,
                WindowTab.Send => TabSend,
                _ => TabStatus,
            }).IsChecked = true;
        }
        finally
        {
            syncing = false;
        }

        StatusPane.Visibility = vm.Tab == WindowTab.Status ? Visibility.Visible : Visibility.Collapsed;
        HistoryPane.Visibility = vm.Tab == WindowTab.History ? Visibility.Visible : Visibility.Collapsed;
        SendPane.Visibility = vm.Tab == WindowTab.Send ? Visibility.Visible : Visibility.Collapsed;
    }

    private void OnTab(object sender, RoutedEventArgs e)
    {
        if (syncing || sender is not RadioButton { Tag: string tag }) return;
        vm.Tab = Enum.Parse<WindowTab>(tag);
    }

    private void OnKey(object sender, KeyEventArgs e)
    {
        if ((Keyboard.Modifiers & ModifierKeys.Control) == 0) return;
        WindowTab? tab = e.Key switch
        {
            Key.D1 or Key.NumPad1 => WindowTab.Status,
            Key.D2 or Key.NumPad2 => WindowTab.History,
            Key.D3 or Key.NumPad3 => WindowTab.Send,
            _ => null,
        };
        if (tab is { } t)
        {
            vm.Tab = t;
            if (t == WindowTab.Send) Dispatcher.BeginInvoke(FocusSendInput, DispatcherPriority.Input);
            e.Handled = true;
        }
        else if (e.Key == Key.F)
        {
            if (vm.Tab == WindowTab.History) HistoryPane.FocusSearch();
            else
            {
                ListSearch.Focus();
                ListSearch.SelectAll();
            }

            e.Handled = true;
        }
    }

    private void OnListSearchChanged(object sender, TextChangedEventArgs e) =>
        ListSearchHint.Visibility = ListSearch.Text.Length == 0 ? Visibility.Visible : Visibility.Collapsed;

    private void OnListSearchKey(object sender, KeyEventArgs e)
    {
        if (e.Key != Key.Escape || ListSearch.Text.Length == 0) return;
        ListSearch.Clear();
        e.Handled = true;
    }

    private void OnMinimize(object sender, RoutedEventArgs e) => WindowState = WindowState.Minimized;

    private void OnMaximize(object sender, RoutedEventArgs e) =>
        WindowState = WindowState == WindowState.Maximized ? WindowState.Normal : WindowState.Maximized;

    private void OnCloseWindow(object sender, RoutedEventArgs e) => Close();

    private async void OnStartDaemon(object sender, RoutedEventArgs e)
    {
        try
        {
            if (!await host.TryStartDaemonAsync())
                ShowNotice("The daemon did not start. Run 'hub daemon start' in a terminal to see why.");
        }
        catch (Exception ex)
        {
            TrayLog.Error("start-daemon", ex);
        }
    }

    private void OnClosed(object? sender, EventArgs e)
    {
        noticeTimer.Stop();
        vm.PropertyChanged -= OnViewModelChanged;
        vm.Panel.PropertyChanged -= OnPanelChanged;
        vm.SelectionChanged -= OnSelection;
        vm.NewActivity -= OnNewActivity;
        history.Query = "";
        host.WindowClosed();
    }
}
