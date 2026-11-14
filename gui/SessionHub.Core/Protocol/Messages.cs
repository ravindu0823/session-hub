using System.Text.Json;
using System.Text.Json.Serialization;

namespace SessionHub.Core.Protocol;

// Wire models for the `hub bridge` NDJSON protocol (src/bridge/protocol.ts). Unknown fields are
// ignored on read. Enum-like strings (state, role) stay strings so a new daemon value never fails.

/// <summary>
/// One "Waiting on you" item. <paramref name="Answered"/>: the owner's own message, or a Session Hub relay
/// message, named its ID after the status reply was posted (false when an older daemon leaves it out).
/// </summary>
public sealed record WaitingItem(string? Id, string Text, bool IsNew, IReadOnlyList<string> Children, bool Answered = false);

public sealed record SessionStatus(
    int? ProgressPct,
    string? Phase,
    IReadOnlyList<string> Remaining,
    IReadOnlyList<WaitingItem> Waiting,
    IReadOnlyList<string> Recommendations,
    string ReplyUuid,
    string ReplyTs);

public sealed record LatestReply(string ReplyUuid, string ReplyTs, bool HasStatus);

public sealed record SessionView(
    [property: JsonRequired] string SessionId,
    [property: JsonRequired] string Profile,
    [property: JsonRequired] string Name,
    string Cwd,
    int Pid,
    string Kind,
    long StartedAt,
    long UpdatedAt,
    long? EndedAt,
    string? PermissionMode,
    [property: JsonRequired] string State,
    SessionStatus? Status,
    LatestReply? LatestReply,
    int WaitingCount);

public sealed record StaleProfile(string Profile, long Since);

/// <summary>
/// <paramref name="RelayStrayCount"/>: relay messages that landed in a session other than the one they were
/// sent to (0 when an older daemon leaves it out).
/// </summary>
public sealed record Meta(
    int UnknownCount,
    string? UnknownVersion,
    IReadOnlyList<StaleProfile> Stale,
    [property: JsonRequired] string Daemon,
    int RelayStrayCount = 0);

public sealed record Totals([property: JsonRequired] int WaitingItems, [property: JsonRequired] int NeedsApproval, [property: JsonRequired] int Live);

public sealed record HubSnapshot([property: JsonRequired] IReadOnlyList<SessionView> Sessions, [property: JsonRequired] Meta Meta, [property: JsonRequired] Totals Totals);

public sealed record MessageRow(
    string Uuid,
    string SessionId,
    string Ts,
    string Role,
    string? FromName,
    string Text,
    bool TextTruncated = false);

public sealed record SearchHit(string SessionId, string Uuid, string Ts, string Role, string Snippet);

/// <summary>A send row as the bridge reports it in a <c>send</c> event.</summary>
public sealed record SendProgress(int Id, string SessionId, string State, string? Detail);

/// <summary>Error part of a failed reply.</summary>
public sealed record BridgeError(string Code, string Message, int? SendId = null);

public sealed record HistoryResult(IReadOnlyList<MessageRow> Messages, bool Truncated, bool More);

public sealed record SearchResult(IReadOnlyList<SearchHit> Hits);

public sealed record SendResult(int SendId, string State);

public sealed record StartDaemonResult(bool Running);

// Events, bridge to tray.

public abstract record BridgeEvent;

public sealed record Hello([property: JsonRequired] int Protocol, string Version) : BridgeEvent;

public sealed record SnapshotEvent(int Seq, HubSnapshot Snapshot) : BridgeEvent;

public sealed record SendUpdate(SendProgress Send) : BridgeEvent;

/// <summary>Nothing changed for 15 s; <paramref name="Seq"/> is the snapshot still current. Liveness only.</summary>
public sealed record Heartbeat(int Seq) : BridgeEvent;

public sealed record ErrorEvent(string Code, string Message) : BridgeEvent;

public sealed record Reply([property: JsonRequired] int Id, [property: JsonRequired] bool Ok, JsonElement? Result = null, BridgeError? Error = null) : BridgeEvent;

// Requests, tray to bridge. The `type` discriminator is written by System.Text.Json.

[JsonPolymorphic(TypeDiscriminatorPropertyName = "type", UnknownDerivedTypeHandling = JsonUnknownDerivedTypeHandling.FailSerialization)]
[JsonDerivedType(typeof(HistoryRequest), "history")]
[JsonDerivedType(typeof(SearchRequest), "search")]
[JsonDerivedType(typeof(SendRequest), "send")]
[JsonDerivedType(typeof(StartDaemonRequest), "startDaemon")]
[JsonDerivedType(typeof(PingRequest), "ping")]
[JsonDerivedType(typeof(ShutdownRequest), "shutdown")]
public abstract record BridgeRequest;

/// <summary>A page before the row (<paramref name="BeforeTs"/>, <paramref name="BeforeUuid"/>); with the uuid, rows sharing that ts are not skipped.</summary>
public sealed record HistoryRequest(int Id, string SessionId, int Limit, string? BeforeTs = null, string? BeforeUuid = null) : BridgeRequest;

public sealed record SearchRequest(int Id, string Q, int Limit) : BridgeRequest;

public sealed record SendRequest(int Id, string SessionId, string Text, bool? Override = null, bool? AllowSecret = null) : BridgeRequest;

public sealed record StartDaemonRequest(int Id) : BridgeRequest;

public sealed record PingRequest(int Id) : BridgeRequest;

public sealed record ShutdownRequest : BridgeRequest;
