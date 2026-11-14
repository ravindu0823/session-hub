namespace SessionHub.Core.ViewModels;

/// <summary>
/// The text and flags of a send request, kept when the bridge refused it. <paramref name="ItemKey"/> is set
/// when it was one item's own Send, so "Send anyway" repeats that same send.
/// </summary>
public sealed record SendAttempt(string Text, bool Override, bool AllowSecret, string? ItemKey = null);

/// <summary>
/// State line of the send box. A send that timed out is never retried here: the text says it may
/// have gone out. A <c>send-in-flight</c> reply offers an explicit override instead, and a
/// <c>looks-secret</c> reply an explicit "Send anyway" for that same text (<see cref="CanSendAnyway"/>).
/// </summary>
public sealed class SendBoxViewModel : ObservableObject
{
    /// <summary>The <c>looks-secret</c> refusal text, as in src/send/refusals.ts (shown after "Not sent: ").</summary>
    public const string LooksSecretText =
        "The message looks like it contains a secret or card number; it goes to the model provider. Send anyway?";

    private string stateText = "";
    private bool canOverride;
    private bool canSendAnyway;

    public string StateText
    {
        get => stateText;
        private set => SetField(ref stateText, value);
    }

    public bool CanOverride
    {
        get => canOverride;
        private set => SetField(ref canOverride, value);
    }

    /// <summary>
    /// The bridge held the text back because it looks like it holds a secret. Only a deliberate
    /// "Send anyway" sends it; editing the text withdraws the offer.
    /// </summary>
    public bool CanSendAnyway
    {
        get => canSendAnyway;
        private set => SetField(ref canSendAnyway, value);
    }

    /// <summary>The request the bridge refused last (null after a success or a clear).</summary>
    public SendAttempt? Refused { get; private set; }

    /// <summary>Raised by <see cref="RequestOverride"/>; the view sends again with override set.</summary>
    public event Action? OverrideRequested;

    /// <summary>Shows a send state from a <c>send</c> event (or a reply result) and returns the text.</summary>
    public string Show(string state, string? detail)
    {
        CanOverride = false;
        CanSendAnyway = false;
        return StateText = state switch
        {
            "pending" => "Sending…",
            "held" => "Held by the session, waiting for its approval",
            "delivered" => "Delivered",
            "expired" => "Not confirmed within 30 minutes",
            "failed" => $"Failed: {detail ?? "unknown"}",
            _ => state,
        };
    }

    /// <summary>
    /// Shows the outcome of a failed <c>send</c> reply, by its error code, in plain English: never the
    /// code itself. The relay's refusals show "Not sent: " and the TUI's text (src/send/refusals.ts,
    /// checked against Fixtures/refusals.ndjson), as the TUI does; send-in-flight offers the override
    /// instead, and looks-secret also offers "Send anyway" (<see cref="CanSendAnyway"/>).
    /// <paramref name="attempt"/> is the refused request, kept so "Send anyway" or the override can
    /// repeat exactly what was refused.
    /// </summary>
    public string ShowReplyError(string code, string? message = null, SendAttempt? attempt = null)
    {
        Refused = attempt;
        CanOverride = code == "send-in-flight";
        CanSendAnyway = code == "looks-secret";
        return StateText = code switch
        {
            // The bridge ended before it replied: the send may have started, just as with a timeout.
            "timeout" or "bridge-restarted" => "May have been sent. Check its state before sending again.",
            "send-in-flight" => "A message to this session is still pending. Send anyway?",
            "unknown-session" => "Not sent: Session Hub doesn't know this session yet; try again in a few seconds.",
            "session-not-live" => "Not sent: That session isn't running any more.",
            "duplicate-name" => "Not sent: Two live sessions share this name; rename one (/rename) and try again.",
            "unsafe-name" =>
                "Not sent: This session's name has characters Session Hub can't send to safely. Rename it in Claude Code (/rename) and try again.",
            "empty-message" => "Not sent: The message is empty. Type something first.",
            // The relay's 20,000-character limit, or the bridge client's 1 MiB request limit.
            "too-long" => "Not sent: The message is too long. Shorten it and try again.",
            "rate-limited" => "Not sent: Six messages went out in the last minute. Wait a minute and try again.",
            // The bridge's message is this same text; the fixed one is shown so a reply can not change it.
            "looks-secret" => "Not sent: " + LooksSecretText,
            "not-connected" => "Not sent: Session Hub isn't connected to its background service. Try again in a moment.",
            "no-store" => "Not sent: Session Hub has no session index yet. Start the daemon and try again.",
            "disposed" => "Not sent: Session Hub is closing.",
            // send-error carries the relay's own error text (e.g. "claude agents failed").
            "send-error" => string.IsNullOrWhiteSpace(message)
                ? "Not sent: something went wrong. Try again."
                : $"Not sent: something went wrong ({message}). Try again.",
            // internal, store-error, too-large, bad-json, unknown-type, bad-request, bad-reply and
            // anything newer: an error inside Session Hub, never the owner's doing.
            _ => "Not sent: something went wrong inside Session Hub. Try again.",
        };
    }

    /// <summary>The draft changed: a "Send anyway" offer no longer applies, so it and its text go.</summary>
    public void TextEdited()
    {
        if (!CanSendAnyway) return;
        CanSendAnyway = false;
        StateText = "";
    }

    public void Clear()
    {
        CanOverride = false;
        CanSendAnyway = false;
        Refused = null;
        StateText = "";
    }

    public void RequestOverride()
    {
        if (CanOverride) OverrideRequested?.Invoke();
    }
}
