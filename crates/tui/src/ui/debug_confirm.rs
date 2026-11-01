//! Confirmation of an ephemeral debug container (#576): its own window, the
//! text about the trace it leaves, `y` to create, `n`/Esc to cancel.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;

/// Render the debug-container confirmation above everything else.
pub(crate) fn render_debug_confirm(f: &mut Frame, app: &App, area: Rect) {
    let Some(proposal) = &app.debug_proposal else {
        return;
    };
    let width = 72u16.min(area.width.saturating_sub(4)).max(30);
    let text = proposal.confirmation_text();
    let inner_width = width.saturating_sub(2).max(1) as usize;
    let rows = crate::ui::help::wrap_words(&text, inner_width).len() as u16;
    let height = (rows + 4).min(area.height).max(5);
    let overlay = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    let color = if proposal.is_prod() {
        app.theme.danger
    } else {
        app.theme.warning
    };
    f.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(color))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            format!(" Debug-контейнер {} {} ", app.theme.glyphs().middle_dot, proposal.cluster.name),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay);
    f.render_widget(block, overlay);
    let lines = vec![
        Line::from(Span::styled(text, app.theme.fg_style())),
        Line::raw(""),
        Line::from(Span::styled(
            "y — создать    n / Esc — отмена".to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )),
    ];
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}
