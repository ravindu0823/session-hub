using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using SessionHub.Core.App;

namespace SessionHub.Tray.Controls;

/// <summary>
/// Answer form of the Send views (window Send tab, panel card and Send tab): a box per waiting item
/// and the message box. Ctrl+Enter sends from any box. <see cref="Compact"/> is the panel's layout:
/// tighter cards and no preview.
/// </summary>
public partial class AnswerForm : UserControl
{
    public static readonly DependencyProperty CompactProperty = DependencyProperty.Register(
        nameof(Compact), typeof(bool), typeof(AnswerForm), new PropertyMetadata(false, (d, _) => ((AnswerForm)d).ApplyLayout()));

    public static readonly DependencyProperty LabelMinWidthProperty = DependencyProperty.Register(
        nameof(LabelMinWidth), typeof(double), typeof(AnswerForm), new PropertyMetadata(64.0));

    public static readonly DependencyProperty ChildMarginProperty = DependencyProperty.Register(
        nameof(ChildMargin), typeof(Thickness), typeof(AnswerForm), new PropertyMetadata(new Thickness(74, 2, 0, 0)));

    public static readonly DependencyProperty CardPaddingProperty = DependencyProperty.Register(
        nameof(CardPadding), typeof(Thickness), typeof(AnswerForm), new PropertyMetadata(new Thickness(14, 12, 14, 12)));

    public static readonly DependencyProperty AnswerMinHeightProperty = DependencyProperty.Register(
        nameof(AnswerMinHeight), typeof(double), typeof(AnswerForm), new PropertyMetadata(54.0));

    public static readonly DependencyProperty StateMaxWidthProperty = DependencyProperty.Register(
        nameof(StateMaxWidth), typeof(double), typeof(AnswerForm), new PropertyMetadata(600.0));

    public AnswerForm()
    {
        InitializeComponent();
        ApplyLayout();
    }

    /// <summary>The panel's layout: no ID column, tighter cards, no preview.</summary>
    public bool Compact
    {
        get => (bool)GetValue(CompactProperty);
        set => SetValue(CompactProperty, value);
    }

    public double LabelMinWidth
    {
        get => (double)GetValue(LabelMinWidthProperty);
        private set => SetValue(LabelMinWidthProperty, value);
    }

    public Thickness ChildMargin
    {
        get => (Thickness)GetValue(ChildMarginProperty);
        private set => SetValue(ChildMarginProperty, value);
    }

    public Thickness CardPadding
    {
        get => (Thickness)GetValue(CardPaddingProperty);
        private set => SetValue(CardPaddingProperty, value);
    }

    public double AnswerMinHeight
    {
        get => (double)GetValue(AnswerMinHeightProperty);
        private set => SetValue(AnswerMinHeightProperty, value);
    }

    public double StateMaxWidth
    {
        get => (double)GetValue(StateMaxWidthProperty);
        private set => SetValue(StateMaxWidthProperty, value);
    }

    /// <summary>Puts the caret in the first answer box (or the message box).</summary>
    public void FocusInput()
    {
        var box = FirstTextBox(Questions) ?? Message;
        box.Focus();
        box.CaretIndex = box.Text.Length;
    }

    private void ApplyLayout()
    {
        var compact = Compact;
        LabelMinWidth = compact ? 0 : 64;
        ChildMargin = compact ? new Thickness(0, 2, 0, 0) : new Thickness(74, 2, 0, 0);
        CardPadding = compact ? new Thickness(10) : new Thickness(14, 12, 14, 12);
        AnswerMinHeight = compact ? 38 : 54;
        StateMaxWidth = compact ? 300 : 600;
        PreviewHost.Visibility = compact ? Visibility.Collapsed : Visibility.Visible;
    }

    private void OnPreviewKeyDown(object sender, KeyEventArgs e)
    {
        if (e.Key != Key.Enter || (Keyboard.Modifiers & ModifierKeys.Control) == 0) return;
        e.Handled = true;
        if (PanelCommands.Send.CanExecute(DataContext, this)) PanelCommands.Send.Execute(DataContext, this);
    }

    private void OnClear(object sender, RoutedEventArgs e) => (DataContext as CardViewModel)?.Answers.Clear();

    private void OnDismiss(object sender, RoutedEventArgs e) => ((sender as FrameworkElement)?.DataContext as AnswerItemViewModel)?.Dismiss();

    private void OnUndo(object sender, RoutedEventArgs e) => ((sender as FrameworkElement)?.DataContext as AnswerItemViewModel)?.Undo();

    private static TextBox? FirstTextBox(DependencyObject root)
    {
        for (var i = 0; i < VisualTreeHelper.GetChildrenCount(root); i++)
        {
            var child = VisualTreeHelper.GetChild(root, i);
            if (child is TextBox box) return box;
            if (FirstTextBox(child) is { } found) return found;
        }

        return null;
    }
}
