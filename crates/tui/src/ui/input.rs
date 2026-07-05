//! Input field, confirmation dialog, and password input rendering.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::{App, AppMode};

/// Render the input area or confirmation dialog.
pub(crate) fn render_input_area(f: &mut Frame, app: &mut App, area: Rect) {
    // Record the input area for future hit-testing (task 3).
    app.input_area = area;

    match app.mode {
        AppMode::Normal => render_normal_input(f, app, area),
        AppMode::Thinking => render_thinking(f, app, area),
        AppMode::Confirming => render_confirm(f, app, area),
        AppMode::Interactive => {
            // Not used — interactive mode renders the terminal grid directly.
        }
        AppMode::PasswordInput => render_password_input(f, app, area),
    }
}

/// Normal mode: chat input field.
fn render_normal_input(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Input")
        .border_style(Style::default().fg(app.theme.accent));

    let input = if app.input.is_empty() {
        "Type your message and press Enter…"
    } else {
        &app.input
    };
    let style = if app.input.is_empty() {
        app.theme.muted()
    } else {
        app.theme.fg_style()
    };
    let paragraph = Paragraph::new(input)
        .block(block)
        .style(style)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);

    // Place cursor at the correct position (handles multi-line wrap).
    place_cursor(f, app, area);
}

/// Thinking mode: disabled input field with spinner.
///
/// Replaces the old yellow "Agent is thinking" box with a stable layout:
/// the same input frame but muted, with a spinner on the left.
fn render_thinking(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Input")
        .border_style(app.theme.muted());

    let spinner = app.spinner_char();
    let label = if app.streaming { "writing…" } else { "thinking…" };
    let text = format!("{spinner} {label}  (Ctrl+C to cancel)");
    let paragraph = Paragraph::new(text)
        .block(block)
        .style(app.theme.muted());
    f.render_widget(paragraph, area);
}

/// Confirmation mode: input panel shows a muted placeholder.
///
/// The actual confirmation dialog is rendered as a centered modal overlay
/// (see [`crate::ui::confirm::render_confirm_modal`]).
fn render_confirm(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Input")
        .border_style(app.theme.muted());

    let text = "waiting for confirmation…";
    let paragraph = Paragraph::new(text)
        .block(block)
        .style(app.theme.muted());
    f.render_widget(paragraph, area);
}

/// Password input mode: masked input field.
fn render_password_input(f: &mut Frame, app: &App, area: Rect) {
    let block = Block::default()
        .borders(Borders::ALL)
        .title("Password Input (masked)")
        .border_style(
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        );

    // Show asterisks instead of actual characters.
    let masked: String = "*".repeat(app.input.chars().count());
    let display = if masked.is_empty() {
        "Type password, press Enter to send (hidden), Esc to cancel"
    } else {
        &masked
    };
    let style = if app.input.is_empty() {
        app.theme.muted()
    } else {
        app.theme.warning_fg()
    };
    let paragraph = Paragraph::new(display)
        .block(block)
        .style(style)
        .wrap(Wrap { trim: false });
    f.render_widget(paragraph, area);

    // Place cursor at end of masked text.
    place_cursor(f, app, area);
}

/// Place the cursor at the correct position within the input area.
fn place_cursor(f: &mut Frame, app: &App, area: Rect) {
    let cursor_pos = app.cursor_pos as u16;
    let inner_width = area.width.saturating_sub(2).max(1); // -2 for borders
    let cursor_col = cursor_pos % inner_width;
    let cursor_row = cursor_pos / inner_width;
    let cursor_x = area.x + 1 + cursor_col;
    let cursor_y = area.y + 1 + cursor_row.min(area.height.saturating_sub(2));
    f.set_cursor_position((cursor_x, cursor_y));
}
