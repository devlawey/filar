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
        .ratio((f64::from(progress) / 100.0).clamp(0.0, 1.0));
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

    // Last row inside border: footer hint.
    let footer = " Esc to close ";
    let footer_area = Rect::new(
        inner.x,
        inner.y + inner.height.saturating_sub(1),
        inner.width,
        1,
    );
    f.render_widget(
        Paragraph::new(Span::styled(footer, app.theme.muted())),
        footer_area,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the save overlay into a test buffer and return all visible text.
    fn render_save_text(app: &App) -> String {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_save_overlay(f, app, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..24 {
            for x in 0..80 {
                text.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
        }
        text
    }

    #[test]
    fn overlay_shows_saving_when_progress_zero() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.save_overlay_visible = true;
        app.save_progress = 0;
        let text = render_save_text(&app);
        assert!(text.contains("Saving..."), "must show Saving... when progress=0");
    }

    #[test]
    fn overlay_does_not_panic_on_progress_overflow() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.save_overlay_visible = true;
        app.save_progress = 200;
        let text = render_save_text(&app);
        assert!(!text.trim().is_empty(), "overlay must render without panic");
    }

    #[test]
    fn overlay_renders_done_when_complete_no_error() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.save_overlay_visible = true;
        app.save_progress = 100;
        let text = render_save_text(&app);
        assert!(text.contains("Done!"), "must show Done! when complete");
    }

    #[test]
    fn overlay_renders_error_when_save_error_set() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.save_overlay_visible = true;
        app.save_error = Some("disk full".into());
        let text = render_save_text(&app);
        assert!(text.contains("Error: disk full"), "must show error message");
    }
}
