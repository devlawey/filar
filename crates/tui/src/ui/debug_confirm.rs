//! Confirmation of an ephemeral debug container (#576): its own window, the
//! text about the trace it leaves, `y` to create, `n`/Esc to cancel.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use crate::app::App;

/// Render the debug-container confirmation above everything else.
///
/// The text — whose pod, image and container names can be long — scrolls
/// when it does not fit (#658); the `y` / `n` line stays at the bottom, so
/// it is on screen whatever the scroll.
pub(crate) fn render_debug_confirm(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(proposal) = &app.debug_proposal else {
        return;
    };
    let width = 72u16.min(area.width.saturating_sub(4)).max(30);
    let text = proposal.confirmation_text();
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let rows = crate::ui::help::wrap_words(&text, inner_width);
    let height = (rows.len() as u16 + 4).min(area.height).max(5);
    let overlay = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width.min(area.width),
        height,
    );
    let color = if proposal.is_prod() {
        app.theme.danger
    } else {
        app.theme.warning
    };
    let title = format!(
        " Debug-контейнер {} {} ",
        app.theme.glyphs().middle_dot,
        proposal.cluster.name
    );
    f.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            title,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay);
    f.render_widget(block, overlay);
    // Below the text: a blank row and the keys; a position row when it
    // scrolls.
    let fixed = 2usize;
    let room = (inner.height as usize).saturating_sub(fixed);
    let scrolls = rows.len() > room && room >= 2;
    let window = if scrolls { room - 1 } else { room };
    let max = rows.len().saturating_sub(window);
    app.debug_scroll_max = if scrolls { max } else { 0 };
    app.debug_scroll = app.debug_scroll.min(app.debug_scroll_max);
    let offset = app.debug_scroll;
    let fg = app.theme.fg_style();
    let mut lines: Vec<Line> = rows
        .iter()
        .skip(offset)
        .take(window)
        .map(|row| Line::from(Span::styled(row.clone(), fg)))
        .collect();
    if scrolls {
        let last = offset + window;
        let mut position = format!("строки {}–{last} из {}", offset + 1, rows.len());
        if last < rows.len() {
            position.push_str(&format!(" · ↓ ещё {}", rows.len() - last));
        }
        position.push_str(" · ↑↓ PgUp PgDn");
        lines.push(Line::from(Span::styled(
            position,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));
    }
    while lines.len() < (inner.height as usize).saturating_sub(fixed) {
        lines.push(Line::raw(""));
    }
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        "y — создать    n / Esc — отмена".to_string(),
        Style::default().fg(color).add_modifier(Modifier::BOLD),
    )));
    // Rows are wrapped above; the paragraph only clips.
    f.render_widget(Paragraph::new(lines), inner);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::pod_tab::DebugProposal;
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn screen(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|f| render_debug_confirm(f, app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn long_names_scroll_and_the_keys_stay_on_screen() {
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        app.debug_proposal = Some(DebugProposal {
            origin: app.active_session().id,
            cluster: filar_core::KubeCluster {
                name: "prod-eu".into(),
                ..Default::default()
            },
            namespace: "payments-processing-eu-central".into(),
            pod: format!("{}-7f9c", "checkout-api-worker".repeat(10)),
            target_container: "main-application-container".into(),
            image: "registry.example.com/platform/debug-tools:2026.11".into(),
            name: "filar-debug-a1b2".into(),
        });
        let top = screen(&mut app, 40, 12);
        assert!(top.contains("y — создать"), "{top}");
        assert!(top.contains("↓ ещё"), "{top}");
        assert!(!top.contains("удалить его нельзя"), "{top}");
        app.handle_key(crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::End,
            crossterm::event::KeyModifiers::NONE,
        ));
        let end = screen(&mut app, 40, 12);
        assert!(end.contains("Создать?"), "{end}");
        assert!(end.contains("y — создать"), "{end}");
        assert!(app.debug_proposal.is_some(), "scrolling does not answer");
    }

    #[test]
    fn short_text_has_no_position_line() {
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        app.debug_proposal = Some(DebugProposal {
            origin: app.active_session().id,
            cluster: filar_core::KubeCluster::default(),
            namespace: "shop".into(),
            pod: "web-1".into(),
            target_container: "main".into(),
            image: "busybox:1.37".into(),
            name: "filar-debug-1".into(),
        });
        let shown = screen(&mut app, 100, 30);
        assert!(!shown.contains("строки 1–"), "{shown}");
        assert!(
            shown.contains("Создать?") && shown.contains("y — создать"),
            "{shown}"
        );
        assert_eq!(app.debug_scroll_max, 0);
    }
}
