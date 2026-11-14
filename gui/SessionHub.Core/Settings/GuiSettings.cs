namespace SessionHub.Core.Settings;

/// <summary>
/// The window's restore bounds in pixels of the monitor it was on, and that monitor's scale
/// (0: unknown, from a gui.json written before the scale was kept).
/// </summary>
public sealed record WindowPlacement(double Left, double Top, double Width, double Height, bool Maximized, double Scale = 0);

/// <summary>The tray's own settings, kept in <c>gui.json</c>.</summary>
public sealed class GuiSettings
{
    public const int MaxDraftChars = 20_000;

    public bool Notifications { get; set; } = true;

    public WindowPlacement? Window { get; set; }

    /// <summary>Unsent message text by session id.</summary>
    public Dictionary<string, string> Drafts { get; set; } = [];

    /// <summary>
    /// When each draft's session was last in a snapshot (or the draft was edited), by session id. A draft
    /// with no entry (a gui.json from before this field) counts as seen when the tray loads it.
    /// </summary>
    public Dictionary<string, DateTimeOffset> DraftsSeen { get; set; } = [];

    /// <summary>At most this many dismissed items are kept; the oldest go first.</summary>
    public const int MaxDismissed = 200;

    /// <summary>Longest dismissed-item key kept; a longer one is dropped.</summary>
    public const int MaxDismissedKeyChars = 1_000;

    /// <summary>
    /// Waiting items the owner dismissed in the tray, as <c>&lt;session id&gt;#&lt;draft label&gt;@&lt;status reply&gt;</c>,
    /// oldest first. A new status reply from the session leaves its old keys behind (they are pruned).
    /// </summary>
    public List<string> Dismissed { get; set; } = [];
}
