using System;
using System.Linq;
using System.Threading;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Input;
using System.Windows.Threading;
using SessionHub.Core.App;
using SessionHub.Core.Bridge;
using SessionHub.Core.Protocol;
using SessionHub.Core.Settings;
using SessionHub.Core.Tray;
using SessionHub.Core.ViewModels;
using Forms = System.Windows.Forms;

namespace SessionHub.Tray;

/// <summary>
/// Owns the tray icon, the bridge, the view models and the panel. Everything runs on the UI thread:
/// bridge events arrive through the dispatcher's synchronization context.
/// </summary>
[System.Diagnostics.CodeAnalysis.SuppressMessage("Design", "CA1001", Justification = "Lives as long as the app; ShutdownAsync disposes store, client, notifyIcon and icons, with a time limit a sync Dispose can not keep.")]
internal sealed class TrayController : IPanelHost, IWindowHost
{
    private static readonly TimeSpan RequestTimeout = TimeSpan.FromSeconds(15);
    private static readonly TimeSpan StartDaemonTimeout = TimeSpan.FromSeconds(30);
    // Below the 5 s that hub install/uninstall waits after the quit event before it kills the tray.
    private static readonly TimeSpan DisposeLimit = TimeSpan.FromSeconds(2.5);

    private readonly Application app;
    private readonly SettingsStore store;
    private readonly GuiSettings settings;
    private readonly BridgeClient client;
    private readonly IconRenderer icons = new();
    private readonly BalloonGate balloons = new();
    private readonly BalloonPlanner planner = new();
    private readonly Forms.NotifyIcon notifyIcon;
    private readonly DispatcherTimer clock;
    private readonly DispatcherTimer sendSweep;
    private int trayIconSize;
    private int saveFailing;
    private IconState? shownIcon;
    private int shownIconSize;
    private string? balloonSession;
    private PanelWindow? panel;
    private MainWindow? window;
    private bool quitting;

    public TrayController(Application app, string dataDir, BridgeFactory factory)
    {
        this.app = app;
        store = new SettingsStore(dataDir);
        store.Corrupt += (e, movedTo) =>
            TrayLog.Error($"settings-corrupt moved-to={(movedTo is null ? "none (rename failed)" : System.IO.Path.GetFileName(movedTo))}", e);
        settings = store.Load();
        balloons.Notifications = settings.Notifications;

        Hub = new HubViewModel(settings.Drafts, settings.DraftsSeen, savedDismissed: settings.Dismissed);
        Hub.DraftChanged += (_, _) => SaveSoon();
        Hub.DismissedChanged += () => Guard("dismissed", () =>
        {
            // A restored item is not news: move the balloon baseline without showing one.
            planner.Rebase(Hub.IsDismissed);
            SaveSoon();
            UpdateIcon();
            CommandManager.InvalidateRequerySuggested();
        });
        Panel = new PanelViewModel(Hub);
        Panel.ExchangeWanted += card => _ = FetchExchangeAsync(card);
        Window = new WindowViewModel(Panel);

        // A failed write keeps retrying inside the store; log the first failure of a run and warn once
        // in the panel footer until a write works again. Never Save from these handlers.
        store.SaveFailed += e =>
        {
            if (Interlocked.Exchange(ref saveFailing, 1) == 1) return;
            TrayLog.Error("settings-save", e);
            app.Dispatcher.BeginInvoke(() => Panel.SetSaveFailed(true));
        };
        store.Saved += () =>
        {
            if (Interlocked.Exchange(ref saveFailing, 0) == 1) app.Dispatcher.BeginInvoke(() => Panel.SetSaveFailed(false));
        };

        client = new BridgeClient(factory, new DispatcherSynchronizationContext(app.Dispatcher));
        Sends = new SendCoordinator(client.RequestAsync);
        client.Snapshot += OnSnapshot;
        client.SendChanged += u => Guard("send-event", () =>
        {
            Sends.OnSendChanged(u);
            CommandManager.InvalidateRequerySuggested();
        });
        client.HealthChanged += h => Guard("health", () =>
        {
            // The relay children die with the bridge: no open send will report again.
            if (h is BridgeHealth.Restarting or BridgeHealth.Failed) Sends.SettleAll();
            Panel.SetHealth(h);
            UpdateIcon();
        });

        notifyIcon = new Forms.NotifyIcon { Text = "Session Hub: starting", ContextMenuStrip = BuildMenu() };
        notifyIcon.MouseClick += (_, e) =>
        {
            if (e.Button == Forms.MouseButtons.Left) Guard("tray-click", TogglePanel);
        };
        notifyIcon.BalloonTipClicked += (_, _) => Guard("balloon-click", () => ShowPanel(balloonSession));

        // "… ago" texts only move while the panel or the window is open; nothing ticks while both are hidden.
        clock = new DispatcherTimer(TimeSpan.FromSeconds(15), DispatcherPriority.Background, (_, _) => Tick(), app.Dispatcher);
        clock.Stop();

        // Sends the relay stopped tracking (30 min) are settled; runs only while a send is open.
        sendSweep = new DispatcherTimer(TimeSpan.FromMinutes(1), DispatcherPriority.Background, (_, _) => Guard("send-sweep", SweepSends), app.Dispatcher);
        sendSweep.Stop();

        trayIconSize = Native.TrayIconSize();
        Microsoft.Win32.SystemEvents.DisplaySettingsChanged += OnDisplayChanged;
        Microsoft.Win32.SystemEvents.UserPreferenceChanged += OnPreferenceChanged;
        Microsoft.Win32.SystemEvents.PowerModeChanged += OnPowerModeChanged;
    }

