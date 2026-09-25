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
mod help;
mod host_select;
mod input;
mod path_picker_overlay;
mod save_overlay;
mod session_select;
mod side_panel;
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
    let (chat_rect, panel_rect) = side_panel_layout(app, chunks[idx + 2], f.area().width);
    chat::render_chat_history(f, app, chat_rect);
    bars::render_separator(f, app, chunks[idx + 3]);
    input::render_input_area(f, app, chunks[idx + 4]);
    bars::render_separator(f, app, chunks[idx + 5]);
    bars::render_help_bar(f, app, chunks[idx + 6]);

    // Side panel (#431): docked beside the feed, or a drawer over it.
    if let Some(rect) = panel_rect {
        side_panel::render_side_panel(f, app, rect);
    }

    // Render confirmation modal on top of chat if in Confirming mode.
    if app.mode == AppMode::Confirming {
        confirm::render_confirm_modal(f, app, app.chat_area);
    }

    // Render help overlay on top of everything if active.
    if app.help_overlay_visible {
        let full = f.area();
        help::render_help_overlay(f, app, full);
    }

    // Render host-selection overlay on top of everything if active.
    if app.host_select_visible {
        let full = f.area();
        host_select::render_host_select(f, app, full);
    }

    // Render session-selection overlay on top of everything if active.
    if app.session_select_visible {
        let full = f.area();
        session_select::render_session_select(f, app, full);
    }

    // Render in-TUI path picker (#351).
    if app.path_picker_visible {
        let full = f.area();
        path_picker_overlay::render_path_picker(f, app, full);
    }

    // Render session-save progress overlay on top of everything if active.
    if app.save_overlay_visible {
        let full = f.area();
        save_overlay::render_save_overlay(f, app, full);
    }
}

/// Split the feed area for the side panel (#431).
///
/// Returns the feed rectangle and, when the panel is visible, the panel's.
/// On a wide terminal the panel docks: the feed narrows to make room. On a
/// narrow one the feed keeps its full width and the panel is a drawer drawn
/// over its right edge, only while opened with `^J`.
fn side_panel_layout(app: &App, feed: Rect, term_width: u16) -> (Rect, Option<Rect>) {
    use crate::side_panel::{docks, PanelContent, FLEET_PANEL_WIDTH, SIDE_PANEL_WIDTH};
    if !app.side_panel.visible(term_width, app.panel_has_content()) {
        return (feed, None);
    }
    let fleet = app.panel_content() == PanelContent::FleetSummary;
    if docks(term_width) {
        let panel = if fleet { FLEET_PANEL_WIDTH } else { SIDE_PANEL_WIDTH };
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(20), Constraint::Length(panel)])
            .split(feed);
        return (chunks[0], Some(chunks[1]));
    }
    // Drawer: leave a strip of the feed visible so it reads as an overlay.
    // The fleet's table takes all but that strip — at 80 columns a 40-wide
    // drawer would clamp every value to a stub (#438).
    let wanted = if fleet { feed.width } else { SIDE_PANEL_WIDTH };
    let width = wanted.min(feed.width.saturating_sub(8)).max(feed.width.min(20));
    let drawer = Rect::new(feed.x + feed.width - width, feed.y, width, feed.height);
    (feed, Some(drawer))
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
        term.render_with_selection(
            f,
            chunks[2],
            app.selection.filter(|s| !s.is_empty()).map(|s| s.normalised()),
            app.theme.selection_bg,
        );

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

    // Render help overlay on top of everything if active.
    if app.help_overlay_visible {
        let full = f.area();
        help::render_help_overlay(f, app, full);
    }

    // Render host-selection overlay on top of everything if active.
    if app.host_select_visible {
        let full = f.area();
        host_select::render_host_select(f, app, full);
    }

    // Render session-selection overlay on top of everything if active.
    if app.session_select_visible {
        let full = f.area();
        session_select::render_session_select(f, app, full);
    }

    // Render in-TUI path picker (#351).
    if app.path_picker_visible {
        let full = f.area();
        path_picker_overlay::render_path_picker(f, app, full);
    }

    // Render session-save progress overlay on top of everything if active.
    if app.save_overlay_visible {
        let full = f.area();
        save_overlay::render_save_overlay(f, app, full);
    }
}

