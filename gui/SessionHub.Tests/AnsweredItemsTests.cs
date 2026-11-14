using System.Text.Json;
using SessionHub.Core.App;
using SessionHub.Core.Bridge;
using SessionHub.Core.Protocol;
using SessionHub.Core.Settings;
using SessionHub.Core.Tray;
using SessionHub.Core.ViewModels;
using static SessionHub.Tests.TestData;

namespace SessionHub.Tests;

/// <summary>Answered items (from the daemon), dismissed items (from the tray), per-item Send and the stray relay count.</summary>
public class AnsweredProtocolTests
{
    private static HubSnapshot Parse(string waitingJson, string metaExtra = "")
    {
        var line =
            "{\"type\":\"snapshot\",\"seq\":1,\"snapshot\":{\"sessions\":[{\"sessionId\":\"s1\",\"profile\":\"claude\",\"name\":\"a\","
            + "\"state\":\"idle\",\"waitingCount\":1,\"status\":{\"remaining\":[],\"recommendations\":[],\"replyUuid\":\"r1\",\"replyTs\":\"t\","
            + "\"waiting\":" + waitingJson + "}}],"
            + "\"meta\":{\"unknownCount\":0,\"stale\":[],\"daemon\":\"running\"" + metaExtra + "},"
            + "\"totals\":{\"waitingItems\":1,\"needsApproval\":0,\"live\":1}}}";
        var ev = Json.ParseEvent(line, out var reason);
        Assert.True(ev is not null, reason);
        return ((SnapshotEvent)ev!).Snapshot;
    }

    [Fact]
    public void Answered_defaults_to_false_and_reads_true()
    {
        var s = Parse("[{\"id\":\"Q1\",\"text\":\"a\",\"isNew\":false,\"children\":[]},{\"id\":\"Q2\",\"text\":\"b\",\"isNew\":false,\"children\":[],\"answered\":true}]");
        var waiting = s.Sessions[0].Status!.Waiting;
        Assert.False(waiting[0].Answered);
        Assert.True(waiting[1].Answered);
    }

    [Fact]
    public void Relay_stray_count_defaults_to_zero_and_reads_a_number()
    {
        Assert.Equal(0, Parse("[]").Meta.RelayStrayCount);
        Assert.Equal(3, Parse("[]", ",\"relayStrayCount\":3").Meta.RelayStrayCount);
    }

    [Fact]
    public void The_bridge_fixture_carries_both_fields()
    {
        var lines = File.ReadAllLines(Path.Combine(AppContext.BaseDirectory, "Fixtures", "snapshot.ndjson")).Where(l => l.Length > 0).ToArray();
        var seeded = ((SnapshotEvent)Json.ParseEvent(lines[0])!).Snapshot;
        Assert.Equal(1, seeded.Meta.RelayStrayCount);
        Assert.All(seeded.Sessions.SelectMany(x => x.Status?.Waiting ?? []), w => Assert.False(w.Answered));
        Assert.Equal(0, ((SnapshotEvent)Json.ParseEvent(lines[1])!).Snapshot.Meta.RelayStrayCount);
    }

    [Fact]
    public void A_new_stray_count_is_a_meta_change()
    {
        var hub = new HubViewModel();
        hub.Apply(Snap(Session("s1")));
        var before = hub.Meta;
        hub.Apply(Snap(Session("s1")) with { Meta = Meta() with { RelayStrayCount = 1 } });
        Assert.NotSame(before, hub.Meta);
        Assert.Equal(1, hub.Meta.RelayStrayCount);
    }
}

public class AnsweredFormTests
{
    // DB-Q1 was answered in the terminal: the daemon counts two.
    internal static readonly WaitingItem[] Questions =
    [
        new("AUTH-Q1", "Approve the auth direction?", true, []),
        new("DB-Q1", "Stay on the current database?", false, [], Answered: true),
        new(null, "Pick a date for the cutover window.", false, ["Weekday evening", "Weekend morning"]),
    ];

    internal static (HubViewModel Hub, CardViewModel Card) Card(
        IReadOnlyList<WaitingItem>? waiting = null, int waitingCount = 2, IEnumerable<string>? dismissed = null, string reply = "u")
    {
        var hub = new HubViewModel(savedDismissed: dismissed);
        hub.Apply(Snap(WithReply(Session("s1", name: "alpha", waitingCount: waitingCount, waiting: waiting ?? Questions), reply)));
        return (hub, new CardViewModel(hub.Sessions[0]));
    }

