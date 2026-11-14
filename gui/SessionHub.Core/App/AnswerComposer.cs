using System.Globalization;
using System.Security.Cryptography;
using System.Text;
using SessionHub.Core.Protocol;

namespace SessionHub.Core.App;

/// <summary>
/// Builds the one message that answers a session's "Waiting on you" items:
/// <c>Answers:</c>, one <c>label: answer</c> line per filled answer, a blank line, then the free
/// message. With no answer it is just the message; with neither it is empty (nothing to send).
/// </summary>
public static class AnswerComposer
{
    /// <summary>The relay's limit on the text (src/send/relay.ts MAX_TEXT_CHARS).</summary>
    public const int MaxChars = 20_000;

    /// <summary>The line the relay puts above the text (src/config.ts MESSAGE_HEADER).</summary>
    public const string MessageHeader =
        "[Session Hub: message from the owner. Reply in your normal terminal reply; do not SendMessage back, the sender has exited.]";

    /// <summary>
    /// One label per item: its ID, or <c>Item n</c> (1-based position) when it has none. A label seen
    /// before gets " (2)", " (3)" so every answer box has its own label and draft.
    /// </summary>
    public static IReadOnlyList<string> Labels(IReadOnlyList<WaitingItem> items)
    {
        ArgumentNullException.ThrowIfNull(items);
        var seen = new Dictionary<string, int>(StringComparer.Ordinal);
        var labels = new List<string>(items.Count);
        for (var i = 0; i < items.Count; i++)
        {
            var id = items[i].Id;
            var label = string.IsNullOrWhiteSpace(id) ? $"Item {(i + 1).ToString(CultureInfo.InvariantCulture)}" : id;
            if (seen.TryGetValue(label, out var n))
            {
                seen[label] = n + 1;
                label = $"{label} ({(n + 1).ToString(CultureInfo.InvariantCulture)})";
            }
            else
            {
                seen[label] = 1;
            }

            labels.Add(label);
        }

        return labels;
    }

    /// <summary>
    /// The label each item's answer draft is kept under: the label for an item with an ID, and for one
    /// without, <c>text@</c> plus the first 8 hex digits of the SHA-256 of its text (<c>.2</c>, <c>.3</c>
    /// for later items with the same text). So the answer moves with its question when the list shifts,
    /// and a reworded question does not get the old answer.
    /// </summary>
    public static IReadOnlyList<string> DraftLabels(IReadOnlyList<WaitingItem> items)
    {
        var labels = Labels(items);
        var seen = new Dictionary<string, int>(StringComparer.Ordinal);
        var keys = new List<string>(items.Count);
        for (var i = 0; i < items.Count; i++)
        {
            if (!string.IsNullOrWhiteSpace(items[i].Id))
            {
                keys.Add(labels[i]);
                continue;
            }

            var key = $"text@{TextHash(items[i].Text)}";
            var n = seen.TryGetValue(key, out var count) ? count + 1 : 1;
            seen[key] = n;
            keys.Add(n == 1 ? key : $"{key}.{n.ToString(CultureInfo.InvariantCulture)}");
        }

        return keys;
    }

    /// <summary>The text to send; <paramref name="answers"/> lines up with <paramref name="items"/> (missing ones are blank).</summary>
    public static string Compose(IReadOnlyList<WaitingItem> items, IReadOnlyList<string?> answers, string? message)
    {
        ArgumentNullException.ThrowIfNull(items);
        ArgumentNullException.ThrowIfNull(answers);
        var labels = Labels(items);
        var lines = new List<string>();
        for (var i = 0; i < items.Count; i++)
        {
            var line = AnswerLine(labels[i], i < answers.Count ? answers[i] : null);
            if (line.Length > 0) lines.Add(line);
        }

        var text = new StringBuilder();
        if (lines.Count > 0) text.Append("Answers:\n").AppendJoin('\n', lines);
        var note = Lines(message);
        if (note.Length > 0)
        {
            if (text.Length > 0) text.Append("\n\n");
            text.Append(note);
        }

        return text.ToString();
    }

    /// <summary>
    /// The text that answers one item on its own: <c>Answers:</c> and its <c>label: answer</c> line, with
    /// the label it has in the whole list (<see cref="Labels"/>). Empty when the answer is blank.
    /// </summary>
    public static string ComposeOne(string label, string? answer)
    {
        ArgumentNullException.ThrowIfNull(label);
        var line = AnswerLine(label, answer);
        return line.Length == 0 ? "" : "Answers:\n" + line;
    }

    /// <summary>What the session receives: the relay's header, a blank line, the text.</summary>
    public static string Preview(string text) => $"{MessageHeader}\n\n{text}";

    /// <summary>The <c>GuiSettings.Drafts</c> key of an answer.</summary>
    public static string DraftKey(string sessionId, string label) => $"{sessionId}#{label}";

    /// <summary>
    /// The <c>GuiSettings.Dismissed</c> key of a dismissed item: session, draft label and the status reply it
    /// came from, so a new status reply from the session leaves the old dismissals behind.
    /// </summary>
    public static string DismissKey(string sessionId, string label, string replyUuid) => $"{sessionId}#{label}@{replyUuid}";

    /// <summary>
    /// Splits a <see cref="DismissKey"/>. The session ends at the first "#", the reply starts after the last
    /// "@" (a draft label can hold "@" itself: <c>text@1a2b3c4d</c>). False for anything else.
    /// </summary>
    public static bool TryParseDismissKey(string? key, out string sessionId, out string label, out string replyUuid)
    {
        sessionId = label = replyUuid = "";
        if (string.IsNullOrEmpty(key)) return false;
        var hash = key.IndexOf('#', StringComparison.Ordinal);
        var at = key.LastIndexOf('@');
        if (hash <= 0 || at <= hash + 1 || at == key.Length - 1) return false;
        sessionId = key[..hash];
        label = key[(hash + 1)..at];
        replyUuid = key[(at + 1)..];
        return true;
    }

    /// <summary>One <c>label: answer</c> line; continuation lines are indented by two spaces, blank lines stay empty.</summary>
    private static string AnswerLine(string label, string? answer)
    {
        var text = Lines(answer);
        if (text.Length == 0) return "";
        var indented = text.Split('\n').Select((l, n) => n == 0 || l.Length == 0 ? l : "  " + l);
        return $"{label}: {string.Join('\n', indented)}";
    }

    private static string TextHash(string text) =>
        Convert.ToHexStringLower(SHA256.HashData(Encoding.UTF8.GetBytes(text)))[..8];

    /// <summary>Line breaks as "\n", trailing spaces off each line, blank lines at both ends off.</summary>
    private static string Lines(string? text)
    {
        if (string.IsNullOrWhiteSpace(text)) return "";
        var lines = text.Replace("\r\n", "\n", StringComparison.Ordinal).Replace('\r', '\n').Split('\n')
            .Select(l => l.TrimEnd());
        return string.Join('\n', lines).Trim();
    }
}
