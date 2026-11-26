using System.Text.Json;
using System.Text.Json.Nodes;
using SessionHub.Core.Protocol;

namespace SessionHub.Tests;

public class ProtocolTests
{
    private static string[] Lines(string name) =>
        File.ReadAllLines(Path.Combine(AppContext.BaseDirectory, "Fixtures", name + ".ndjson"))
            .Where(l => l.Length > 0).ToArray();

    private static BridgeEvent Parse(string line)
    {
        var ev = Json.ParseEvent(line, out var reason);
        Assert.True(ev is not null, $"reason: {reason}");
        return ev!;
    }

    private static Reply ReplyAt(int index) => Assert.IsType<Reply>(Parse(Lines("replies")[index]));

    [Fact]
    public void HelloParses()
    {
        var hello = Assert.IsType<Hello>(Parse(Lines("hello")[0]));
        Assert.Equal(1, hello.Protocol);
        Assert.Equal("0.1.0-fixture", hello.Version);
    }

    [Fact]
    public void SnapshotParsesEveryField()
    {
        var snap = Assert.IsType<SnapshotEvent>(Parse(Lines("snapshot")[0]));
        Assert.Equal(1, snap.Seq);
        var s = snap.Snapshot;
        var alpha = s.Sessions.Single(x => x.SessionId == "s-alpha");
        Assert.Equal("claude", alpha.Profile);
        Assert.Equal(4101, alpha.Pid);
        Assert.Equal(1762996400000L, alpha.StartedAt);
        Assert.Null(alpha.EndedAt);
        Assert.Equal("default", alpha.PermissionMode);
        Assert.Equal("busy", alpha.State);
        Assert.NotNull(alpha.Status);
        Assert.Equal(40, alpha.Status!.ProgressPct);
        Assert.Equal("build red evil", alpha.Status.Phase);
        Assert.Equal("quote \" and backslash \\ stay", alpha.Status.Remaining[1]);
        Assert.Equal("Q1", alpha.Status.Waiting[0].Id);
        Assert.True(alpha.Status.Waiting[0].IsNew);
        Assert.Equal(["option A", "option B"], alpha.Status.Waiting[0].Children);
        Assert.Null(alpha.Status.Waiting[1].Id);
        Assert.Equal("r-alpha-1", alpha.Status.ReplyUuid);
        Assert.True(alpha.LatestReply!.HasStatus);
        Assert.Equal(2, alpha.WaitingCount);

        var beta = s.Sessions.Single(x => x.SessionId == "s-beta");
        Assert.Equal("beta-\U0001F680", beta.Name);
        Assert.Equal("D:/work/naïve dir/日本", beta.Cwd);
        Assert.Null(beta.Status);
        Assert.Null(beta.LatestReply);
        Assert.Equal("waiting", beta.State);
        Assert.NotNull(s.Meta);
        Assert.NotNull(s.Totals);
    }

    [Fact]
    public void EmptySnapshotParses()
    {
        var snap = Assert.IsType<SnapshotEvent>(Parse(Lines("snapshot")[1]));
        Assert.Empty(snap.Snapshot.Sessions);
        Assert.Equal("stopped", snap.Snapshot.Meta.Daemon);
        Assert.Null(snap.Snapshot.Meta.UnknownVersion);
        Assert.Empty(snap.Snapshot.Meta.Stale);
        Assert.Equal(0, snap.Snapshot.Totals.Live);
    }

    [Fact]
    public void SendEventsParse()
    {
        var all = Lines("send-events").Select(l => Assert.IsType<SendUpdate>(Parse(l))).ToArray();
        Assert.Equal(("s-alpha", "held", "recipient is busy; queued"),
            (all[0].Send.SessionId, all[0].Send.State, all[0].Send.Detail));
        Assert.Equal(1, all[0].Send.Id);
        Assert.Null(all[1].Send.Detail);
        Assert.Equal("failed", all[2].Send.State);
    }

    [Fact]
    public void ErrorEventsParse()
    {
        var all = Lines("errors").Select(l => Assert.IsType<ErrorEvent>(Parse(l))).ToArray();
        Assert.Equal("bad-json", all[0].Code);
        Assert.Equal("unknown-type", all[1].Code);
        Assert.Contains("malformed", all[1].Message);
    }

    [Fact]
    public void HistoryReplyParses()
    {
        var r = ReplyAt(0);
        Assert.True(r.Ok);
        Assert.Equal(1, r.Id);
        var h = Json.DecodeHistory(r);
        Assert.NotNull(h);
        Assert.NotEmpty(h!.Messages);
        Assert.Equal("owner", h.Messages[0].Role);
        Assert.Null(h.Messages[0].FromName);
        Assert.Contains("\n", h.Messages[0].Text);
        Assert.Contains("\u0007", h.Messages[0].Text);
        Assert.False(h.Truncated);
        Assert.False(h.More);
    }

