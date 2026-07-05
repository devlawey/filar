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
pub mod layout_cache;
mod text;
pub mod theme;

pub use theme::Theme;

use ratatui::layout::{Constraint, Direction, Layout};
use ratatui::style::Style;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::{App, AppMode};

/// Render the entire UI.
pub fn render(f: &mut Frame, app: &mut App) {
    if app.mode == AppMode::Interactive {
        render_interactive(f, app);
        return;
    }

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // status bar
            Constraint::Min(10),    // chat history
            Constraint::Length(5),  // input / confirm (3 text lines + 2 borders)
            Constraint::Length(1),  // help bar
        ])
        .split(f.area());

    bars::render_status_bar(f, app, chunks[0]);
    chat::render_chat_history(f, app, chunks[1]);
    input::render_input_area(f, app, chunks[2]);
    bars::render_help_bar(f, app, chunks[3]);

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
            Constraint::Min(1),     // terminal grid
            Constraint::Length(1),  // help bar
        ])
        .split(f.area());

    bars::render_status_bar(f, app, chunks[0]);

    // Render the terminal model grid.
    if let Some(ref term) = app.terminal {
        term.render(f, chunks[1]);
    } else {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Terminal")
            .border_style(Style::default().fg(app.theme.warning));
        let paragraph = Paragraph::new("No terminal active").block(block);
        f.render_widget(paragraph, chunks[1]);
    }

    bars::render_help_bar(f, app, chunks[2]);
}
