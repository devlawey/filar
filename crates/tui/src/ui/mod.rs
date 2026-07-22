//! UI rendering for the TUI.
//!
//! The layout is:
//! ```text
//! ┌─────────────────────────────────────┐
//! │ Status bar: target | mode           │
//! ├─────────────────────────────────────┤
//! │                                     │
//! │ Chat history (scrollable)           │
//! │                                     │
//! ├─────────────────────────────────────┤
//! │ Input field / Confirmation dialog   │
//! ├─────────────────────────────────────┤
//! │ Help bar                            │
//! └─────────────────────────────────────┘
//! ```
//!
//! All colours come from [`Theme`] — no `Color::*` literals exist outside
//! `theme.rs`.

/// Number of lines occupied by interactive-mode "chrome"
/// (status bar + separator + separator + help bar).
pub const INTERACTIVE_CHROME_LINES: u16 = 4;

/// Returns the number of grid rows available for the interactive terminal,
/// given the total terminal height.
pub fn interactive_grid_rows(total_height: u16) -> u16 {
    total_height.saturating_sub(INTERACTIVE_CHROME_LINES)
}

mod bars;
mod chat;
mod confirm;
mod input;
#[allow(unused_imports)]
pub(crate) use chat::scrollbar_content_len;
pub mod layout_cache;
mod text;
pub mod theme;

pub use theme::Theme;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
use ratatui::Frame;

use crate::app::{App, AppMode};
use self::text::wrap_text;

/// Compute the input area height based on wrapped input text.
///
/// Grows from 1 to `max_lines` (5) as the user types multi-line input.
/// Only applies in Normal mode with non-empty input.
fn input_height(app: &App, term_width: u16) -> u16 {
    const MAX_INPUT_LINES: u16 = 5;
    const PROMPT_WIDTH: u16 = 2; // "❯ " or "$ "

    if app.mode != AppMode::Normal || app.input.is_empty() {
        return 1;
    }
    let inner_width = term_width.saturating_sub(PROMPT_WIDTH).max(1) as usize;
    let wrapped = wrap_text(&app.input, inner_width);
    wrapped.len().min(MAX_INPUT_LINES as usize) as u16
}

/// Render the entire UI.
pub fn render(f: &mut Frame, app: &mut App) {
    if app.mode == AppMode::Interactive {
        render_interactive(f, app);
        return;
    }

    let in_height = input_height(app, f.area().width);
    let has_tabs = app.sessions.len() > 1;

    // Layout: optional tab bar (1 line) above the status bar.
    let mut constraints = vec![];
    if has_tabs {
        constraints.push(Constraint::Length(1)); // tab bar
    }
    constraints.extend_from_slice(&[
        Constraint::Length(1),       // status bar
        Constraint::Length(1),       // separator
        Constraint::Min(8),          // chat history
        Constraint::Length(1),       // separator
        Constraint::Length(in_height), // input
        Constraint::Length(1),       // separator
        Constraint::Length(1),       // help bar
    ]);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(f.area());

    let mut idx = 0usize;
    if has_tabs {
        render_tab_bar(f, app, chunks[0]);
        idx += 1;
    }
    bars::render_status_bar(f, app, chunks[idx]);
    bars::render_separator(f, app, chunks[idx + 1]);
    chat::render_chat_history(f, app, chunks[idx + 2]);
    bars::render_separator(f, app, chunks[idx + 3]);
    input::render_input_area(f, app, chunks[idx + 4]);
    bars::render_separator(f, app, chunks[idx + 5]);
    bars::render_help_bar(f, app, chunks[idx + 6]);

    // Render confirmation modal on top of chat if in Confirming mode.
    if app.mode == AppMode::Confirming {
        confirm::render_confirm_modal(f, app, app.chat_area);
    }
}

/// Render the interactive terminal mode.
fn render_interactive(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // status bar
            Constraint::Length(1),  // separator
            Constraint::Min(1),     // terminal grid
            Constraint::Length(1),  // separator
            Constraint::Length(1),  // help bar
        ])
        .split(f.area());

    bars::render_status_bar(f, app, chunks[0]);
    bars::render_separator(f, app, chunks[1]);

    // Store terminal area for mouse hit-testing in interactive mode.
    app.terminal_area = chunks[2];

    // Render the terminal model grid.
    if let Some(ref term) = app.terminal {
        term.render(f, chunks[2]);

        // Scrollbar for scrollback — shown on the right edge of the
        // terminal area when there's history to scroll through.
        let grid_total = term.total_grid_lines();
        let grid_visible = term.rows() as usize;
        let scroll_len = chat::scrollbar_content_len(grid_total, grid_visible);
        if scroll_len > 0 {
            let offset = term.display_offset();
            // display_offset = 0 at bottom (latest output), but ratatui
            // position 0 = top of track. Invert: top-of-history offset
            // maps to position 0, bottom maps to position = scroll_len.
            let mut state = ScrollbarState::default()
                .content_length(scroll_len)
                .viewport_content_length(grid_visible)
                .position(scroll_len.saturating_sub(offset));
            let sb = Scrollbar::new(ScrollbarOrientation::VerticalRight)
                .thumb_style(app.theme.dim())
                .track_style(app.theme.muted());
            f.render_stateful_widget(sb, chunks[2], &mut state);
        }
    } else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Terminal")
            .border_style(Style::default().fg(app.theme.warning));
        let paragraph = Paragraph::new("No terminal active").block(block);
        f.render_widget(paragraph, chunks[2]);
    }

    bars::render_separator(f, app, chunks[3]);
    bars::render_help_bar(f, app, chunks[4]);
}

/// Render the tab bar — thin strip above the status bar showing each
/// open session. Only called when `sessions.len() > 1`.
fn render_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active;
    let mut spans: Vec<Span> = Vec::with_capacity(app.sessions.len() * 4);
    for (i, s) in app.sessions.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        // Activity marker: spinner char for agent running, dot for new output,
        // question mark for pending confirmation.
        let marker = if i != active {
            if s.awaiting_confirmation {
                "? "
            } else if s.background_activity {
                // Use a fullwidth bullet to avoid layout jitter between states.
                "\u{25cf} "
            } else if s.has_new {
                "\u{25cb} "
            } else {
                ""
            }
        } else {
            ""
        };
        let label = format!("{}{}. {}", marker, i + 1, s.target_name);
        let style = if i == active {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(label, style));
    }
    let line = Line::from(spans);
    let paragraph = Paragraph::new(line);
    f.render_widget(paragraph, area);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_grid_reserves_four_chrome_lines() {
        assert_eq!(interactive_grid_rows(30), 26);
        assert_eq!(interactive_grid_rows(4), 0);
        assert_eq!(interactive_grid_rows(3), 0); // saturating
    }
}
