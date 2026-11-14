using System;
using System.Collections.Generic;
using System.IO;
using System.Linq;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Media;
using System.Windows.Media.Imaging;
using System.Windows.Threading;
using SessionHub.Core.App;
using SessionHub.Core.Bridge;
using SessionHub.Core.Protocol;
using SessionHub.Core.Tray;
using SessionHub.Core.ViewModels;

namespace SessionHub.Tray;

/// <summary>
/// Hidden switch <c>--render-check &lt;dir&gt;</c>: renders the panel from the fixture snapshot to PNG
/// (Sessions with a card open, Home, Send, daemon stopped) at 96, 144 and 192 DPI in both themes, a card
/// with the answer form at 96 and 144 DPI,
/// the window (Status tab) at 720, 1000 and 1400 DIP wide at the same DPIs and themes, the window's
/// History, Send, answer-form and ended-session views at two sizes, the plain reply box, and the tray icon in every state at 16, 20,
/// 24 and 32 px; writes <c>render-check.txt</c> with the file list, and exits. No bridge, no tray icon,
/// no settings: nothing outside the output folder is touched. History rows are synthetic.
/// </summary>
internal static class RenderCheck
{
    private static readonly int[] Dpis = [96, 144, 192];
    private static readonly int[] WindowWidths = [720, 1000, 1400];

    public static int Run(Application app, ThemeManager theme, string outDir)
    {
        var report = new List<string>();
        try
        {
            Directory.CreateDirectory(outDir);
            var lines = ReadFixture("snapshot.ndjson");
            var snapshot = ((SnapshotEvent)Json.ParseEvent(lines[0])!).Snapshot;
            var stopped = ((SnapshotEvent)Json.ParseEvent(lines[1])!).Snapshot;
            var history = Json.DecodeHistory((Reply)Json.ParseEvent(ReadFixture("exchange.ndjson")[0])!)!;
            var now = DateTimeOffset.FromUnixTimeMilliseconds(snapshot.Sessions.Max(s => s.UpdatedAt) + 40_000);
            var answers = WithQuestions(snapshot);

            foreach (var light in new[] { false, true })
            {
                theme.Apply(light);
                foreach (var scenario in new[] { "sessions", "home", "send", "stopped" })
                {
                    foreach (var dpi in Dpis)
                    {
                        var file = Path.Combine(outDir, $"panel-{scenario}-{(light ? "light" : "dark")}-{dpi}.png");
                        Render(app, scenario == "stopped" ? stopped : snapshot, history, now, scenario, dpi, file);
                        report.Add(file);
                    }
                }

                // The answer form in an expanded card, with synthetic questions: one answered in the terminal,
                // one dismissed, one filled box.
                foreach (var dpi in new[] { 96, 144 })
                {
                    var file = Path.Combine(outDir, $"panel-answers-{(light ? "light" : "dark")}-{dpi}.png");
                    Render(app, answers, history, now, "answers", dpi, file);
                    report.Add(file);
                }
            }

            foreach (var light in new[] { false, true })
            {
                theme.Apply(light);
                var tone = light ? "light" : "dark";
                foreach (var width in WindowWidths)
                {
                    foreach (var dpi in Dpis)
                    {
                        var file = Path.Combine(outDir, $"window-status-{width}-{tone}-{dpi}.png");
                        RenderWindow(app, snapshot, history, now, WindowTab.Status, width, dpi, ended: false, file);
                        report.Add(file);
                    }
                }

                foreach (var (tab, width, dpi, name) in new[]
                {
                    (WindowTab.History, 1000, 96, "history"),
                    (WindowTab.History, 720, 144, "history"),
                    (WindowTab.Send, 1000, 96, "send"),
                    (WindowTab.Send, 720, 192, "send"),
                })
                {
                    var file = Path.Combine(outDir, $"window-{name}-{width}-{tone}-{dpi}.png");
                    RenderWindow(app, snapshot, history, now, tab, width, dpi, ended: false, file);
                    report.Add(file);
                }

                // The Send tab's answer form (answered, dismissed, filled and open items), the same session's
                // Status tab, and the plain reply box of a session nothing waits on.
                foreach (var (width, dpi) in new[] { (1000, 96), (720, 144) })
                {
                    var file = Path.Combine(outDir, $"window-answers-{width}-{tone}-{dpi}.png");
                    RenderWindow(app, answers, history, now, WindowTab.Send, width, dpi, ended: false, file, fillAnswers: true, height: 1180);
                    report.Add(file);
                }

                var answersStatus = Path.Combine(outDir, $"window-answers-status-1000-{tone}-96.png");
                RenderWindow(app, answers, history, now, WindowTab.Status, 1000, 96, ended: false, answersStatus, fillAnswers: true, height: 800);
                report.Add(answersStatus);

                var replyFile = Path.Combine(outDir, $"window-reply-1000-{tone}-96.png");
                RenderWindow(app, snapshot, history, now, WindowTab.Send, 1000, 96, ended: false, replyFile, pickIdle: true);
                report.Add(replyFile);

                // Daemon stopped with no sessions (banner, empty state), and an ended session with its group open.
                var stoppedFile = Path.Combine(outDir, $"window-stopped-1000-{tone}-96.png");
                RenderWindow(app, stopped, history, now, WindowTab.Status, 1000, 96, ended: false, stoppedFile);
                report.Add(stoppedFile);
                var endedFile = Path.Combine(outDir, $"window-ended-1000-{tone}-144.png");
                RenderWindow(app, snapshot, history, now, WindowTab.Status, 1000, 144, ended: true, endedFile);
                report.Add(endedFile);
            }

            foreach (var (kind, badge) in new[] { (IconKind.Running, ""), (IconKind.Waiting, "3"), (IconKind.Waiting, "99+"), (IconKind.Stopped, ""), (IconKind.Error, "") })
            {
                foreach (var size in new[] { 16, 20, 24, 32 })
                {
                    var file = Path.Combine(outDir, $"icon-{kind.ToString().ToLowerInvariant()}{(badge == "99+" ? "-99" : "")}-{size}.png");
                    using var bmp = IconRenderer.Draw(kind, badge, size);
                    bmp.Save(file, System.Drawing.Imaging.ImageFormat.Png);
                    report.Add(file);
                }
            }

            File.WriteAllLines(Path.Combine(outDir, "render-check.txt"), report);
            return 0;
        }
        catch (Exception e)
        {
            report.Add($"FAILED: {e.GetType().FullName}: {e.Message}");
            report.Add(e.StackTrace ?? "");
            try
            {
                File.WriteAllLines(Path.Combine(outDir, "render-check.txt"), report);
            }
            catch (IOException)
            {
                // Nothing else to report to.
            }

            return 1;
        }
    }