    internal static SessionView WithReply(SessionView v, string reply) => v with { Status = v.Status! with { ReplyUuid = reply } };

    [Fact]
    public void An_answered_item_is_tagged_and_left_out_of_the_total()
    {
        var (_, c) = Card();
        var f = c.Answers;
        Assert.True(f.IsActive);
        Assert.Equal(3, f.Items.Count);
        Assert.Equal(2, f.Total);
        Assert.True(f.Items[1].IsAnswered);
        Assert.True(f.Items[1].IsSettled);
        Assert.Equal("Answered", f.Items[1].TagText);
        Assert.Equal(AnswerItemViewModel.AnsweredToolTip, f.Items[1].TagToolTip);
        Assert.False(f.Items[1].CanDismiss);
        Assert.False(f.Items[1].CanUndo);
        Assert.Equal("", f.Items[0].TagText);
        Assert.Equal("0 of 2 answered · Ctrl+Enter sends", f.FooterText);

        // Its box stays usable, is still sent, but is not counted.
        f.Items[1].Answer = "Stay";
        Assert.Equal("0 of 2 answered · Ctrl+Enter sends", f.FooterText);
        Assert.Equal("Send answers", f.SendLabel);
        Assert.Equal("Answers:\nDB-Q1: Stay", f.Composed);
        f.Items[0].Answer = "Yes";
        Assert.Equal("1 of 2 answered · Ctrl+Enter sends", f.FooterText);
    }

    [Fact]
    public void A_later_snapshot_marks_a_kept_item_answered()
    {
        var (hub, c) = Card();
        var auth = c.Answers.Items[0];
        WaitingItem[] next = [Questions[0] with { Answered = true }, Questions[1], Questions[2]];
        hub.Apply(Snap(WithReply(Session("s1", name: "alpha", waitingCount: 1, waiting: next), "u")));
        Assert.Same(auth, c.Answers.Items[0]);
        Assert.True(auth.IsAnswered);
        Assert.Equal(1, c.Answers.Total);
    }

    [Fact]
    public void Answered_items_alone_give_no_amber_edge()
    {
        WaitingItem[] all = [Questions[1], Questions[0] with { Answered = true }];
        var (_, c) = Card(waiting: all, waitingCount: 0);
        Assert.Equal(CardEdge.None, c.Edge);
        Assert.Null(c.ListBadge);
        Assert.False(c.HasOpenWaiting);
        Assert.True(c.HasWaiting);
        Assert.Equal(SessionGroup.Idle, c.Session.Group);
        Assert.Equal("All items answered or dismissed · Ctrl+Enter sends", c.Answers.FooterText);
    }

    [Fact]
    public void Dismiss_is_counted_like_answered_and_undo_brings_it_back()
    {
        var (hub, c) = Card();
        var changes = 0;
        hub.DismissedChanged += () => changes++;
        var auth = c.Answers.Items[0];
        Assert.True(auth.CanDismiss);

        auth.Dismiss();
        Assert.True(auth.IsDismissed);
        Assert.True(auth.IsSettled);
        Assert.Equal("Dismissed", auth.TagText);
        Assert.True(auth.CanUndo);
        Assert.False(auth.CanDismiss);
        Assert.Equal(1, c.Answers.Total);
        Assert.Equal(1, c.Session.EffectiveWaitingCount);
        Assert.Equal(2, c.Session.WaitingCount);
        Assert.Equal("1 waiting on you", c.StatusChip.Text);
        Assert.Equal("1", c.ListBadge!.Text);
        Assert.Equal(["s1#AUTH-Q1@u"], hub.Dismissed);
        Assert.Equal(1, changes);

        auth.Undo();
        Assert.False(auth.IsDismissed);
        Assert.Equal(2, c.Session.EffectiveWaitingCount);
        Assert.Equal("2 waiting on you", c.StatusChip.Text);
        Assert.Empty(hub.Dismissed);
        Assert.Equal(2, changes);
    }