    [Fact]
    public void HistoryPagingFlagsAndUnicode()
    {
        var h = Json.DecodeHistory(ReplyAt(10))!;
        Assert.True(h.More);
        Assert.False(h.Truncated);
        Assert.Contains("\U0001F600 مرحبا", h.Messages[0].Text);
        Assert.False(h.Messages[0].TextTruncated);
    }

    [Fact]
    public void BigMessageKeepsFlagAndLength()
    {
        var h = Json.DecodeHistory(ReplyAt(11))!;
        Assert.True(h.Messages[0].TextTruncated);
        Assert.True(h.Messages[0].Text.Length > 100_000);
    }

    [Fact]
    public void SearchSendAndOtherReplies()
    {
        var hits = Json.DecodeSearch(ReplyAt(2))!;
        Assert.Equal("m1", hits.Hits[0].Uuid);
        Assert.StartsWith("[deploy]", hits.Hits[0].Snippet);

        var send = Json.DecodeSend(ReplyAt(3))!;
        Assert.Equal(1, send.SendId);
        Assert.Equal("pending", send.State);

        Assert.True(Json.DecodeStartDaemon(ReplyAt(8))!.Running);
        Assert.True(ReplyAt(9).Ok);
    }

    [Fact]
    public void ErrorRepliesParse()
    {
        var inflight = ReplyAt(5);
        Assert.False(inflight.Ok);
        Assert.Null(inflight.Result);
        Assert.Equal("send-in-flight", inflight.Error!.Code);
        Assert.Equal(3, inflight.Error.SendId);
        var empty = ReplyAt(7);
        Assert.Equal("empty-message", empty.Error!.Code);
        Assert.Null(empty.Error.SendId);
        Assert.Null(Json.DecodeHistory(empty));
    }

    [Fact]
    public void EveryFixtureLineParses()
    {
        foreach (var name in new[] { "hello", "snapshot", "heartbeat", "send-events", "replies", "errors" })
            foreach (var line in Lines(name))
                Assert.NotNull(Json.ParseEvent(line));
    }

    [Fact]
    public void HeartbeatParsesWithTheCurrentSeq()
    {
        var hb = Assert.IsType<Heartbeat>(Parse(Lines("heartbeat")[0]));
        Assert.Equal(1, hb.Seq);
    }

    [Fact]
    public void SnapshotWithoutATranscriptPathParses()
    {
        var line = Lines("snapshot")[0];
        Assert.DoesNotContain("transcriptPath", line, StringComparison.Ordinal);
        var snap = Assert.IsType<SnapshotEvent>(Parse(line));
        Assert.NotEmpty(snap.Snapshot.Sessions);
    }

    [Fact]
    public void UnknownFieldsAreIgnored()
    {
        var ev = Json.ParseEvent("""{"type":"hello","protocol":1,"version":"x","future":{"a":1}}""");
        Assert.IsType<Hello>(ev);
    }

    [Theory]
    [InlineData("""{"type":"focus","id":1}""")]
    [InlineData("""{"id":1}""")]
    public void UnknownTypeIsNullWithReason(string line)
    {
        Assert.Null(Json.ParseEvent(line, out var reason));
        Assert.Equal("unknown-type", reason);
    }

    [Theory]
    [InlineData("""{"type":"hello","protocol":1,"vers""")]
    [InlineData("not json")]
    [InlineData("[1,2]")]
    [InlineData("")]
    [InlineData("null")]
    public void BadJsonIsNullWithReason(string line)
    {
        Assert.Null(Json.ParseEvent(line, out var reason));
        Assert.Equal("bad-json", reason);
    }

    [Fact]
    public void WrongShapeForKnownTypeIsBadEvent()
    {
        Assert.Null(Json.ParseEvent("""{"type":"send","send":"nope"}""", out var reason));
        Assert.Equal("bad-event", reason);
    }

    [Fact]
    public void TwentyThousandCharTextParsesIntact()
    {
        var text = new string('x', 10_000) + "\"\\\n\u00e9" + new string('y', 9_996);
        Assert.Equal(20_000, text.Length);
        var line = new JsonObject
        {
            ["type"] = "reply",
            ["id"] = 5,
            ["ok"] = true,
            ["result"] = new JsonObject
            {
                ["messages"] = new JsonArray(new JsonObject
                {
                    ["uuid"] = "u", ["sessionId"] = "s", ["ts"] = "t", ["role"] = "peer",
                    ["fromName"] = null, ["text"] = text,
                }),
                ["truncated"] = false,
                ["more"] = false,
            },
        }.ToJsonString();
        var h = Json.DecodeHistory(Assert.IsType<Reply>(Parse(line)))!;
        Assert.Equal(text, h.Messages[0].Text);
    }

