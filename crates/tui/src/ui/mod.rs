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

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
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

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),       // status bar
            Constraint::Length(1),       // separator
            Constraint::Min(8),          // chat history
            Constraint::Length(1),       // separator
            Constraint::Length(in_height), // input
            Constraint::Length(1),       // separator
            Constraint::Length(1),       // help bar
        ])
        .split(f.area());

    bars::render_status_bar(f, app, chunks[0]);
    bars::render_separator(f, app, chunks[1]);
    chat::render_chat_history(f, app, chunks[2]);
    bars::render_separator(f, app, chunks[3]);
    input::render_input_area(f, app, chunks[4]);
    bars::render_separator(f, app, chunks[5]);
    bars::render_help_bar(f, app, chunks[6]);

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
            let mut state = ScrollbarState::default()
                .content_length(scroll_len)
                .viewport_content_length(grid_visible)
                .position(offset);
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
