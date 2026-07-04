//! Chat history rendering.

use std::collections::HashSet;

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;

use crate::app::App;

/// Render the chat history (scrollable).
///
/// Uses [`ChatLayoutCache`](super::layout_cache::ChatLayoutCache) to avoid
/// re-wrapping text on every frame.  The cache is rebuilt only when the
/// terminal width, message count, or message revision changes.
pub(crate) fn render_chat_history(f: &mut Frame, app: &mut App, area: Rect) {
    // Record the chat area for future hit-testing (task 3).
    app.chat_area = area;

    // Inner width (without borders) — drives cache invalidation.
    let inner_width = area.width.saturating_sub(2);

    // Rebuild cache if any invalidation key changed.
    if app
        .layout_cache
        .needs_rebuild(&app.messages, inner_width, app.message_rev)
    {
        app.layout_cache.rebuild(
            &app.messages,
            inner_width,
            &app.theme,
            &HashSet::new(), // no collapsed blocks yet (task 6)
            app.message_rev,
        );
    }

    // Compute visible slice from cached lines.
    let total_lines = app.layout_cache.lines.len();
    let visible_height = area.height.saturating_sub(2) as usize; // -2 for border
    let skip = if total_lines > visible_height {
        total_lines.saturating_sub(visible_height + app.scroll)
    } else {
        0
    };
    let skip = skip.min(total_lines);

    let visible_lines: Vec<Line> = app
        .layout_cache
        .lines
        .iter()
        .skip(skip)
        .take(visible_height)
        .map(|rl| rl.line.clone())
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .title("Chat")
        .border_style(app.theme.muted());

    let paragraph = Paragraph::new(visible_lines).block(block);
    f.render_widget(paragraph, area);
}
