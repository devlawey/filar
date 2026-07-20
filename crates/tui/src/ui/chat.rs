//! Chat history rendering.

use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
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

    // Build visible lines, applying selection highlighting if active.
    let sel = app.selection;
    let selection_style = Style::default().bg(app.theme.selection_bg);
    let visible_lines: Vec<Line> = app
        .layout_cache
        .lines
        .iter()
        .enumerate()
        .skip(skip)
        .take(visible_height)
        .map(|(line_idx, rl)| {
            let line = rl.line.clone();
            apply_selection(line, line_idx, sel, selection_style)
        })
        .collect();

    let paragraph = Paragraph::new(visible_lines);
    f.render_widget(paragraph, area);

    // Scrollbar — shown only when content overflows.
    if total_lines > visible_height {
        let scroll_len = scrollbar_content_len(total_lines, visible_height);
        let mut scrollbar_state = ScrollbarState::default()
            .content_length(scroll_len)
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

/// Apply selection background to a rendered line.
///
/// If the line is within the selection range, the relevant character columns
/// get `selection_bg` as their background colour.  This works by rebuilding
/// the line's spans: each original span is split at selection boundaries.
fn apply_selection(
    line: Line<'static>,
    line_idx: usize,
    sel: Option<crate::app::Selection>,
    sel_style: Style,
) -> Line<'static> {
    let Some(sel) = sel else { return line };
    if sel.is_empty() {
        return line;
    }
    let ((start_line, start_col), (end_line, end_col)) = sel.normalised();
    // Is this line within the selection range at all?
    if line_idx < start_line || line_idx > end_line {
        return line;
    }
    // Compute the column range for this specific line.
    let col_start = if line_idx == start_line { start_col } else { 0 };
    let col_end = if line_idx == end_line { end_col } else { usize::MAX };

    // Walk through the line's spans, splitting them at selection boundaries.
    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut current_col = 0usize;
    for span in &line.spans {
        let span_len = span.content.chars().count();
        let span_end = current_col + span_len;
        // Compute intersection [col_start, col_end) with [current_col, span_end)
        let intersect_start = col_start.max(current_col);
        let intersect_end = col_end.min(span_end);
        if intersect_start >= intersect_end {
            // No intersection — keep span as-is.
            new_spans.push(span.clone());
        } else {
            // Split into up to 3 parts: before, selected, after.
            let chars: Vec<char> = span.content.chars().collect();
            // Before selection
            if intersect_start > current_col {
                let before: String = chars[..intersect_start - current_col].iter().collect();
                new_spans.push(Span::styled(before, span.style));
            }
            // Selected portion
            let selected: String = chars[intersect_start - current_col..intersect_end - current_col]
                .iter()
                .collect();
            new_spans.push(Span::styled(
                selected,
                span.style.patch(sel_style),
            ));
            // After selection
            if intersect_end < span_end {
                let after: String = chars[intersect_end - current_col..].iter().collect();
                new_spans.push(Span::styled(after, span.style));
            }
        }
        current_col = span_end;
    }
    Line::from(new_spans)
}

/// Helper: compute the scrollbar track length (scrollable positions).
/// `content_length` in ratatui `ScrollbarState` is the number of scrollable
/// positions, NOT the total number of lines. When `total_lines` is the full
/// content height and `visible_height` is the viewport, the scrollbar track
/// represents `total_lines − visible_height` positions.
///
/// Extracted for testability — the rendering path and tests share this formula.
pub(crate) fn scrollbar_content_len(total_lines: usize, visible_height: usize) -> usize {
    total_lines.saturating_sub(visible_height)
}