    private static void Render(Application app, HubSnapshot snapshot, HistoryResult history, DateTimeOffset now, string scenario, int dpi, string file)
    {
        var vm = new PanelViewModel(new HubViewModel());
        vm.SetHealth(new BridgeHealth.Connected());
        vm.Apply(snapshot, now);
        switch (scenario)
        {
            case "home":
            case "stopped":
                vm.SelectTab(PanelTab.Home);
                break;
            case "send":
                vm.SelectTab(PanelTab.Send);
                vm.SendTarget!.Session.Draft = "Status please. Keep it short.";
                vm.SendTarget.ShowSend("pending", null);
                break;
            case "answers":
                vm.SelectTab(PanelTab.Sessions);
                var asked = vm.Items.First(c => c.Answers.IsActive);
                vm.Toggle(asked);
                FillAnswers(asked);
                break;
            default:
                vm.SelectTab(PanelTab.Sessions);
                var card = vm.Items[0];
                vm.Toggle(card);
                card.SetExchange(history.Messages, TimeZoneInfo.Utc);
                card.Session.Draft = "Approve Q1 with option A.";
                card.ShowSendError("send-in-flight", null);
                break;
        }

        var window = new PanelWindow(new Host(), vm)
        {
            WindowStartupLocation = WindowStartupLocation.Manual,
            Left = -32000,
            Top = -32000,
            ShowActivated = false,
        };
        window.Show();
        window.UpdateLayout();
        app.Dispatcher.Invoke(static () => { }, DispatcherPriority.ApplicationIdle);
        window.UpdateLayout();

        var scale = dpi / 96.0;
        var bitmap = new RenderTargetBitmap(
            (int)Math.Ceiling(window.ActualWidth * scale), (int)Math.Ceiling(window.ActualHeight * scale), dpi, dpi, PixelFormats.Pbgra32);
        bitmap.Render(window);
        var encoder = new PngBitmapEncoder();
        encoder.Frames.Add(BitmapFrame.Create(bitmap));
        using (var stream = File.Create(file)) encoder.Save(stream);
        window.Close();
    }

