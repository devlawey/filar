//! Chat history rendering.

use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};
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

    // Inner width (no borders) — drives cache invalidation.
    let inner_width = area.width;

    // Rebuild cache if any invalidation key changed.
    if app
        .layout_cache
        .needs_rebuild(&app.messages, inner_width, app.message_rev)
    {
        let collapsed = app.collapsed_set();
        app.layout_cache.rebuild(
            &app.messages,
            inner_width,
            &app.theme,
            &collapsed,
            app.message_rev,
        );
    }

    // Compute visible slice from cached lines.
    let total_lines = app.layout_cache.lines.len();
    let visible_height = area.height as usize;

    // Definitive scroll clamp — the render path knows the exact visible_height
    // and has just rebuilt the cache, so this is the authoritative clamp.
    let max_scroll = total_lines.saturating_sub(visible_height);
    if app.scroll > max_scroll {
        app.scroll = max_scroll;
    }

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

    let paragraph = Paragraph::new(visible_lines);
    f.render_widget(paragraph, area);

    // Scrollbar — shown only when content overflows.
    if total_lines > visible_height {
        let mut scrollbar_state = ScrollbarState::default()
            .content_length(total_lines)
            .viewport_content_length(visible_height)
            .position(skip);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .thumb_style(app.theme.dim())
            .track_style(app.theme.muted());
        f.render_stateful_widget(scrollbar, area, &mut scrollbar_state);
    }

    // "↓ N new" indicator — shown when the user has scrolled up from the bottom.
    // N is the number of lines below the viewport (= scroll after clamping).
    if app.scroll > 0 && area.height >= 3 && area.width >= 3 {
        let indicator = format!("\u{2193} {} new", app.scroll);
        // Use char count, not byte length — `↓` (U+2193) is 3 bytes but 1 column.
        let indicator_width = indicator.chars().count() as u16;
        let indicator_width = indicator_width.min(inner_width);
        let indicator_area = Rect::new(
            area.x + area.width.saturating_sub(indicator_width),
            area.y + area.height.saturating_sub(1),
            indicator_width,
            1,
        );
        // Store for click detection in hit_test.
        app.indicator_area = indicator_area;
        f.render_widget(
            Paragraph::new(indicator).style(app.theme.muted()),
            indicator_area,
        );
    } else {
        // Clear indicator area so hit_test doesn't detect a stale indicator.
        app.indicator_area = Rect::default();
    }
}
