using System;
using System.ComponentModel;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Interop;
using System.Windows.Media.Animation;
using System.Windows.Threading;
using SessionHub.Core.App;
using SessionHub.Core.Layout;
using SessionHub.Core.ViewModels;

namespace SessionHub.Tray;

/// <summary>
/// The tray panel (design 1). Kept alive between openings, so cards, drafts and the scroll position
/// stay. Placed by <see cref="PanelPlacement"/> next to the taskbar's tray corner; hides on Esc and
/// when it loses activation.
/// </summary>
internal sealed partial class PanelWindow : Window
{
    /// <summary>The transparent margin around the panel that holds its shadow (DIP).</summary>
    public const double ShadowMargin = 16;

    private static readonly TimeSpan NoticeTime = TimeSpan.FromSeconds(5);

    private readonly IPanelHost host;
    private readonly PanelViewModel vm;
    private readonly DispatcherTimer noticeTimer;
    private DateTime hiddenAt = DateTime.MinValue;
    private DateTime shownAt = DateTime.MinValue;
    private bool syncing;

    public PanelWindow(IPanelHost host, PanelViewModel vm)
    {
        this.host = host;
        this.vm = vm;
        InitializeComponent();
        DataContext = vm;
        noticeTimer = new DispatcherTimer(NoticeTime, DispatcherPriority.Normal, (_, _) => HideNotice(), Dispatcher);
        noticeTimer.Stop();

        CommandBindings.Add(new CommandBinding(PanelCommands.Toggle, (_, e) => On<CardViewModel>(e, vm.Toggle)));
        CommandBindings.Add(new CommandBinding(PanelCommands.Focus, (_, e) => On<CardViewModel>(e, c => vm.FocusSession(c.Session.SessionId))));
        CommandBindings.Add(new CommandBinding(
            PanelCommands.Jump,
            (_, e) => On<CardViewModel>(e, c => host.Jump(c.Session)),
            (_, e) => e.CanExecute = e.Parameter is CardViewModel { CanJump: true }));
        CommandBindings.Add(new CommandBinding(PanelCommands.OpenInWindow, (_, e) => On<CardViewModel>(e, c => host.OpenWindow(c.Session.SessionId))));
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
                if (vm.Card(i.SessionId) is { } c) _ = host.SendItemAsync(c, i.Key);
            }),
            (_, e) => e.CanExecute = CanSendItem(e.Parameter)));

        vm.PropertyChanged += OnViewModelChanged;
        PreviewKeyDown += OnKey;
        // A deactivation in the first 500 ms after showing comes from the activation itself (the
        // foreground moving back, or a retry below), not from the user clicking away: ignore it.
        Deactivated += (_, _) =>
        {
            if (host.Interactive && DateTime.UtcNow - shownAt > TimeSpan.FromMilliseconds(500)) HideNow();
        };
        SyncAll();
    }

    public bool IsOpen => IsVisible;

    /// <summary>The panel hid itself (lost activation) a moment ago: the tray click that caused it must not reopen it.</summary>
    public bool JustHidden => DateTime.UtcNow - hiddenAt < TimeSpan.FromMilliseconds(400);

    /// <summary>Shows the panel next to the tray, at the taskbar monitor's scale, and takes the focus.</summary>
    public void ShowNearTray()
    {
        var hwnd = new WindowInteropHelper(this).EnsureHandle();
        var (edge, work, scale) = Native.TaskbarPlacement();
        var panel = PanelPlacement.Compute(work, edge, scale);
        var m = (int)Math.Round(ShadowMargin * scale);
        var outer = new RectPx(panel.X - m, panel.Y - m, panel.W + (2 * m), panel.H + (2 * m));

        // Twice: the first move may cross monitors and change the window's DPI, which resizes it.
        Native.PlaceWindow(hwnd, outer);
        var wasVisible = IsVisible;
        shownAt = DateTime.UtcNow;
        Show();
        Native.PlaceWindow(hwnd, outer);
        Activate();
        Native.Activate(hwnd);
        if (!IsActive)
        {
            // Windows can refuse the foreground to a process that was not just clicked (a second
            // start, a balloon). Try once more when the dispatcher is idle; then it stays inactive
            // until the user clicks it.
            Dispatcher.BeginInvoke(
                () =>
                {
                    if (!IsVisible || IsActive) return;
                    shownAt = DateTime.UtcNow;
                    Activate();
                    Native.Activate(hwnd);
                },
                DispatcherPriority.ApplicationIdle);
        }

        if (!wasVisible) Animate();
        FocusDefault();
    }

    public void HideNow()
    {
        if (!IsVisible) return;
        hiddenAt = DateTime.UtcNow;
        HideNotice();
        Hide();
        host.PanelHidden();
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

    /// <summary>Fade in and rise 12 px over 180 ms, unless Windows animation effects are off.</summary>
    private void Animate()
    {
        if (!SystemParameters.ClientAreaAnimation)
        {
            Root.Opacity = 1;
            Rise.Y = 0;
            return;
        }

        var ease = new CubicEase { EasingMode = EasingMode.EaseOut };
        var time = TimeSpan.FromMilliseconds(180);
        Root.BeginAnimation(OpacityProperty, new DoubleAnimation(0, 1, time) { EasingFunction = ease });
        Rise.BeginAnimation(System.Windows.Media.TranslateTransform.YProperty, new DoubleAnimation(12, 0, time) { EasingFunction = ease });
    }

    private void FocusDefault()
    {
        if (vm.Tab == PanelTab.Send)
        {
            if (vm.SendTarget?.Answers.IsActive == true) SendTabAnswers.FocusInput();
            else SendTabBox.FocusInput();
        }
        else if (vm.ShowSearch) Search.Focus();
        else Focus();
    }

    /// <summary>Sends need a live link, and the card's previous request back (no double send on a double Enter).</summary>
    private bool CanSend(object? parameter) =>
        vm.CanSend && parameter is CardViewModel { IsRequesting: false } card && (!card.Answers.IsActive || card.Answers.CanSend);

    /// <summary>One item's Send: its box filled and within the limit, and its card free to send (as the main button).</summary>
    private bool CanSendItem(object? parameter) =>
        parameter is AnswerItemViewModel { CanSend: true } item && vm.CanSend && vm.Card(item.SessionId) is { IsRequesting: false };

    private static void On<T>(ExecutedRoutedEventArgs e, Action<T> action)
        where T : class
    {
        if (e.Parameter is T value) action(value);
        e.Handled = true;
    }

    private void OnKey(object sender, KeyEventArgs e)
    {
        var ctrl = (Keyboard.Modifiers & ModifierKeys.Control) != 0;
        if (e.Key == Key.Escape)
        {
            // Esc first closes an open settings menu or picker, then the panel.
            if (Picker.IsDropDownOpen) return;
            HideNow();
            e.Handled = true;
        }
        else if (ctrl && e.Key is >= Key.D1 and <= Key.D4)
        {
            vm.SelectTab((PanelTab)(e.Key - Key.D1));
            FocusDefault();
            e.Handled = true;
        }
        else if (ctrl && e.Key is >= Key.NumPad1 and <= Key.NumPad4)
        {
            vm.SelectTab((PanelTab)(e.Key - Key.NumPad1));
            FocusDefault();
            e.Handled = true;
        }
        else if (ctrl && e.Key == Key.F)
        {
            if (!vm.ShowSearch) vm.SelectTab(PanelTab.Sessions);
            Search.Focus();
            Search.SelectAll();
            e.Handled = true;
        }
    }

    private void OnViewModelChanged(object? sender, PropertyChangedEventArgs e)
    {
        switch (e.PropertyName)
        {
            case nameof(PanelViewModel.Tab):
            case nameof(PanelViewModel.Filter):
                SyncSelectors();
                break;
            case nameof(PanelViewModel.WaitingTotal):
                SyncBadge();
                break;
            case nameof(PanelViewModel.Link):
                SyncLink();
                break;
            case nameof(PanelViewModel.CanSend):
                CommandManager.InvalidateRequerySuggested();
                break;
        }
    }

    private void SyncAll()
    {
        SyncSelectors();
        SyncBadge();
        SyncLink();
        SearchHint.Visibility = string.IsNullOrEmpty(Search.Text) ? Visibility.Visible : Visibility.Collapsed;
    }

    private void SyncSelectors()
    {
        syncing = true;
        try
        {
            (vm.Tab switch
            {
                PanelTab.Home => TabHome,
                PanelTab.Waiting => TabWaiting,
                PanelTab.Send => TabSend,
                _ => TabSessions,
            }).IsChecked = true;
            (vm.Filter switch
            {
                PanelFilter.Waiting => FilterWaiting,
                PanelFilter.Working => FilterWorking,
                PanelFilter.Idle => FilterIdle,
                PanelFilter.Ended => FilterEnded,
                _ => FilterAll,
            }).IsChecked = true;
        }
        finally
        {
            syncing = false;
        }

        HomeList.Visibility = vm.Tab == PanelTab.Home ? Visibility.Visible : Visibility.Collapsed;
        CardList.Visibility = vm.Tab is PanelTab.Sessions or PanelTab.Waiting ? Visibility.Visible : Visibility.Collapsed;
        SyncBadge();
    }

    private void SyncBadge()
    {
        var n = vm.WaitingTotal;
        WaitingBadge.Visibility = n > 0 ? Visibility.Visible : Visibility.Collapsed;
        WaitingCount.Text = n > 99 ? "99+" : n.ToString(System.Globalization.CultureInfo.InvariantCulture);
        var on = vm.Tab == PanelTab.Waiting;
        WaitingBadge.SetResourceReference(System.Windows.Controls.Border.BackgroundProperty, on ? "OnAccent" : "Badge");
        WaitingCount.SetResourceReference(TextBlock.ForegroundProperty, on ? "AccentFill" : "BadgeFg");
    }

    private void SyncLink()
    {
        var (fill, dot) = vm.Link switch
        {
            HubLink.Live => ("GreenSoft", "Green"),
            HubLink.Reconnecting => ("RedSoft", "RedDot"),
            _ => ("Surface2", "Muted"),
        };
        LinkPill.SetResourceReference(System.Windows.Controls.Border.BackgroundProperty, fill);
        LinkDot.SetResourceReference(System.Windows.Shapes.Shape.FillProperty, dot);

        var error = vm.BannerIsError;
        Banner.SetResourceReference(System.Windows.Controls.Border.BackgroundProperty, error ? "RedSoft" : "Surface2");
        Banner.SetResourceReference(System.Windows.Controls.Border.BorderBrushProperty, error ? "RedSoft" : "Line");
        BannerIcon.Data = (System.Windows.Media.Geometry)FindResource(error ? "Icon.TriangleAlert" : "Icon.CircleAlert");
        BannerIcon.SetResourceReference(Controls.Glyph.ForegroundProperty, error ? "Red" : "Text2");
    }

    private void OnTab(object sender, RoutedEventArgs e)
    {
        if (syncing || sender is not RadioButton { Tag: string tag }) return;
        vm.SelectTab(Enum.Parse<PanelTab>(tag));
        FocusDefault();
    }

    private void OnFilter(object sender, RoutedEventArgs e)
    {
        if (syncing || sender is not RadioButton { Tag: string tag }) return;
        vm.Filter = Enum.Parse<PanelFilter>(tag);
    }

    private void OnSearchChanged(object sender, TextChangedEventArgs e) =>
        SearchHint.Visibility = Search.Text.Length == 0 ? Visibility.Visible : Visibility.Collapsed;

    private void OnSettings(object sender, RoutedEventArgs e)
    {
        var item = new MenuItem { Header = "Notifications", IsCheckable = true, IsChecked = host.Notifications };
        item.Click += (_, _) => host.Notifications = item.IsChecked;
        var menu = new ContextMenu { PlacementTarget = SettingsButton, Placement = System.Windows.Controls.Primitives.PlacementMode.Bottom };
        menu.Items.Add(item);
        menu.IsOpen = true;
    }

    private void OnOpenWindow(object sender, RoutedEventArgs e) =>
        host.OpenWindow(vm.Items.FirstOrDefaultExpanded()?.Session.SessionId);

    private void OnClose(object sender, RoutedEventArgs e) => HideNow();

    private void OnQuit(object sender, RoutedEventArgs e) => host.Quit(fromSignal: false);

    private async void OnStartDaemon(object sender, RoutedEventArgs e)
    {
        try
        {
            await host.StartDaemonAsync();
        }
        catch (Exception ex)
        {
            TrayLog.Error("start-daemon", ex);
        }
    }
}

internal static class CardListExtensions
{
    public static CardViewModel? FirstOrDefaultExpanded(this System.Collections.Generic.IEnumerable<CardViewModel> cards)
    {
        foreach (var c in cards)
        {
            if (c.Session.IsExpanded) return c;
        }

        return null;
    }
}