/// Render the tab bar — thin strip above the status bar showing each
/// open session. Only called when `sessions.len() > 1`.
fn render_tab_bar(f: &mut Frame, app: &App, area: Rect) {
    let active = app.active;
    let mut spans: Vec<Span> = Vec::with_capacity(app.sessions.len() * 4);
    // The fleet layer (#432) is not a numbered tab: it leads the row as its
    // own chip, and tab numbers count ordinary tabs only.
    if let Some((fi, fleet)) = app
        .sessions
        .iter()
        .enumerate()
        .find_map(|(i, s)| s.fleet.as_ref().map(|f| (i, f)))
    {
        let (name, n) = (fleet.group_name(), fleet.len());
        let style = if fi == active {
            Style::default().add_modifier(Modifier::REVERSED)
        } else {
            Style::default().add_modifier(Modifier::DIM)
        };
        spans.push(Span::styled(format!("[fleet: {name} ({n})]"), style));
    }
    let mut number = 0usize;
    for (i, s) in app.sessions.iter().enumerate() {
        if s.fleet.is_some() {
            continue;
        }
        number += 1;
        if !spans.is_empty() {
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
        let label = format!("{}{}. {}", marker, number, s.tab_label(i));
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

    fn app_with_op() -> App {
        let mut app = App::new("t".into(), filar_core::CommandConfirmMode::Always);
        app.operations = vec![crate::ops::Operation {
            session: app.sessions[0].id,
            id: "job-1".into(),
            label: "sleep 5".into(),
            hosts: vec![],
        }];
        app
    }

    #[test]
    fn wide_terminal_docks_the_panel_beside_the_feed() {
        let app = app_with_op();
        let feed = Rect::new(0, 2, 160, 30);
        let (chat, panel) = side_panel_layout(&app, feed, 160);
        let panel = panel.expect("docked");
        assert_eq!(chat.width + panel.width, 160);
        assert_eq!(panel.width, crate::side_panel::SIDE_PANEL_WIDTH);
        assert!(panel.x >= chat.x + chat.width, "panel does not cover the feed");
    }

    #[test]
    fn eighty_columns_keep_the_feed_whole_until_ctrl_j() {
        let mut app = app_with_op();
        let feed = Rect::new(0, 2, 80, 20);
        let (chat, panel) = side_panel_layout(&app, feed, 80);
        assert_eq!(chat, feed);
        assert!(panel.is_none());
        app.side_panel.toggle();
        let (chat, panel) = side_panel_layout(&app, feed, 80);
        assert_eq!(chat, feed, "drawer overlays, the feed is not reflowed");
        let panel = panel.expect("drawer");
        assert!(panel.width < 80 && panel.x + panel.width == 80);
    }

    #[test]
    fn the_fleet_table_gets_a_wider_panel() {
        let mut app = crate::app::test_fleet_app(crate::app::test_fleet_view());
        let (_, docked) = side_panel_layout(&app, Rect::new(0, 2, 160, 30), 160);
        assert_eq!(docked.expect("docked").width, crate::side_panel::FLEET_PANEL_WIDTH);
        app.side_panel.toggle();
        let (_, drawer) = side_panel_layout(&app, Rect::new(0, 2, 80, 20), 80);
        assert_eq!(drawer.expect("drawer").width, 72, "all but an 8-column strip of the feed");
    }

    #[test]
    fn nothing_to_show_takes_no_space() {
        let app = App::new("t".into(), filar_core::CommandConfirmMode::Always);
        let feed = Rect::new(0, 2, 160, 30);
        assert_eq!(side_panel_layout(&app, feed, 160), (feed, None));
    }

    #[test]
    fn tab_bar_shows_the_fleet_as_a_chip_and_numbers_tabs_only() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut app = App::new("local".into(), filar_core::CommandConfirmMode::Always);
        app.ssh_targets = vec![filar_core::SshTarget {
            name: "web-1".into(),
            host: "w".into(),
            port: 22,
            user: "u".into(),
            auth: filar_core::SshAuth::Agent,
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: vec!["web".into()],
        }];
        app.host_groups = vec![filar_core::HostGroup {
            name: "web".into(),
            match_tags: vec!["web".into()],
            ..Default::default()
        }];
        app.new_tab();
        app.enter_fleet("web");
        let mut t = Terminal::new(TestBackend::new(80, 1)).unwrap();
        t.draw(|f| render_tab_bar(f, &app, f.area())).unwrap();
        let row: String = (0..80).map(|x| t.backend().buffer()[(x, 0)].symbol().to_string()).collect();
        assert!(row.starts_with("[fleet: web (1)] 1. local 2. local-2"), "{row:?}");
    }

    #[test]
    fn interactive_grid_reserves_four_chrome_lines() {
        assert_eq!(interactive_grid_rows(30), 26);
        assert_eq!(interactive_grid_rows(4), 0);
        assert_eq!(interactive_grid_rows(3), 0); // saturating
    }
}