    private static void RenderWindow(
        Application app, HubSnapshot snapshot, HistoryResult exchange, DateTimeOffset now, WindowTab tab, int width, int dpi, bool ended, string file,
        bool fillAnswers = false, bool pickIdle = false, int height = 620)
    {
        var panelVm = new PanelViewModel(new HubViewModel());
        panelVm.SetHealth(new BridgeHealth.Connected());
        panelVm.Apply(snapshot, now);
        var vm = new WindowViewModel(panelVm);
        vm.Refresh(reorder: true);
        // The ended scenario shows an ended session (its group opened) when the snapshot has one.
        var first = snapshot.Sessions.Count > 0 ? snapshot.Sessions[0] : null;
        var pick = ended
            ? snapshot.Sessions.FirstOrDefault(s => s.EndedAt is not null) ?? first
            : pickIdle
                ? snapshot.Sessions.FirstOrDefault(s => s.EndedAt is null && s.WaitingCount == 0 && s.State == "idle") ?? first
                : snapshot.Sessions.FirstOrDefault(s => s.Status is not null) ?? first;
        if (pick is not null) vm.Select(pick.SessionId);
        vm.Tab = tab;
        if (fillAnswers && vm.Selected is { } filled)
        {
            FillAnswers(filled);
        }
        else if (tab == WindowTab.Send && vm.Selected is { } card)
        {
            card.Session.Draft = "Status please. Keep it short.";
            card.ShowSend("pending", null);
        }

        var window = new MainWindow(new WindowHost(exchange), vm)
        {
            Left = -32000,
            Top = -32000,
            Width = width,
            Height = height,
            ShowActivated = false,
            ShowInTaskbar = false,
        };
        window.Show();
        window.UpdateLayout();
        app.Dispatcher.Invoke(static () => { }, DispatcherPriority.ApplicationIdle);
        if (tab == WindowTab.History) window.HistoryControl.MessageScroller?.ScrollToEnd();
        window.UpdateLayout();
        app.Dispatcher.Invoke(static () => { }, DispatcherPriority.ApplicationIdle);
        window.UpdateLayout();

        var scale = dpi / 96.0;
        var bitmap = new RenderTargetBitmap(
            (int)Math.Ceiling(window.ActualWidth * scale), (int)Math.Ceiling(window.ActualHeight * scale), dpi, dpi, PixelFormats.Pbgra32);
        bitmap.Render(window);
        var encoder = new PngBitmapEncoder();
        encoder.Frames.Add(BitmapFrame.Create(bitmap));
        using (var stream = File.Create(file)) encoder.Save(stream);
        window.Close();
    }

    /// <summary>
    /// The fixture with synthetic "Waiting on you" items on its first live session with a status: IDs,
    /// a hyphenated one, one answered in the terminal (DB-Q1, so the daemon counts three), and one
    /// without an ID that has sub-items; and one relay message that reached the wrong session.
    /// </summary>
    private static HubSnapshot WithQuestions(HubSnapshot snapshot)
    {
        WaitingItem[] questions =
        [
            new("AUTH-Q1", "Approve the auth direction (harden the current tokens first, a new identity server later only if needed)?", true, []),
            new("DB-Q1", "Stay on the current database and reduce the lock-in as we go, or start the per-service migration now?", false, [], Answered: true),
            new("OPS-Q3", "OK to ask the three vendors for their maintenance windows? Who is the contact for each?", false, []),
            new(null, "Pick a date for the cutover window.", false, ["Weekday after 22:00", "Weekend morning"]),
        ];
        var target = snapshot.Sessions.First(s => s.Status is not null && s.EndedAt is null);
        var sessions = snapshot.Sessions
            .Select(s => s.SessionId == target.SessionId
                ? s with { WaitingCount = questions.Count(q => !q.Answered), Status = s.Status! with { Waiting = questions } }
                : s)
            .ToList();
        return snapshot with { Sessions = sessions, Meta = snapshot.Meta with { RelayStrayCount = 1 } };
    }