    [Fact]
    public void Dismissing_every_open_item_clears_the_edge_and_the_waiting_group()
    {
        var (hub, c) = Card();
        c.Answers.Items[0].Dismiss();
        c.Answers.Items[2].Dismiss();
        Assert.Equal(0, c.Session.EffectiveWaitingCount);
        Assert.Equal(CardEdge.None, c.Edge);
        Assert.False(c.HasOpenWaiting);
        Assert.Equal(SessionGroup.Idle, c.Session.Group);
        Assert.Equal(0, hub.EffectiveTotals.WaitingItems);
        Assert.Equal(2, hub.Totals.WaitingItems);
        Assert.Contains("s1#" + Keys.NoId(Questions[2].Text) + "@u", hub.Dismissed);
    }

    [Fact]
    public void Dismissing_an_answered_item_takes_nothing_more_off()
    {
        var (_, c) = Card();
        c.Session.SetDismissed("DB-Q1", true);
        Assert.Equal(2, c.Session.EffectiveWaitingCount);
        Assert.Equal("Answered", c.Answers.Items[1].TagText);
    }

    [Fact]
    public void A_new_status_reply_clears_the_dismissals()
    {
        var (hub, c) = Card();
        c.Answers.Items[0].Dismiss();
        Assert.True(hub.TakeDismissedSaveDue() is false);

        hub.Apply(Snap(WithReply(Session("s1", name: "alpha", waitingCount: 2, waiting: Questions), "u2")));
        Assert.False(c.Answers.Items[0].IsDismissed);
        Assert.Equal("", c.Answers.Items[0].TagText);
        Assert.Equal(2, c.Session.EffectiveWaitingCount);
        Assert.Empty(hub.Dismissed);
        Assert.True(hub.TakeDismissedSaveDue());
        Assert.False(hub.TakeDismissedSaveDue());
    }

    [Fact]
    public void Saved_dismissals_apply_only_to_their_session_and_reply()
    {
        var (hub, c) = Card(dismissed: ["s1#AUTH-Q1@u", "s1#" + Keys.NoId(Questions[2].Text) + "@old", "s2#AUTH-Q1@u", "junk"]);
        Assert.True(c.Answers.Items[0].IsDismissed);
        Assert.False(c.Answers.Items[2].IsDismissed);
        Assert.Equal(1, c.Session.EffectiveWaitingCount);
        // The old reply's key is pruned; s2 is gone from a trusted snapshot; the junk key never loaded.
        Assert.Equal(["s1#AUTH-Q1@u"], hub.Dismissed);
        Assert.True(hub.TakeDismissedSaveDue());
    }

    [Fact]
    public void An_untrusted_snapshot_keeps_dismissals_of_missing_sessions()
    {
        var hub = new HubViewModel(savedDismissed: ["s2#Q1@r"]);
        hub.Apply(Snap(Session("s1")) with { Meta = Meta("stopped") });
        Assert.Equal(["s2#Q1@r"], hub.Dismissed);
        hub.Apply(Snap(Session("s1")));
        Assert.Empty(hub.Dismissed);
    }

    [Fact]
    public void Dismissals_are_bounded_oldest_first()
    {
        var keys = Enumerable.Range(0, GuiSettings.MaxDismissed + 5).Select(i => $"s9#Q{i}@r").ToList();
        var hub = new HubViewModel(savedDismissed: keys);
        Assert.Equal(GuiSettings.MaxDismissed, hub.Dismissed.Count);
        Assert.Equal("s9#Q5@r", hub.Dismissed[0]);
    }

    [Fact]
    public void Dismiss_keys_split_on_the_first_hash_and_the_last_at()
    {
        var key = AnswerComposer.DismissKey("s1", "text@1a2b3c4d", "r-1");
        Assert.Equal("s1#text@1a2b3c4d@r-1", key);
        Assert.True(AnswerComposer.TryParseDismissKey(key, out var sid, out var label, out var reply));
        Assert.Equal(("s1", "text@1a2b3c4d", "r-1"), (sid, label, reply));
        Assert.False(AnswerComposer.TryParseDismissKey("s1#Q1", out _, out _, out _));
        Assert.False(AnswerComposer.TryParseDismissKey("#Q1@r", out _, out _, out _));
        Assert.False(AnswerComposer.TryParseDismissKey("s1#Q1@", out _, out _, out _));
        Assert.False(AnswerComposer.TryParseDismissKey(null, out _, out _, out _));
    }

