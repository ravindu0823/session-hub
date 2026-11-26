using System.Text.Json;
using SessionHub.Core.App;
using SessionHub.Core.Protocol;
using SessionHub.Core.ViewModels;

namespace SessionHub.Tests;

public class DecisionsTests
{
    private static string[] Lines(string name) =>
        File.ReadAllLines(Path.Combine(AppContext.BaseDirectory, "Fixtures", name + ".ndjson"))
            .Where(l => l.Length > 0).ToArray();

    private static HubSnapshot Fixture() =>
        Assert.IsType<SnapshotEvent>(Json.ParseEvent(Lines("snapshot")[0], out _)).Snapshot;

    private static Reply Ok(string json) => new(1, true, JsonDocument.Parse(json).RootElement.Clone());

    private static Reply Error(string code, string message) => new(1, false, null, new BridgeError(code, message));

    private sealed class FakeBridge
    {
        public readonly List<BridgeRequest> Requests = [];
        public Reply Next = Ok("""{"result":"answered","role":"alpha"}""");

        /// <summary>When set, replies wait for it.</summary>
        public TaskCompletionSource<Reply>? Hold;

        public Task<Reply> RequestAsync(BridgeRequest request, TimeSpan timeout)
        {
            Requests.Add(request);
            return Hold?.Task ?? Task.FromResult(Next);
        }
    }

    [Fact]
    public void SnapshotCarriesTheMailSummary()
    {
        var mail = Fixture().Mail;
        Assert.NotNull(mail);
        Assert.Equal(["D-0001", "D-0002"], mail!.Decisions.Select(d => d.Id));
        var open = mail.Decisions[0];
        Assert.Equal("open", open.State);
        Assert.Equal("alpha", open.Role);
        Assert.Equal("Ship the release?\nIt has two open bugs.", open.Question);
        Assert.Equal(["yes", "no"], open.Options);
        Assert.Equal("yes", open.Recommendation);
        var answered = mail.Decisions[1];
        Assert.Equal("answered", answered.State);
        Assert.Equal("Rename the table?", answered.Question);
        Assert.Equal("yes", answered.Answer);
        Assert.Equal("cli", answered.AnsweredVia);
        Assert.Equal(1, answered.Conflicts);
        Assert.Equal([new WaitingRole("docs", 1)], mail.WaitingRoles);
        Assert.Equal(0, mail.Flagged);
        Assert.Equal(0, mail.OwnerOpen);
    }

    [Fact]
    public void ASnapshotFromAnOlderBridgeHasNoMail()
    {
        const string line = """{"type":"snapshot","seq":1,"snapshot":{"sessions":[],"meta":{"unknownCount":0,"unknownVersion":null,"stale":[],"daemon":"running"},"totals":{"waitingItems":0,"needsApproval":0,"live":0}}}""";
        var snap = Assert.IsType<SnapshotEvent>(Json.ParseEvent(line, out _));
        Assert.Null(snap.Snapshot.Mail);
    }

    [Fact]
    public void AnswerRepliesFromTheBridgeDecode()
    {
        var replies = Lines("replies").Select(l => Assert.IsType<Reply>(Json.ParseEvent(l, out _))).ToDictionary(r => r.Id);
        Assert.Equal(new AnswerResult("answered", "alpha"), Json.DecodeAnswer(replies[14]));
        Assert.Equal(new AnswerResult("conflict", "gamma"), Json.DecodeAnswer(replies[15]));
    }

    [Theory]
    [InlineData("""{"waitingRoles":[],"flagged":0,"ownerOpen":0}""")]
    [InlineData("""{"decisions":[{"id":"D-0001","role":"a","state":"open","question":"q"}],"waitingRoles":[],"flagged":0,"ownerOpen":0}""")]
    public void ASnapshotWithBrokenMailIsABadEvent(string mail)
    {
        var line = """{"type":"snapshot","seq":1,"snapshot":{"sessions":[],"meta":{"unknownCount":0,"unknownVersion":null,"stale":[],"daemon":"running"},"totals":{"waitingItems":0,"needsApproval":0,"live":0},"mail":""" + mail + "}}";
        Assert.Null(Json.ParseEvent(line, out var reason));
        Assert.Equal("bad-event", reason);
    }

