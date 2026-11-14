using System.Threading.Tasks;
using System.Windows.Input;
using SessionHub.Core.App;
using SessionHub.Core.ViewModels;

namespace SessionHub.Tray;

/// <summary>Commands raised by cards and the send box; the panel window handles them. The parameter is the card.</summary>
public static class PanelCommands
{
    public static readonly RoutedUICommand Toggle = new("Toggle", nameof(Toggle), typeof(PanelCommands));
    public static readonly RoutedUICommand Focus = new("Show session", nameof(Focus), typeof(PanelCommands));
    public static readonly RoutedUICommand Send = new("Send", nameof(Send), typeof(PanelCommands));
    public static readonly RoutedUICommand SendAnyway = new("Send anyway", nameof(SendAnyway), typeof(PanelCommands));

    /// <summary>One waiting item's own Send button. The parameter is the item (an AnswerItemViewModel), not the card.</summary>
    public static readonly RoutedUICommand SendItem = new("Send this answer", nameof(SendItem), typeof(PanelCommands));

    /// <summary>"Send anyway" after the secret warning. A button click only: it has no key gesture, and Enter never runs it.</summary>
    public static readonly RoutedUICommand SendSecretAnyway = new("Send anyway", nameof(SendSecretAnyway), typeof(PanelCommands));
    public static readonly RoutedUICommand Jump = new("Jump to terminal", nameof(Jump), typeof(PanelCommands));
    public static readonly RoutedUICommand OpenInWindow = new("Open in window", nameof(OpenInWindow), typeof(PanelCommands));
}

/// <summary>What the panel needs from the app. <see cref="TrayController"/> is the real one; the render check has its own.</summary>
internal interface IPanelHost
{
    bool Notifications { get; set; }

    /// <summary>False while rendering offscreen: the panel never hides on deactivate then.</summary>
    bool Interactive { get; }

    void Jump(SessionViewModel session);

    Task SendAsync(CardViewModel card, SendMode mode);

    /// <summary>Sends one waiting item's answer on its own.</summary>
    Task SendItemAsync(CardViewModel card, string itemKey);

    Task StartDaemonAsync();

    void OpenWindow(string? sessionId);

    void Quit(bool fromSignal);

    void PanelHidden();
}