    public HubViewModel Hub { get; }

    public PanelViewModel Panel { get; }

    /// <summary>The window's state; it outlives the window itself (closing drops only the view).</summary>
    public WindowViewModel Window { get; }

    public SendCoordinator Sends { get; }

    public bool Interactive => true;

    public WindowPlacement? Placement => settings.Window;

    public bool Notifications
    {
        get => settings.Notifications;
        set
        {
            settings.Notifications = value;
            balloons.Notifications = value;
            SaveSoon();
        }
    }

    public void Start()
    {
        UpdateIcon();
        notifyIcon.Visible = true;
        client.Start();
    }

    /// <summary>Opens the panel, on one session when <paramref name="sessionId"/> is given.</summary>
    public void ShowPanel(string? sessionId)
    {
        if (quitting) return;
        panel ??= new PanelWindow(this, Panel);
        if (sessionId is not null) Panel.FocusSession(sessionId);
        else Panel.Refresh(reorder: true);
        Tick();
        clock.Start();
        panel.ShowNearTray();
    }

    /// <summary>Opens the window (design 2), on a session when one is given. Only one window exists; a second call brings it back.</summary>
    public void OpenWindow(string? sessionId)
    {
        if (quitting) return;
        panel?.HideNow();
        window ??= new MainWindow(this, Window);
        Tick();
        clock.Start();
        window.ShowOn(sessionId);
    }

    public void PanelHidden()
    {
        if (window is null) clock.Stop();
    }

    void IWindowHost.WindowClosed()
    {
        window = null;
        if (panel is not { IsOpen: true }) clock.Stop();
    }

    void IWindowHost.SavePlacement(WindowPlacement placement)
    {
        settings.Window = placement;
        SaveSoon();
    }

    FocusResult IWindowHost.JumpTo(SessionViewModel session) => Native.FocusProcessWindow(session.Pid);

    Task<Reply> IWindowHost.RequestAsync(BridgeRequest request, TimeSpan timeout) => client.RequestAsync(request, timeout);

    void IWindowHost.FetchExchange(CardViewModel card) => _ = FetchExchangeAsync(card);

    public void Jump(SessionViewModel session)
    {
        var result = Native.FocusProcessWindow(session.Pid);
        if (result == FocusResult.Focused) panel?.HideNow();
        else panel?.ShowNotice(result == FocusResult.Denied
            ? $"Windows did not let Session Hub bring {session.Name} to the front."
            : $"No terminal window found for {session.Name}.");
    }

    public Task SendAsync(CardViewModel card, SendMode mode) => RunSendAsync(card, () => Sends.SendAsync(card, mode));

    /// <summary>One waiting item's own Send button.</summary>
    public Task SendItemAsync(CardViewModel card, string itemKey) => RunSendAsync(card, () => Sends.SendItemAsync(card, itemKey));

