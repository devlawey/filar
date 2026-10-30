//! Namespace chooser for a Kubernetes cluster tab (#575).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::App;

/// Render the configured or discovered namespace choices above the feed.
pub(crate) fn render_namespace_select(f: &mut Frame, app: &App, area: Rect) {
    let Some(select) = &app.kube_namespace_select else {
        return;
    };
    let Some(kube) = &app.kube else {
        return;
    };
    let width = 56u16.min(area.width.saturating_sub(4)).max(24);
    let rows = if select.loading {
        1
    } else {
        select.namespaces.len().max(1)
    };
    let height = (rows as u16 + 4).min(area.height.saturating_sub(2)).max(4);
    let overlay = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, overlay);
    let glyphs = app.theme.glyphs();
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            format!(" Namespace {} {} ", glyphs.middle_dot, kube.target.name),
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay);
    f.render_widget(block, overlay);
    if inner.height < 2 {
        return;
    }
    let list_area = Rect::new(inner.x, inner.y, inner.width, inner.height - 1);
    if select.loading {
        f.render_widget(Paragraph::new("  Discovering namespaces..."), list_area);
    } else {
        let items = if select.namespaces.is_empty() {
            vec![ListItem::new("  (no accessible namespaces)")]
        } else {
            select
                .namespaces
                .iter()
                .map(|namespace| ListItem::new(Line::from(format!("  {namespace}"))))
                .collect()
        };
        let mut state = ListState::default();
        state.select((!select.namespaces.is_empty()).then_some(select.index));
        f.render_stateful_widget(
            List::new(items).highlight_style(Style::default().bg(app.theme.selection_bg)),
            list_area,
            &mut state,
        );
    }
    f.render_widget(
        Paragraph::new(Span::styled(
            format!(" Enter select {} Esc cancel ", glyphs.middle_dot),
            app.theme.muted(),
        )),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}
