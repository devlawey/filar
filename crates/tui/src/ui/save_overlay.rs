//! Session-save progress overlay — modal window shown during Ctrl+S export.
//!
//! Renders a centred modal with a progress bar (`Gauge`), status text,
//! and a footer hint.  Follows the same overlay convention as `host_select`.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph};
use ratatui::Frame;

use crate::app::App;

const OVERLAY_WIDTH: u16 = 50;
const OVERLAY_HEIGHT: u16 = 8;
const H_MARGIN: u16 = 6;
const V_MARGIN: u16 = 4;

/// Render the session-save progress overlay.
pub(crate) fn render_save_overlay(f: &mut Frame, app: &App, area: Rect) {
    let width = OVERLAY_WIDTH.min(area.width.saturating_sub(2 * H_MARGIN));
    let height = OVERLAY_HEIGHT.min(area.height.saturating_sub(2 * V_MARGIN));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;

    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            " Saving Session ",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));

    // Inner area for content (inside the border).
    let inner = block.inner(overlay_area);

    f.render_widget(block, overlay_area);

    // Row 0: percentage label.
    let progress = app.save_progress as u16;
    let pct_line = Line::from(vec![
        Span::raw("  "),
        Span::styled(
            format!("{:>3}%", progress),
            Style::default().fg(app.theme.fg),
        ),
    ]);
    let pct_area = Rect::new(inner.x, inner.y, inner.width, 1);
    f.render_widget(Paragraph::new(pct_line), pct_area);

    // Row 1: progress bar.
    let bar_area = Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1);
    let gauge = Gauge::default()
        .block(Block::default())
        .gauge_style(app.theme.success_fg())
        .style(Style::default().fg(app.theme.fg_dim))
        .ratio(f64::from(progress) / 100.0);
    f.render_widget(gauge, bar_area);

    // Row 3: status text.
    let status = if let Some(ref err) = app.save_error {
        format!("Error: {err}")
    } else if progress < 100 {
        "Saving...".to_string()
    } else {
        "Done!".to_string()
    };
    let status_area = Rect::new(inner.x, inner.y + 3, inner.width, 1);
    f.render_widget(
        Paragraph::new(Span::styled(status, app.theme.muted())),
        status_area,
    );

    // Last row: footer hint.
    let footer = " Esc to close ";
    let footer_area = Rect::new(
        overlay_area.x,
        overlay_area.y + overlay_area.height.saturating_sub(1),
        overlay_area.width,
        1,
    );
    f.render_widget(
        Paragraph::new(Span::styled(footer, app.theme.muted())),
        footer_area,
    );
}