    private async Task RunSendAsync(CardViewModel card, Func<Task> send)
    {
        if (!Panel.CanSend) return;
        try
        {
            await send();
            if (Sends.IsWatching) sendSweep.Start();
        }
        catch (Exception e)
        {
            TrayLog.Error("send", e);
            card.ShowSendError("error", "the send could not start");
        }
        finally
        {
            CommandManager.InvalidateRequerySuggested();
        }
    }

    public async Task StartDaemonAsync()
    {
        if (!await TryStartDaemonAsync()) panel?.ShowNotice("The daemon did not start. Run 'hub daemon start' in a terminal to see why.");
    }

    public async Task<bool> TryStartDaemonAsync()
    {
        var reply = await client.RequestAsync(new StartDaemonRequest(0), StartDaemonTimeout);
        return reply.Ok && Json.DecodeStartDaemon(reply)?.Running == true;
    }

    /// <summary>
    /// Quits the tray. A user's quit asks first when a send of this app session is not final, because
    /// the relay ends with the bridge. The quit event (install/uninstall) never asks.
    /// </summary>
    public void Quit(bool fromSignal)
    {
        if (quitting) return;
        if (!fromSignal && Sends.HasPending)
        {
            var answer = MessageBox.Show(
                "A message is still being sent. Quit anyway?",
                "Session Hub",
                MessageBoxButton.YesNo,
                MessageBoxImage.Warning,
                MessageBoxResult.No);
            // The prompt runs a nested message loop: the quit event may have started the shutdown meanwhile.
            if (answer != MessageBoxResult.Yes || quitting) return;
        }

        quitting = true;
        _ = ShutdownAsync();
    }

    private async Task ShutdownAsync()
    {
        clock.Stop();
        sendSweep.Stop();
        Microsoft.Win32.SystemEvents.DisplaySettingsChanged -= OnDisplayChanged;
        Microsoft.Win32.SystemEvents.UserPreferenceChanged -= OnPreferenceChanged;
        Microsoft.Win32.SystemEvents.PowerModeChanged -= OnPowerModeChanged;
        panel?.HideNow();
        window?.Close();
        notifyIcon.Visible = false;
        try
        {
            settings.Drafts = Hub.Drafts.ToDictionary();
            settings.DraftsSeen = Hub.DraftsSeen.ToDictionary();
            settings.Dismissed = [.. Hub.Dismissed];
            store.Save(settings);
            store.Dispose();
        }
        catch (Exception e)
        {
            TrayLog.Error("settings-flush", e);
        }

        try
        {
            await Task.WhenAny(client.DisposeAsync().AsTask(), Task.Delay(DisposeLimit));
        }
        catch (Exception e)
        {
            TrayLog.Error("bridge-dispose", e);
        }

        notifyIcon.Dispose();
        icons.Dispose();
        app.Shutdown(0);
    }

    private void OnSnapshot(HubSnapshot snapshot) => Guard("snapshot", () =>
    {
        var now = DateTimeOffset.Now;
        Panel.Apply(snapshot, now);
        // Both flags are taken (no short circuit): each is reset when read.
        if (Hub.TakeSeenSaveDue() | Hub.TakeDismissedSaveDue()) SaveSoon();
        Window.Refresh(reorder: false);
        UpdateIcon();
        CommandManager.InvalidateRequerySuggested(); // a session that ended can no longer Jump
        // At most one balloon per snapshot (several sessions share one). ShowBalloonTip throws on an
        // empty text, so the planner's default line stands in for one.
        foreach (var note in planner.Plan(snapshot, balloons, now, Hub.IsDismissed))
        {
            balloonSession = note.SessionId;
            var text = string.IsNullOrWhiteSpace(note.Text) ? BalloonPlanner.DefaultText : note.Text;
            // Windows cuts balloon titles at 63 and texts at 255 characters.
            notifyIcon.ShowBalloonTip(10_000, IconState.Truncate(note.Title, 63), IconState.Truncate(text, 255), Forms.ToolTipIcon.None);
        }
    });