    [Fact]
    public void AnAnswerRequestEncodesWithItsType()
    {
        Assert.Equal(
            """{"type":"answer","id":3,"decisionId":"D-0001","text":"yes"}""",
            Json.Encode(new AnswerRequest(3, "D-0001", "yes")));
        Assert.Equal(
            """{"type":"answer","id":4,"decisionId":"D-0001","text":"no","replace":true}""",
            Json.Encode(new AnswerRequest(4, "D-0001", "no", true)));
    }

    [Fact]
    public void ApplyListsOpenDecisionsAndConflictsWithNotices()
    {
        var vm = new DecisionsViewModel(new FakeBridge().RequestAsync);
        vm.Apply(Fixture().Mail);
        Assert.Equal(["D-0001", "D-0002"], vm.Items.Select(c => c.Id));
        Assert.Equal(1, vm.OpenCount);
        Assert.True(vm.Items[0].IsOpen);
        Assert.Equal("Options: yes | no", vm.Items[0].OptionsText);
        Assert.Equal("Recommended: yes", vm.Items[0].RecommendationText);
        Assert.Equal("Answered: yes (cli). 1 other answer was not applied or was replaced.", vm.Items[1].AnswerText);
        Assert.Equal(["Mail waits for role docs (1 queued): start a session named docs."], vm.Notices);
        Assert.True(vm.HasAny);
    }

    [Fact]
    public void ApplyKeepsDraftsAndDropsDecisionsThatAreGone()
    {
        var vm = new DecisionsViewModel(new FakeBridge().RequestAsync);
        var mail = Fixture().Mail!;
        vm.Apply(mail);
        var card = vm.Items[0];
        card.Draft = "yes, after the fix";
        vm.Apply(mail with { Decisions = [mail.Decisions[0]] });
        Assert.Same(card, Assert.Single(vm.Items));
        Assert.Equal("yes, after the fix", card.Draft);
        vm.Apply(null);
        Assert.Empty(vm.Items);
        Assert.False(vm.HasAny);
    }

    [Fact]
    public async Task AnswerSendsTheDraftOnceAndShowsWhereItWent()
    {
        var bridge = new FakeBridge();
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        var card = vm.Items[0];
        card.Draft = "  yes  ";
        await vm.AnswerAsync(card, replace: false);
        var sent = Assert.IsType<AnswerRequest>(Assert.Single(bridge.Requests));
        Assert.Equal("D-0001", sent.DecisionId);
        Assert.Equal("yes", sent.Text);
        Assert.Null(sent.Replace);
        Assert.Equal("Answered; mailed to alpha.", card.Status);
        Assert.False(card.StatusIsError);
        Assert.Equal("", card.Draft);
    }

    [Fact]
    public async Task AConflictOffersReplaceAndReplaceSendsIt()
    {
        var bridge = new FakeBridge { Next = Ok("""{"result":"conflict","role":"gamma"}""") };
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        var card = vm.Items[1];
        card.Draft = "no";
        await vm.AnswerAsync(card, replace: false);
        Assert.True(card.CanReplace);
        Assert.True(card.StatusIsError);
        Assert.Contains("different", card.Status);
        Assert.Equal("no", card.Draft);
        bridge.Next = Ok("""{"result":"replaced","role":"gamma"}""");
        await vm.AnswerAsync(card, replace: true);
        Assert.True(Assert.IsType<AnswerRequest>(bridge.Requests[1]).Replace);
        Assert.False(card.CanReplace);
        Assert.Equal("Answer changed; mailed to gamma.", card.Status);
    }

    [Fact]
    public async Task ATimeoutIsNotRetriedAndKeepsTheDraft()
    {
        var bridge = new FakeBridge { Next = Error("timeout", "no reply in time") };
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        var card = vm.Items[0];
        card.Draft = "yes";
        await vm.AnswerAsync(card, replace: false);
        Assert.Single(bridge.Requests);
        Assert.True(card.StatusIsError);
        Assert.Contains("may have been stored", card.Status);
        Assert.Equal("yes", card.Draft);
        Assert.False(card.IsBusy);
    }

    [Fact]
    public async Task ARefusalShowsItsMessage()
    {
        var bridge = new FakeBridge { Next = Error("looks-secret", "refused: looks-secret: never answer with a secret") };
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        var card = vm.Items[0];
        card.Draft = "x";
        await vm.AnswerAsync(card, replace: false);
        Assert.Equal("Not sent: refused: looks-secret: never answer with a secret", card.Status);
    }