    /// <summary>AUTH-Q1 filled (open), OPS-Q3 dismissed; DB-Q1 is answered by the snapshot, the last item stays open.</summary>
    private static void FillAnswers(CardViewModel card)
    {
        card.Answers.Items[0].Answer = "Yes, harden the current tokens first.";
        card.Answers.Items[2].Dismiss();
    }

    /// <summary>Synthetic history (no real data): messages of mixed length, the third newest marked shortened.</summary>
    internal static Reply SyntheticHistory(string sessionId, int count, DateTimeOffset end)
    {
        var rows = new List<object>();
        for (var i = 0; i < count; i++)
        {
            var owner = i % 2 == 0;
            var text = owner
                ? $"Question {i}: what is the state of step {i / 2}?"
                : $"Step {i / 2} is done. " + string.Concat(Enumerable.Repeat("The tests pass and the build is clean. ", 1 + (i % 5)));
            rows.Add(new
            {
                uuid = $"syn-{i}",
                sessionId,
                ts = end.AddMinutes(i - count).UtcDateTime.ToString("yyyy-MM-ddTHH:mm:ss.fffZ", System.Globalization.CultureInfo.InvariantCulture),
                role = owner ? "owner" : "assistant",
                fromName = (string?)null,
                text,
                textTruncated = i == count - 3,
            });
        }

        var json = System.Text.Json.JsonSerializer.Serialize(new { messages = rows, truncated = false, more = true });
        return new Reply(1, true, System.Text.Json.JsonDocument.Parse(json).RootElement.Clone());
    }

    private static string[] ReadFixture(string name)
    {
        using var stream = typeof(RenderCheck).Assembly.GetManifestResourceStream("Fixtures." + name)
            ?? throw new InvalidOperationException("fixture missing: " + name);
        using var reader = new StreamReader(stream);
        return reader.ReadToEnd().Split('\n', StringSplitOptions.RemoveEmptyEntries);
    }

    /// <summary>Answers history with synthetic rows; never saves a placement or starts anything.</summary>
    private sealed class WindowHost(HistoryResult exchange) : IWindowHost
    {
        public bool Interactive => false;

        public SessionHub.Core.Settings.WindowPlacement? Placement => null;

        public void SavePlacement(SessionHub.Core.Settings.WindowPlacement placement)
        {
        }

        public FocusResult JumpTo(SessionViewModel session) => FocusResult.NoWindow;

        public Task SendAsync(CardViewModel card, SendMode mode) => Task.CompletedTask;

        public Task SendItemAsync(CardViewModel card, string itemKey) => Task.CompletedTask;

        public Task<bool> TryStartDaemonAsync() => Task.FromResult(false);

        public Task<Reply> RequestAsync(BridgeRequest request, TimeSpan timeout) => Task.FromResult(
            request is HistoryRequest h
                ? SyntheticHistory(h.SessionId, 40, DateTimeOffset.FromUnixTimeMilliseconds(1_763_000_000_000))
                : new Reply(1, false, null, new BridgeError("render-check", "no bridge")));

        public void FetchExchange(CardViewModel card) => card.SetExchange(exchange.Messages, TimeZoneInfo.Utc);

        public void WindowClosed()
        {
        }
    }

    /// <summary>Does nothing; the render never hides on deactivate.</summary>
    private sealed class Host : IPanelHost
    {
        public bool Notifications { get; set; } = true;

        public bool Interactive => false;

        public void Jump(SessionViewModel session)
        {
        }

        public Task SendAsync(CardViewModel card, SendMode mode) => Task.CompletedTask;

        public Task SendItemAsync(CardViewModel card, string itemKey) => Task.CompletedTask;

        public Task StartDaemonAsync() => Task.CompletedTask;

        public void OpenWindow(string? sessionId)
        {
        }

        public void Quit(bool fromSignal)
        {
        }

        public void PanelHidden()
        {
        }
    }
}
