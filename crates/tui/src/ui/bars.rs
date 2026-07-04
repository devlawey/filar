//! Status bar (top) and help bar (bottom).

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use ratatui::Frame;

use crate::app::{App, AppMode};

/// Render the status bar (top line).
pub(crate) fn render_status_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let mode_text = match app.mode {
        AppMode::Normal => "NORMAL",
        AppMode::Thinking => "THINKING...",
        AppMode::Confirming => "CONFIRM",
        AppMode::Interactive => "INTERACTIVE",
        AppMode::PasswordInput => "PASSWORD",
    };

    // Store area for hit-testing.
    app.status_bar_area = area;

    let mode_color = app.theme.mode_color(app.mode);

    let line = Line::from(vec![
        Span::styled(
            format!(" {} ", app.target_name),
            app.theme.target_badge_style(),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!(" {:?} ", app.confirm_mode),
            app.theme.muted(),
        ),
        Span::raw(" │ "),
        Span::styled(
            format!(" {mode_text} "),
            app.theme.mode_badge_style(mode_color),
        ),
    ]);

    let paragraph = Paragraph::new(line).style(app.theme.surface_style());
    f.render_widget(paragraph, area);
}

/// Render the help bar (bottom line).
pub(crate) fn render_help_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let help_text = match app.mode {
        AppMode::Normal => " Enter=Send | !=Shell | Ctrl+T=Terminal | Ctrl+P=Password | Ctrl+C=Quit",
        AppMode::Thinking => " Ctrl+C=Quit | PgUp/PgDn=Scroll",
        AppMode::Confirming => " Tab=Switch | Enter=Confirm | a/y=Approve | d/n=Deny | Ctrl+C=Quit",
        AppMode::Interactive => " Ctrl+T=Agent mode | (terminal input is forwarded)",
        AppMode::PasswordInput => " Enter=Send password | Esc=Cancel | Ctrl+C=Cancel",
    };

    // Store area for hit-testing.
    app.help_bar_area = area;

    let paragraph = Paragraph::new(help_text).style(app.theme.help_bar_style());
    f.render_widget(paragraph, area);
}
