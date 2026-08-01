//! Host-selection overlay — modal window for choosing an SSH target.
//!
//! Opened via `Ctrl+O` in Normal mode. Shows `local` plus all configured
//! `[[ssh_targets]]`. The user navigates with Up/Down and confirms with Enter.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState};
use ratatui::Frame;

use crate::app::App;

const OVERLAY_MAX_WIDTH: u16 = 60;
const OVERLAY_H_MARGIN: u16 = 6;
const OVERLAY_V_MARGIN: u16 = 4;

/// Render the host-selection overlay on top of the current frame.
pub(crate) fn render_host_select(f: &mut Frame, app: &App, area: Rect) {
    let width = OVERLAY_MAX_WIDTH.min(area.width.saturating_sub(2 * OVERLAY_H_MARGIN));
    let list_size = 1 + app.ssh_targets.len() as u16;
    // Height: border (2) + title (1) + items + footer (1) + padding.
    let content_height = list_size + 4;
    let height = content_height.min(area.height.saturating_sub(2 * OVERLAY_V_MARGIN));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;

    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    // Build list items.
    let mut items: Vec<ListItem> = Vec::new();

    if app.ssh_targets.is_empty() {
        // Diagnostic: inform the user why only local is shown.
        let msg = Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "No [[ssh_targets]] found in config.toml.",
                app.theme.warning_fg(),
            ),
        ]);
        items.push(ListItem::new(msg));
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "Add targets like:",
                app.theme.muted(),
            ),
        ])));
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "  [[ssh_targets]]",
                app.theme.dim(),
            ),
        ])));
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "  name = \"my-server\"",
                app.theme.dim(),
            ),
        ])));
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "  host = \"...\"",
                app.theme.dim(),
            ),
        ])));
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "  user = \"root\"",
                app.theme.dim(),
            ),
        ])));
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "  [ssh_targets.auth]",
                app.theme.dim(),
            ),
        ])));
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                "  type = \"agent\"",
                app.theme.dim(),
            ),
        ])));
        items.push(ListItem::new("")); // blank line
    }

    // Item 0: local
    let local_marker = if app.ssh_info.is_none() { " \u{25cf}" } else { "  " };
    let cursor = if app.host_select_index == 0 { "\u{25b6}" } else { " " };
    items.push(ListItem::new(Line::from(vec![
        Span::raw(format!(" {}{}", cursor, local_marker)),
        Span::raw(" "),
        Span::styled("local", Style::default().fg(app.theme.fg)),
        Span::raw("  "),
        Span::styled("Local machine", app.theme.muted()),
    ])));

    // Items 1..N: ssh_targets
    for (i, t) in app.ssh_targets.iter().enumerate() {
        let is_current = app.ssh_info.as_ref()
            .map(|info| *info == format!("{}@{}:{}", t.user, t.host, t.port))
            .unwrap_or(false);
        let marker = if is_current { " \u{25cf}" } else { "  " };
        let cursor = if app.host_select_index == i + 1 { "\u{25b6}" } else { " " };

        let auth_label = match &t.auth {
            filar_core::SshAuth::Agent => "Agent",
            filar_core::SshAuth::Key { .. } => "Key",
            filar_core::SshAuth::Password { .. } => "Password",
        };

        items.push(ListItem::new(Line::from(vec![
            Span::raw(format!(" {}{}", cursor, marker)),
            Span::raw(" "),
            Span::styled(&t.name, Style::default().fg(app.theme.fg).add_modifier(Modifier::BOLD)),
            Span::raw("  "),
            Span::styled(format!("{}@{}:{}", t.user, t.host, t.port), app.theme.muted()),
            Span::raw("  "),
            Span::styled(format!("[{}]", auth_label), app.theme.dim()),
        ])));
    }

    let title = " Select host (Ctrl+O) ";
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(title, Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD)));

    let mut state = ListState::default();
    state.select(Some(app.host_select_index));

    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().bg(app.theme.selection_bg));

    f.render_stateful_widget(list, overlay_area, &mut state);

    // Footer hint.
    let footer = " \u{2191}\u{2193} navigate   Enter select   Esc cancel ";
    let footer_area = Rect::new(
        overlay_area.x,
        overlay_area.y + overlay_area.height.saturating_sub(1),
        overlay_area.width,
        1,
    );
    f.render_widget(
        ratatui::widgets::Paragraph::new(Span::styled(footer, app.theme.muted())),
        footer_area,
    );
}