    [Fact]
    public void RequestsEncodeToTheFixtureJson()
    {
        var expected = Lines("requests");
        BridgeRequest[] reqs =
        [
            new HistoryRequest(1, "s-alpha", 50),
            new HistoryRequest(2, "s-alpha", 50, "2025-11-13T02:10:00.000Z"),
            new SearchRequest(3, "deploy", 20),
            new SendRequest(4, "s-alpha", "hello from the tray\nsecond line"),
            new SendRequest(5, "s-alpha", "please fail"),
            new SendRequest(6, "s-beta", "are you there?"),
            new SendRequest(7, "s-beta", "are you there?", true),
            new SendRequest(8, "s-alpha", "   "),
            new StartDaemonRequest(9),
            new PingRequest(10),
            new HistoryRequest(11, "s-alpha", 1),
            new HistoryRequest(12, "s-big", 10),
            new HistoryRequest(13, "s-alpha", 50, "2025-11-13T02:10:00.000Z", "m3"),
            new AnswerRequest(14, "D-0001", "yes"),
            new AnswerRequest(15, "D-0002", "maybe"),
            new ShutdownRequest(),
        ];
        Assert.Equal(expected.Length, reqs.Length);
        for (var i = 0; i < reqs.Length; i++)
        {
            var encoded = Json.Encode(reqs[i]);
            Assert.DoesNotContain('\n', encoded);
            Assert.True(JsonNode.DeepEquals(JsonNode.Parse(expected[i]), JsonNode.Parse(encoded)),
                $"request {i}: {encoded}");
        }
    }

    [Fact]
    public void LoneSurrogateInHistoryTextNeverThrows()
    {
        var line = """{"type":"reply","id":1,"ok":true,"result":{"messages":[{"uuid":"u","sessionId":"s","ts":"t","role":"peer","fromName":null,"text":"a\ud800b"}],"truncated":false,"more":false}}""";
        var ev = Json.ParseEvent(line, out var reason);
        if (ev is Reply r)
            Assert.Null(Record.Exception(() => Json.DecodeHistory(r)));
        else
            Assert.NotNull(reason);
    }

    [Fact]
    public void LoneSurrogateInSnapshotNameNeverThrows()
    {
        var line = """{"type":"snapshot","seq":1,"snapshot":{"sessions":[{"sessionId":"s","profile":"p","name":"a\ud800","cwd":"c","pid":1,"kind":"k","startedAt":1,"updatedAt":1,"endedAt":null,"transcriptPath":"t","permissionMode":null,"state":"idle","status":null,"latestReply":null,"waitingCount":0}],"meta":{"unknownCount":0,"unknownVersion":null,"stale":[],"daemon":"running"},"totals":{"waitingItems":0,"needsApproval":0,"live":1}}}""";
        Assert.Null(Record.Exception(() => Json.ParseEvent(line, out _)));
    }

    [Fact]
    public void LoneSurrogateInTypeIsNullWithReason()
    {
        Assert.Null(Json.ParseEvent("""{"type":"a\ud800"}""", out var reason));
        Assert.Equal("bad-json", reason);
    }

    [Fact]
    public void MalformedSessionIsBadEvent()
    {
        var line = """{"type":"snapshot","seq":1,"snapshot":{"sessions":[{"pid":1}],"meta":{"unknownCount":0,"unknownVersion":null,"stale":[],"daemon":"running"},"totals":{"waitingItems":0,"needsApproval":0,"live":0}}}""";
        Assert.Null(Json.ParseEvent(line, out var reason));
        Assert.Equal("bad-event", reason);
    }

    [Fact]
    public void ReplyWithoutIdOrOkIsBadEvent()
    {
        Assert.Null(Json.ParseEvent("""{"type":"reply","ok":true,"result":{}}""", out var r1));
        Assert.Equal("bad-event", r1);
        Assert.Null(Json.ParseEvent("""{"type":"reply","id":1,"result":{}}""", out var r2));
        Assert.Equal("bad-event", r2);
    }

    [Fact]
    public void HostileTextRoundTripsThroughEncode()
    {
        var texts = new[]
        {
            "a" + (char)0 + "b", "bell" + (char)7, "rlo" + (char)0x202E + "evil",
            "ls" + (char)0x2028 + "ps" + (char)0x2029, "emoji " + char.ConvertFromUtf32(0x1F600),
            new string('z', 10_000) + "\"\\\n\r\t" + new string('q', 9_995),
        };
        Assert.Equal(20_000, texts[^1].Length);
        foreach (var text in texts)
        {
            var encoded = Json.Encode(new SendRequest(1, "s", text));
            Assert.DoesNotContain('\n', encoded);
            Assert.DoesNotContain('\r', encoded);
            Assert.Equal(text, JsonNode.Parse(encoded)!["text"]!.GetValue<string>());
        }
    }
}