    [Fact]
    public void Compose_one_uses_the_items_own_label()
    {
        Assert.Equal("Answers:\nItem 3: Weekend\n  morning", AnswerComposer.ComposeOne("Item 3", " Weekend\r\nmorning "));
        Assert.Equal("", AnswerComposer.ComposeOne("Q1", "   "));
        var (_, c) = Card();
        c.Answers.Items[2].Answer = "Weekend";
        Assert.Equal("Answers:\nItem 3: Weekend", c.Answers.Items[2].SendText);
    }

    [Fact]
    public void Per_item_can_send_needs_a_filled_box_within_the_limit_and_a_live_session()
    {
        var (hub, c) = Card();
        var auth = c.Answers.Items[0];
        Assert.False(auth.CanSend);
        auth.Answer = "  ";
        Assert.False(auth.CanSend);
        auth.Answer = "Yes";
        Assert.True(auth.CanSend);
        auth.Answer = new string('a', AnswerComposer.MaxChars);
        Assert.False(auth.CanSend);
        auth.Answer = "Yes";

        var raised = new List<string?>();
        auth.PropertyChanged += (_, e) => raised.Add(e.PropertyName);
        hub.Apply(Snap(WithReply(Session("s1", name: "alpha", waitingCount: 2, endedAt: 9, waiting: Questions), "u")));
        Assert.False(auth.CanSend);
        Assert.Contains(nameof(AnswerItemViewModel.CanSend), raised);
    }

    [Fact]
    public void Settings_keep_dismissals_deduplicated_and_bounded()
    {
        var dir = Path.Combine(Path.GetTempPath(), "sh-dismiss-" + Guid.NewGuid().ToString("N"));
        try
        {
            using (var store = new SettingsStore(dir))
            {
                var keys = new List<string> { "s1#Q1@r", "s1#Q1@r", " ", "s1#" + new string('x', GuiSettings.MaxDismissedKeyChars) + "@r" };
                keys.AddRange(Enumerable.Range(0, GuiSettings.MaxDismissed).Select(i => $"s2#Q{i}@r"));
                store.Save(new GuiSettings { Dismissed = keys });
            }

            var back = new SettingsStore(dir).Load();
            Assert.Equal(GuiSettings.MaxDismissed, back.Dismissed.Count);
            Assert.DoesNotContain("s1#Q1@r", back.Dismissed);
            Assert.Equal("s2#Q0@r", back.Dismissed[0]);
            Assert.Empty(new SettingsStore(Path.Combine(dir, "none")).Load().Dismissed);
        }
        finally
        {
            if (Directory.Exists(dir)) Directory.Delete(dir, recursive: true);
        }
    }
}

public class EffectiveCountTests
{
    private static readonly BridgeHealth Ok = new BridgeHealth.Connected();

    [Fact]
    public void Icon_badge_and_tooltip_use_the_effective_count()
    {
        var (hub, c) = AnsweredFormTests.Card();
        c.Answers.Items[0].Dismiss();
        var totals = hub.EffectiveTotals;
        Assert.Equal(1, totals.WaitingItems);
        Assert.Equal(new IconState(IconKind.Waiting, 1), IconState.From(hub.Meta, totals, Ok));
        Assert.Equal("Session Hub: alpha (1) waiting on you", IconState.Tooltip(hub.Meta, totals, Ok, hub.Sessions));

        c.Answers.Items[2].Dismiss();
        totals = hub.EffectiveTotals;
        Assert.Equal(new IconState(IconKind.Running, 0), IconState.From(hub.Meta, totals, Ok));
        Assert.StartsWith("Session Hub: 1 live session, nothing waiting", IconState.Tooltip(hub.Meta, totals, Ok, hub.Sessions));
    }

    [Fact]
    public void Panel_badge_tabs_and_window_groups_follow_a_dismissal_at_once()
    {
        var hub = new HubViewModel();
        var panel = new PanelViewModel(hub);
        panel.SetHealth(Ok);
        panel.Apply(Snap(AnsweredFormTests.WithReply(Session("s1", name: "alpha", waitingCount: 2, waiting: AnsweredFormTests.Questions), "u")), DateTimeOffset.UnixEpoch);
        var window = new WindowViewModel(panel);
        window.Refresh(reorder: true);
        panel.SelectTab(PanelTab.Waiting);
        Assert.Equal(2, panel.WaitingTotal);
        Assert.Single(panel.Items);
        Assert.Equal("Waiting on you · 1", window.Groups[0].Header);
        Assert.Equal("2 waiting on you", window.WaitingChip);

        var card = panel.Card("s1")!;
        card.Answers.Items[0].Dismiss();
        card.Answers.Items[2].Dismiss();
        Assert.Equal(0, panel.WaitingTotal);
        Assert.Empty(panel.Items);
        Assert.Equal("Waiting on you · 0", window.Groups[0].Header);
        Assert.Equal("Idle · 1", window.Groups[2].Header);
        Assert.Equal("", window.WaitingChip);
    }