    private async Task FetchExchangeAsync(CardViewModel card)
    {
        try
        {
            var reply = await client.RequestAsync(new HistoryRequest(0, card.Session.SessionId, 2), RequestTimeout);
            if (reply.Ok && Json.DecodeHistory(reply) is { } history) card.SetExchange(history.Messages, TimeZoneInfo.Local);
        }
        catch (Exception e)
        {
            TrayLog.Error("history", e);
        }
    }

    private void Tick()
    {
        Panel.Tick(DateTimeOffset.Now);
        Window.Tick();
    }

    private void TogglePanel()
    {
        if (panel is { IsOpen: true }) panel.HideNow();
        else if (panel is null || !panel.JustHidden) ShowPanel(null);
    }

    private void UpdateIcon()
    {
        var health = client.Health ?? new BridgeHealth.Restarting(0, TimeSpan.Zero);
        var totals = Hub.EffectiveTotals;
        var state = IconState.From(Hub.Meta, totals, health, Hub.HasSnapshot);
        if (state != shownIcon || trayIconSize != shownIconSize)
        {
            notifyIcon.Icon = icons.Get(state, trayIconSize);
            shownIcon = state;
            shownIconSize = trayIconSize;
        }

        var tip = IconState.Truncate(IconState.Tooltip(Hub.Meta, totals, health, Hub.Sessions, Hub.HasSnapshot));
        if (notifyIcon.Text != tip) notifyIcon.Text = tip;
    }

    private void SweepSends()
    {
        Sends.SettleExpired();
        if (!Sends.IsWatching) sendSweep.Stop();
        CommandManager.InvalidateRequerySuggested();
    }

    // After sleep the bridge was suspended as well: its silence is not a hang (see NotifyResumed).
    private void OnPowerModeChanged(object? sender, Microsoft.Win32.PowerModeChangedEventArgs e)
    {
        if (e.Mode == Microsoft.Win32.PowerModes.Resume) client.NotifyResumed();
    }

    // A scale or monitor change can change the tray icon size; it is read here, not per snapshot.
    private void OnDisplayChanged(object? sender, EventArgs e) =>
        app.Dispatcher.BeginInvoke(() => Guard("display-changed", RefreshIconSize));

    private void OnPreferenceChanged(object? sender, Microsoft.Win32.UserPreferenceChangedEventArgs e)
    {
        if (e.Category is Microsoft.Win32.UserPreferenceCategory.Desktop or Microsoft.Win32.UserPreferenceCategory.Window)
            app.Dispatcher.BeginInvoke(() => Guard("display-changed", RefreshIconSize));
    }

    private void RefreshIconSize()
    {
        trayIconSize = Native.TrayIconSize();
        UpdateIcon();
    }

    private Forms.ContextMenuStrip BuildMenu()
    {
        var menu = new Forms.ContextMenuStrip();
        menu.Items.Add("Open panel", null, (_, _) => Guard("menu-panel", () => ShowPanel(null)));
        menu.Items.Add("Open window", null, (_, _) => Guard("menu-window", () => OpenWindow(null)));
        menu.Items.Add(new Forms.ToolStripSeparator());
        menu.Items.Add("Quit", null, (_, _) => Guard("menu-quit", () => Quit(fromSignal: false)));
        return menu;
    }

    /// <summary>
    /// Writes drafts and last-seen times now. For <c>App.OnExit</c>, which also runs when Windows ends
    /// the session without a Quit; after a Quit the store is disposed and this does nothing.
    /// </summary>
    public void FlushSettings()
    {
        try
        {
            settings.Drafts = Hub.Drafts.ToDictionary();
            settings.DraftsSeen = Hub.DraftsSeen.ToDictionary();
            settings.Dismissed = [.. Hub.Dismissed];
            store.Save(settings);
        }
        catch (Exception e)
        {
            TrayLog.Error("settings-exit-flush", e);
        }
    }

    private void SaveSoon()
    {
        settings.Drafts = Hub.Drafts.ToDictionary();
        settings.DraftsSeen = Hub.DraftsSeen.ToDictionary();
        settings.Dismissed = [.. Hub.Dismissed];
        store.ScheduleSave(settings);
    }

    /// <summary>A handler that throws is logged and never ends the bridge or the tray.</summary>
    private static void Guard(string where, Action action)
    {
        try
        {
            action();
        }
        catch (Exception e)
        {
            TrayLog.Error(where, e);
        }
    }
}
