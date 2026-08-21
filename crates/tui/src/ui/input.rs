//! Input field, confirmation dialog, and password input rendering.

use ratatui::layout::Rect;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use crate::app::{App, AppMode};
use super::text::wrap_text;

/// Render the input area or confirmation dialog.
pub(crate) fn render_input_area(f: &mut Frame, app: &mut App, area: Rect) {
    // Record the input area for future hit-testing.
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

/// Normal mode: chat input with prompt.
///
/// Supports multi-line growth: the input area grows up to 5 lines as the user
/// types, with internal scrolling to keep the cursor visible.
fn render_normal_input(f: &mut Frame, app: &mut App, area: Rect) {
    let glyphs = app.theme.glyphs();

    // Shell-escape: input starts with `!` -> `$` prompt in warning.
    // Prompt glyph is a single char (like `❯` / `>`); the trailing space is
    // added once via `format!("{prompt} ")` so display width stays 2 and
    // matches `place_cursor` / click mapping (#337).
    let is_shell = app.input.starts_with('!');

    let (prompt, prompt_style, text_style) = if is_shell {
        ("$", app.theme.warning_fg(), app.theme.warning_fg())
    } else {
        (glyphs.prompt, app.theme.user_style(), app.theme.fg_style())
    };

    if app.input.is_empty() {
        // Placeholder.
        let line = Line::from(vec![
            Span::styled(format!("{prompt} "), prompt_style),
            Span::styled("enter your message...", app.theme.muted()),
        ]);
        f.render_widget(Paragraph::new(line), area);
        app.input_scroll_offset = 0;
    } else {
        // Wrap input to terminal width and render multiple lines.
        let prompt_width: usize = 2; // prompt char + space
        let inner_width = area.width.saturating_sub(prompt_width as u16).max(1) as usize;
        let wrapped = wrap_text(&app.input, inner_width);

        // Determine which lines to show (scroll to keep cursor visible).
        let cursor_line = app.cursor_pos / inner_width;
        let max_visible = area.height as usize;
        let scroll_offset = if cursor_line >= max_visible {
            cursor_line - max_visible + 1
        } else {
            0
        };

        // Store scroll_offset so set_cursor_from_click can reverse the mapping.
        app.input_scroll_offset = scroll_offset;

        let lines: Vec<Line> = wrapped
            .iter()
            .skip(scroll_offset)
            .take(max_visible)
            .enumerate()
            .map(|(i, text)| {
                if i == 0 && scroll_offset == 0 {
                    Line::from(vec![
                        Span::styled(format!("{prompt} "), prompt_style),
                        Span::styled(text.clone(), text_style),
                    ])
                } else {
                    Line::from(vec![
                        Span::raw("  "), // indent to align with text after prompt
                        Span::styled(text.clone(), text_style),
                    ])
                }
            })
            .collect();

        f.render_widget(Paragraph::new(lines), area);
    }

    // Place cursor after prompt + current position.
    place_cursor(f, app, area, 2, app.input_scroll_offset); // 2 = prompt char + space
}

/// Thinking mode: spinner + muted label.
fn render_thinking(f: &mut Frame, app: &App, area: Rect) {
    let spinner = app.spinner_char();
    let label = if app.streaming { "writing..." } else { "thinking..." };
    let line = Line::from(vec![
        Span::styled(format!("{spinner} "), app.theme.warning_fg()),
        Span::styled(label, app.theme.muted()),
        Span::raw("  "),
        Span::styled("(ctrl+c to cancel)", app.theme.muted()),
    ]);
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(line), area);
}

/// Confirmation mode: muted placeholder.
fn render_confirm(f: &mut Frame, app: &App, area: Rect) {
    let line = Line::from(vec![
        Span::styled("  ", app.theme.muted()),
        Span::styled("waiting for confirmation...", app.theme.muted()),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

/// Password input mode: masked input with prompt.
fn render_password_input(f: &mut Frame, app: &mut App, area: Rect) {
    let glyphs = app.theme.glyphs();
    let prompt = format!("{} ", glyphs.prompt);

    // Show asterisks instead of actual characters.
    let masked: String = "*".repeat(app.input.chars().count());
    if masked.is_empty() {
        let line = Line::from(vec![
            Span::styled(prompt, app.theme.user_style()),
            Span::styled("type password, press enter to send (hidden), esc to cancel", app.theme.muted()),
        ]);
        f.render_widget(Paragraph::new(line), area);
    } else {
        let line = Line::from(vec![
            Span::styled(prompt, app.theme.user_style()),
            Span::styled(masked, app.theme.warning_fg()),
        ]);
        f.render_widget(Paragraph::new(line), area);
    }

    // Place cursor after prompt.
    app.input_scroll_offset = 0;
    place_cursor(f, app, area, 2, 0);
}

/// Place the cursor at the correct position within the input area.
///
/// `prompt_width` is the number of columns the prompt occupies (e.g. 2 for `> `).
/// `scroll_offset` is the number of wrapped lines scrolled out of view (0 when
/// input fits in the visible area).
fn place_cursor(f: &mut Frame, app: &App, area: Rect, prompt_width: u16, scroll_offset: usize) {
    let cursor_pos = app.cursor_pos as u16;
    let inner_width = area.width.saturating_sub(prompt_width).max(1);
    let cursor_col = cursor_pos % inner_width;
    let cursor_row = (cursor_pos / inner_width).saturating_sub(scroll_offset as u16);
    let cursor_x = area.x + prompt_width + cursor_col;
    let cursor_y = area.y + cursor_row.min(area.height.saturating_sub(1));
    f.set_cursor_position((cursor_x, cursor_y));
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn draw_input(app: &mut App, width: u16, height: u16) -> Result<(u16, u16), String> {
        let backend = TestBackend::new(width, height);
        let mut terminal =
            Terminal::new(backend).map_err(|e| format!("TestBackend terminal: {e}"))?;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, width, height);
                render_input_area(f, app, area);
            })
            .map_err(|e| format!("draw input: {e}"))?;
        let pos = terminal
            .get_cursor_position()
            .map_err(|e| format!("cursor position: {e}"))?;
        Ok((pos.x, pos.y))
    }

    #[test]
    fn shell_bang_places_cursor_after_prefix() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('!'),
            crossterm::event::KeyModifiers::NONE,
        ));
        assert_eq!(app.input, "!");
        assert_eq!(app.cursor_pos, 1, "cursor must be past '!'");
        // Prompt "$ " (2 cols) + bang at col 2 → cursor after bang at col 3.
        let (x, y) = draw_input(&mut app, 40, 3).expect("draw shell bang");
        assert_eq!(y, 0);
        assert_eq!(x, 3, "cursor after '$ !', got col {x}");
    }

    #[test]
    fn shell_command_places_cursor_after_last_char() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.input = "!pwd".into();
        app.cursor_pos = 4;
        // "$ " + "!pwd" → cursor after 'd' at column 2+4=6.
        let (x, _) = draw_input(&mut app, 40, 3).expect("draw !pwd");
        assert_eq!(x, 6);
    }

    #[test]
    fn normal_input_cursor_still_after_last_char() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.input = "hi".into();
        app.cursor_pos = 2;
        // Prompt glyph + space (2) + "hi" → cursor at col 4.
        let (x, _) = draw_input(&mut app, 40, 3).expect("draw normal input");
        assert_eq!(x, 4);
    }
}
