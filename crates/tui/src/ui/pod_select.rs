//! Pod and container chooser of a cluster tab (#565).

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;

use crate::app::pod_tab::{PodSelectRow, PodSelectStage};
use crate::app::App;

/// Render the chooser above the feed: the pod list with its filter, or the
/// containers of the chosen pod.
pub(crate) fn render_pod_select(f: &mut Frame, app: &App, area: Rect) {
    let Some(select) = &app.kube_pod_select else {
        return;
    };
    let glyphs = app.theme.glyphs();
    let (title, items, selected, hint): (String, Vec<ListItem>, Option<usize>, String) =
        match &select.stage {
            PodSelectStage::Pods => {
                let rows = select.rows();
                // A typed selector works without the pod list, so it is
                // offered even while the list loads or failed.
                let selector_typed = select.filter.contains('=');
                let items: Vec<ListItem> = if select.loading && !selector_typed {
                    vec![ListItem::new("  Загрузка подов...")]
                } else if let Some(error) = select.error.as_ref().filter(|_| !selector_typed) {
                    vec![ListItem::new(format!("  {error}"))]
                } else if rows.is_empty() {
                    vec![ListItem::new(
                        "  (нет запущенных подов; введите селектор, например app=web)",
                    )]
                } else {
                    rows.iter()
                        .map(|row| match row {
                            PodSelectRow::Selector(selector) => ListItem::new(Line::from(vec![
                                Span::raw("  "),
                                Span::styled(
                                    format!("по селектору {selector}"),
                                    Style::default().fg(app.theme.accent),
                                ),
                            ])),
                            PodSelectRow::Pod(i) => {
                                let pod = &select.pods[*i];
                                let state = if pod.ready { "" } else { "  (not ready)" };
                                let containers = if pod.containers.len() > 1 {
                                    format!("  контейнеров: {}", pod.containers.len())
                                } else {
                                    String::new()
                                };
                                ListItem::new(Line::from(vec![
                                    Span::raw(format!("  {}", pod.name)),
                                    Span::styled(format!("{state}{containers}"), app.theme.muted()),
                                ]))
                            }
                        })
                        .collect()
                };
                let selected = ((selector_typed || (!select.loading && select.error.is_none()))
                    && !rows.is_empty())
                .then_some(select.index.min(rows.len().saturating_sub(1)));
                (
                    format!(
                        " Под {} {} / {} ",
                        glyphs.middle_dot, select.cluster.name, select.namespace
                    ),
                    items,
                    selected,
                    format!(
                        " фильтр: {}_ {} Enter войти {} Esc ",
                        select.filter, glyphs.middle_dot, glyphs.middle_dot
                    ),
                )
            }
            PodSelectStage::Containers {
                pod,
                containers,
                index,
            } => (
                format!(" Контейнер {} {pod} ", glyphs.middle_dot),
                containers
                    .iter()
                    .map(|c| ListItem::new(format!("  {c}")))
                    .collect(),
                Some(*index),
                format!(" Enter войти {} Esc к подам ", glyphs.middle_dot),
            ),
        };
    let width = 64u16.min(area.width.saturating_sub(4)).max(24);
    let height = (items.len() as u16 + 4)
        .min(area.height.saturating_sub(2))
        .max(4);
    let overlay = Rect::new(
        area.x + area.width.saturating_sub(width) / 2,
        area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    );
    f.render_widget(Clear, overlay);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            title,
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
    let mut state = ListState::default();
    state.select(selected);
    f.render_stateful_widget(
        List::new(items).highlight_style(Style::default().bg(app.theme.selection_bg)),
        list_area,
        &mut state,
    );
    f.render_widget(
        Paragraph::new(Span::styled(hint, app.theme.muted())),
        Rect::new(inner.x, inner.y + inner.height - 1, inner.width, 1),
    );
}