    [Fact]
    public void Stray_relay_warning_in_the_panel_and_the_window()
    {
        var panel = new PanelViewModel(new HubViewModel());
        panel.SetHealth(Ok);
        var window = new WindowViewModel(panel);
        panel.Apply(Snap(Session("s1")), DateTimeOffset.UnixEpoch);
        window.Refresh(reorder: false);
        Assert.Equal("", panel.RelayStrayText);
        Assert.Equal("", window.StrayText);

        panel.Apply(Snap(Session("s1")) with { Meta = Meta() with { RelayStrayCount = 1 } }, DateTimeOffset.UnixEpoch);
        window.Refresh(reorder: false);
        Assert.Equal("1 relay message reached the wrong session (see relay.log)", panel.RelayStrayText);
        Assert.Equal(" · 1 relay message reached the wrong session (see relay.log)", window.StrayText);

        panel.Apply(Snap(Session("s1")) with { Meta = Meta() with { RelayStrayCount = 2 } }, DateTimeOffset.UnixEpoch);
        Assert.Equal("2 relay messages reached the wrong session (see relay.log)", panel.RelayStrayText);
    }

    [Fact]
    public void Balloons_leave_out_dismissed_and_answered_items()
    {
        var gate = new BalloonGate();
        var planner = new BalloonPlanner();
        Func<string, string, bool> dismissed = (sid, label) => sid == "a" && label == "Q2";
        planner.Plan(Snap(Session("a", "alpha", waitingCount: 1, waiting: [new WaitingItem("Q1", "One", false, [])])), gate, DateTimeOffset.UnixEpoch, dismissed);

        // Q2 is dismissed and Q3 answered: Q4 is the newest open item; the daemon's 3 less one dismissed is 2.
        WaitingItem[] next =
        [
            new("Q1", "One", false, []),
            new("Q2", "Two", true, []),
            new("Q4", "Four", true, []),
            new("Q3", "Three", true, [], Answered: true),
        ];
        var due = planner.Plan(Snap(Session("a", "alpha", waitingCount: 3, waiting: next)), gate, DateTimeOffset.UnixEpoch.AddSeconds(1), dismissed);
        var b = Assert.Single(due);
        Assert.Equal("alpha is waiting on you (2)", b.Title);
        Assert.Equal("Q4: Four", b.Text);
    }

    [Fact]
    public void Undoing_a_dismissal_shows_no_balloon()
    {
        var gate = new BalloonGate();
        var planner = new BalloonPlanner();
        var isDismissed = true;
        Func<string, string, bool> dismissed = (sid, label) => isDismissed && sid == "a" && label == "Q2";
        WaitingItem[] items = [new("Q1", "One", false, []), new("Q2", "Two", false, [])];
        var snap = Snap(Session("a", "alpha", waitingCount: 2, waiting: items));
        planner.Plan(snap, gate, DateTimeOffset.UnixEpoch, dismissed);
        planner.Plan(snap, gate, DateTimeOffset.UnixEpoch.AddMinutes(1), dismissed);

        // Undo: Q2 counts again. The baseline moves with it, so the next snapshot is no rise.
        isDismissed = false;
        planner.Rebase(dismissed);
        Assert.Empty(planner.Plan(snap, gate, DateTimeOffset.UnixEpoch.AddMinutes(2), dismissed));

        // A real new item afterwards still shows one.
        WaitingItem[] more = [.. items, new("Q3", "Three", true, [])];
        var due = planner.Plan(Snap(Session("a", "alpha", waitingCount: 3, waiting: more)), gate, DateTimeOffset.UnixEpoch.AddMinutes(30), dismissed);
        Assert.Single(due);
    }
}

public class ItemSendTests
{
    private sealed class Bridge
    {
        private readonly Queue<Reply> replies = new();

        public List<SendRequest> Sent { get; } = [];

        public TaskCompletionSource? Gate { get; set; }

        public void Enqueue(Reply r) => replies.Enqueue(r);