    [Fact]
    public async Task AnEmptyDraftSendsNothing()
    {
        var bridge = new FakeBridge();
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        vm.Items[0].Draft = "   ";
        Assert.False(vm.Items[0].CanAnswer);
        await vm.AnswerAsync(vm.Items[0], replace: false);
        Assert.Empty(bridge.Requests);
    }

    [Fact]
    public async Task ASecondAnswerWhileTheFirstIsInFlightSendsNothing()
    {
        var bridge = new FakeBridge { Hold = new TaskCompletionSource<Reply>() };
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        var card = vm.Items[0];
        card.Draft = "yes";
        var first = vm.AnswerAsync(card, replace: false);
        Assert.True(card.IsBusy);
        Assert.False(card.CanAnswer);
        await vm.AnswerAsync(card, replace: false);
        Assert.Single(bridge.Requests);
        bridge.Hold.SetResult(Ok("""{"result":"answered","role":"alpha"}"""));
        await first;
        Assert.False(card.IsBusy);
        Assert.Single(bridge.Requests);
    }

    [Fact]
    public async Task ABridgeRestartAfterTheWriteIsNotRetried()
    {
        var bridge = new FakeBridge { Next = Error("bridge-restarted", "bridge ended before the reply") };
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        vm.Items[0].Draft = "yes";
        await vm.AnswerAsync(vm.Items[0], replace: false);
        Assert.Single(bridge.Requests);
        Assert.Contains("may have been stored", vm.Items[0].Status);
        Assert.Equal("yes", vm.Items[0].Draft);
    }

    [Theory]
    [InlineData("same", "Already answered with the same text", false, "")]
    [InlineData("closed", "D-0001 is closed", true, "yes")]
    [InlineData("not-found", "D-0001 no longer exists", true, "yes")]
    [InlineData("later-result", "does not understand", true, "yes")]
    public async Task OtherResultsSayWhatHappened(string result, string status, bool isError, string draftAfter)
    {
        var bridge = new FakeBridge { Next = Ok($$"""{"result":"{{result}}","role":"alpha"}""") };
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        vm.Apply(Fixture().Mail);
        var card = vm.Items[0];
        card.Draft = "yes";
        await vm.AnswerAsync(card, replace: false);
        Assert.Contains(status, card.Status);
        Assert.Equal(isError, card.StatusIsError);
        Assert.Equal(draftAfter, card.Draft);
        Assert.False(card.CanReplace);
    }

    [Fact]
    public async Task ReplaceEndsWhenTheDecisionChanges()
    {
        var bridge = new FakeBridge { Next = Ok("""{"result":"conflict","role":"gamma"}""") };
        var vm = new DecisionsViewModel(bridge.RequestAsync);
        var mail = Fixture().Mail!;
        vm.Apply(mail);
        var card = vm.Items[1];
        card.Draft = "no";
        await vm.AnswerAsync(card, replace: false);
        Assert.True(card.CanReplace);
        vm.Apply(mail);
        Assert.True(card.CanReplace);
        var changed = mail.Decisions[1] with { Answer = "no", AnsweredVia = "cli", Conflicts = 2 };
        vm.Apply(mail with { Decisions = [mail.Decisions[0], changed] });
        Assert.False(card.CanReplace);
    }

    [Fact]
    public void ADraftComesBackWithItsCardAfterAnEmptySnapshot()
    {
        var vm = new DecisionsViewModel(new FakeBridge().RequestAsync);
        var mail = Fixture().Mail!;
        vm.Apply(mail);
        vm.Items[0].Draft = "yes, after the fix";
        vm.Apply(mail with { Decisions = [] });
        Assert.Empty(vm.Items);
        vm.Apply(mail);
        Assert.Equal("yes, after the fix", vm.Items.First(c => c.Id == "D-0001").Draft);
    }

    [Fact]
    public void TheWaitingBadgeCountsOpenDecisions()
    {
        var hub = new HubViewModel();
        var decisions = new DecisionsViewModel(new FakeBridge().RequestAsync);
        var panel = new PanelViewModel(hub, decisions);
        var before = 0;
        var snap = Fixture();
        panel.Apply(snap with { Mail = null }, DateTimeOffset.UnixEpoch);
        before = panel.WaitingTotal;
        panel.Apply(snap, DateTimeOffset.UnixEpoch);
        Assert.Equal(before + 1, panel.WaitingTotal);
        panel.SelectTab(PanelTab.Waiting);
        Assert.True(panel.ShowDecisions);
        panel.SelectTab(PanelTab.Sessions);
        Assert.False(panel.ShowDecisions);
    }
}
