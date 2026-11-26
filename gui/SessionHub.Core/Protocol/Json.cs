using System.Text.Json;
using System.Text.Json.Serialization;
using System.Text.Json.Serialization.Metadata;

namespace SessionHub.Core.Protocol;

[JsonSourceGenerationOptions(
    PropertyNamingPolicy = JsonKnownNamingPolicy.CamelCase,
    DefaultIgnoreCondition = JsonIgnoreCondition.WhenWritingNull)]
[JsonSerializable(typeof(Hello))]
[JsonSerializable(typeof(SnapshotEvent))]
[JsonSerializable(typeof(SendUpdate))]
[JsonSerializable(typeof(Heartbeat))]
[JsonSerializable(typeof(ErrorEvent))]
[JsonSerializable(typeof(Reply))]
[JsonSerializable(typeof(HistoryResult))]
[JsonSerializable(typeof(SearchResult))]
[JsonSerializable(typeof(SendResult))]
[JsonSerializable(typeof(StartDaemonResult))]
[JsonSerializable(typeof(AnswerResult))]
[JsonSerializable(typeof(BridgeRequest))]
internal sealed partial class BridgeJsonContext : JsonSerializerContext;

public static class Json
{
    /// <summary>
    /// Parses one event line. Never throws. A bad line gives null and a reason:
    /// <c>bad-json</c> (not a JSON object), <c>unknown-type</c>, or <c>bad-event</c> (known type, wrong shape).
    /// </summary>
    public static BridgeEvent? ParseEvent(string line, out string? reason)
    {
        reason = null;
        try
        {
            using var doc = JsonDocument.Parse(line);
            var root = doc.RootElement;
            if (root.ValueKind != JsonValueKind.Object)
            {
                reason = "bad-json";
                return null;
            }

            string? type;
            try
            {
                type = root.TryGetProperty("type", out var t) && t.ValueKind == JsonValueKind.String
                    ? t.GetString()
                    : null;
            }
            catch (Exception)
            {
                // A lone surrogate escape in the value cannot be turned into a string.
                reason = "bad-json";
                return null;
            }

            if (type is not ("hello" or "snapshot" or "heartbeat" or "send" or "error" or "reply"))
            {
                reason = "unknown-type";
                return null;
            }

            try
            {
                BridgeEvent? ev = type switch
                {
                    "hello" => root.Deserialize(BridgeJsonContext.Default.Hello),
                    "snapshot" => root.Deserialize(BridgeJsonContext.Default.SnapshotEvent),
                    "heartbeat" => root.Deserialize(BridgeJsonContext.Default.Heartbeat),
                    "send" => root.Deserialize(BridgeJsonContext.Default.SendUpdate),
                    "error" => root.Deserialize(BridgeJsonContext.Default.ErrorEvent),
                    _ => root.Deserialize(BridgeJsonContext.Default.Reply),
                };
                if (ev is null || !IsComplete(ev))
                {
                    reason = "bad-event";
                    return null;
                }

                return ev;
            }
            catch (JsonException)
            {
                reason = "bad-event";
                return null;
            }
        }
        catch (Exception)
        {
            // JsonDocument.Parse failures, and invalid UTF-16 in any string value.
            reason = "bad-json";
            return null;
        }
    }

    public static BridgeEvent? ParseEvent(string line) => ParseEvent(line, out _);

    /// <summary>One request as one line, without the terminator. Control characters are escaped.</summary>
    public static string Encode(BridgeRequest request) =>
        JsonSerializer.Serialize(request, BridgeJsonContext.Default.BridgeRequest);

    public static HistoryResult? DecodeHistory(Reply r) => Decode(r, BridgeJsonContext.Default.HistoryResult);

    public static SearchResult? DecodeSearch(Reply r) => Decode(r, BridgeJsonContext.Default.SearchResult);

    public static SendResult? DecodeSend(Reply r) => Decode(r, BridgeJsonContext.Default.SendResult);

    public static StartDaemonResult? DecodeStartDaemon(Reply r) =>
        Decode(r, BridgeJsonContext.Default.StartDaemonResult);

    public static AnswerResult? DecodeAnswer(Reply r) => Decode(r, BridgeJsonContext.Default.AnswerResult);

    /// <summary>Null for an error reply or a result of the wrong shape.</summary>
    private static T? Decode<T>(Reply r, JsonTypeInfo<T> info)
        where T : class
    {
        if (!r.Ok || r.Result is not { } result) return null;
        try
        {
            return result.Deserialize(info);
        }
        catch (Exception)
        {
            return null;
        }
    }

    // System.Text.Json leaves a non-nullable reference property null when its key is absent.
    private static bool IsComplete(BridgeEvent ev) => ev switch
    {
        Hello h => h.Version is not null,
        SnapshotEvent s => s.Snapshot?.Sessions is not null && s.Snapshot.Meta is not null && s.Snapshot.Totals is not null
            && MailComplete(s.Snapshot.Mail),
        SendUpdate u => u.Send?.SessionId is not null && u.Send.State is not null,
        Heartbeat => true,
        ErrorEvent e => e.Code is not null && e.Message is not null,
        Reply r => r.Ok ? r.Result is not null : r.Error?.Code is not null && r.Error.Message is not null,
        _ => false,
    };

    // Mail is optional (older bridges leave it out); when present its lists must be there.
    private static bool MailComplete(MailSummary? m) =>
        m is null
        || (m.Decisions is not null && m.WaitingRoles is not null
            && m.Decisions.All(d => d is not null && d.Options is not null)
            && m.WaitingRoles.All(w => w is not null));
}