        public async Task<Reply> Request(BridgeRequest r, TimeSpan _)
        {
            Sent.Add((SendRequest)r);
            if (Gate is { } gate) await gate.Task;
            return replies.Count > 0
                ? replies.Dequeue()
                : new Reply(1, true, JsonDocument.Parse("{\"sendId\":5,\"state\":\"pending\"}").RootElement.Clone());
        }
    }

    [Fact]
    public async Task One_items_send_carries_only_its_answer_and_clears_only_its_box()
    {
        var (hub, c) = AnsweredFormTests.Card();
        c.Answers.Items[0].Answer = "Yes";
        c.Answers.Items[2].Answer = "Weekend";
        c.Session.Draft = "and a message";
        var bridge = new Bridge();
        await new SendCoordinator(bridge.Request).SendItemAsync(c, c.Answers.Items[2].Key);

        Assert.Equal("Answers:\nItem 3: Weekend", bridge.Sent.Single().Text);
        Assert.Equal("", c.Answers.Items[2].Answer);
        Assert.Equal("Yes", c.Answers.Items[0].Answer);
        Assert.Equal("and a message", c.Session.Draft);
        Assert.Equal("Yes", hub.Drafts["s1#AUTH-Q1"]);
        Assert.Equal("Sending…", c.SendBox.StateText);
    }

    [Fact]
    public async Task An_edit_made_while_the_request_is_out_stays()
    {
        var (_, c) = AnsweredFormTests.Card();
        c.Answers.Items[0].Answer = "Yes";
        var bridge = new Bridge { Gate = new TaskCompletionSource() };
        var sends = new SendCoordinator(bridge.Request);
        var send = sends.SendItemAsync(c, "AUTH-Q1");
        Assert.True(c.IsRequesting);

        // Every Send of the card is blocked meanwhile (the main one ignores a second request too).
        await sends.SendAsync(c);
        Assert.Single(bridge.Sent);

        c.Answers.Items[0].Answer = "Yes, but later";
        bridge.Gate.SetResult();
        await send;
        Assert.False(c.IsRequesting);
        Assert.Equal("Yes, but later", c.Answers.Items[0].Answer);
    }

    [Fact]
    public async Task A_blank_or_unknown_item_sends_nothing()
    {
        var (_, c) = AnsweredFormTests.Card();
        var bridge = new Bridge();
        var sends = new SendCoordinator(bridge.Request);
        await sends.SendItemAsync(c, "AUTH-Q1");
        await sends.SendItemAsync(c, "NOPE");
        Assert.Empty(bridge.Sent);
    }

    [Fact]
    public async Task Send_anyway_after_a_secret_warning_repeats_the_items_send()
    {
        var (_, c) = AnsweredFormTests.Card();
        c.Answers.Items[0].Answer = "token is abc";
        c.Session.Draft = "message";
        var bridge = new Bridge();
        bridge.Enqueue(new Reply(1, false, null, new BridgeError("looks-secret", "x")));
        var sends = new SendCoordinator(bridge.Request);
        await sends.SendItemAsync(c, "AUTH-Q1");
        Assert.True(c.SendBox.CanSendAnyway);
        Assert.Equal("AUTH-Q1", c.SendBox.Refused!.ItemKey);

        await sends.SendSecretAnywayAsync(c);
        Assert.Equal(2, bridge.Sent.Count);
        Assert.Equal("Answers:\nAUTH-Q1: token is abc", bridge.Sent[1].Text);
        Assert.True(bridge.Sent[1].AllowSecret);
        Assert.Equal("", c.Answers.Items[0].Answer);
        Assert.Equal("message", c.Session.Draft);
    }

    [Fact]
    public async Task Send_anyway_is_withdrawn_when_the_items_box_changed()
    {
        var (_, c) = AnsweredFormTests.Card();
        c.Answers.Items[0].Answer = "token is abc";
        var bridge = new Bridge();
        bridge.Enqueue(new Reply(1, false, null, new BridgeError("looks-secret", "x")));
        var sends = new SendCoordinator(bridge.Request);
        await sends.SendItemAsync(c, "AUTH-Q1");
        c.Answers.Items[0].Answer = "something else";
        Assert.False(c.SendBox.CanSendAnyway);
        await sends.SendSecretAnywayAsync(c);
        Assert.Single(bridge.Sent);
    }
}
