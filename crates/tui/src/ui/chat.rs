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
        // Get a local mutable reference to the active session so Rust's
        // split-borrow analysis sees distinct field borrows instead of
        // clashing through DerefMut.
        let s = &mut app.sessions[app.active];
        s.layout_cache.rebuild(
            &s.messages,
            inner_width,
            &app.theme,
            &collapsed,
            s.message_rev,
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

    // Force every cell in the chat viewport to be rewritten each frame.
    // `Clear` alone is not enough on some terminals / with wide glyphs: after
    // scroll, shorter lines left "ghost" characters from the previous frame
    // (#325). Pad lines to full width and fill unused rows with spaces so the
    // differential backend emits a full-area update.
    let width = area.width as usize;
    let visible_lines: Vec<Line> = visible_lines
        .into_iter()
        .map(|line| pad_line_to_width(line, width))
        .chain(std::iter::repeat_with(|| Line::from(" ".repeat(width))))
        .take(visible_height)
        .collect();

    reset_area_cells(f, area);

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
        // Display columns (↓ is width 1); match pad/wrap (#333).
        let indicator_width = unicode_width::UnicodeWidthStr::width(indicator.as_str()) as u16;
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

/// Reset every cell in `area` so the next draw cannot leave stale glyphs.
fn reset_area_cells(f: &mut Frame, area: Rect) {
    let buf = f.buffer_mut();
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = buf.cell_mut((x, y)) {
                cell.reset();
            }
        }
    }
}

/// Pad `line` with trailing spaces up to `width` **display columns**
/// (ratatui / unicode-width). Truncate if the line is already wider so the
/// Paragraph never spills past the viewport (#333).
fn pad_line_to_width(mut line: Line<'static>, width: usize) -> Line<'static> {
    let current = line.width();
    if current < width {
        line.spans.push(Span::raw(" ".repeat(width - current)));
        return line;
    }
    if current > width {
        return truncate_line_to_width(line, width);
    }
    line
}

/// Truncate a styled line to at most `width` display columns.
fn truncate_line_to_width(line: Line<'static>, width: usize) -> Line<'static> {
    use unicode_width::UnicodeWidthChar;

    let style = line.style;
    let alignment = line.alignment;
    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0usize;
    for span in line.spans {
        if used >= width {
            break;
        }
        let mut kept = String::new();
        for c in span.content.chars() {
            let cw = UnicodeWidthChar::width(c).unwrap_or(0);
            // Keep combining marks with the preceding base even at the edge.
            if cw > 0 && used + cw > width {
                break;
            }
            kept.push(c);
            used += cw;
        }
        if !kept.is_empty() {
            new_spans.push(Span::styled(kept, span.style));
        }
    }
    if used < width {
        new_spans.push(Span::raw(" ".repeat(width - used)));
    }
    let mut out = Line::from(new_spans);
    out.style = style;
    out.alignment = alignment;
    out
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use filar_core::{ChatBlock, CommandConfirmMode};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn chat_row(buf: &ratatui::buffer::Buffer, y: u16, width: u16) -> String {
        (0..width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn scroll_replaces_previous_long_line_cells() {
        let mut app = App::new("local".into(), CommandConfirmMode::Allowlist);
        // Long then short lines so scroll exposes shorter content over a
        // previously long row.
        app.messages = vec![
            ChatBlock::User("AAAA".repeat(30)),
            ChatBlock::Agent("BBBB".repeat(30)),
            ChatBlock::User("short".into()),
            ChatBlock::Agent("ok".into()),
        ];
        app.message_rev = 1;
        app.scroll = 0;

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 8);
                render_chat_history(f, &mut app, area);
            })
            .unwrap();

        // Scroll up so shorter content occupies rows that previously held A's.
        app.scroll = 6;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 8);
                render_chat_history(f, &mut app, area);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        // Every cell in the chat area must be a concrete space or content —
        // no leftover run of 'A' from the first frame on a short-line row.
        for y in 0..8 {
            let row = chat_row(buf, y, 40);
            assert_eq!(row.chars().count(), 40, "row {y} must be fully painted: {row:?}");
            // If the row is mostly spaces (padded short line), it must not
            // still contain a long AAAA tail from the previous frame.
            if row.trim().len() < 10 {
                assert!(
                    !row.contains("AAAA"),
                    "stale glyphs on row {y}: {row:?}"
                );
            }
        }
    }

    #[test]
    fn pad_line_to_width_extends_short_line() {
        let line = Line::from("hi");
        let padded = pad_line_to_width(line, 5);
        assert_eq!(padded.width(), 5);
    }

    #[test]
    fn pad_line_to_width_uses_display_columns_for_wide_chars() {
        // Two CJK chars = 4 columns; pad to 6 → two trailing spaces.
        let padded = pad_line_to_width(Line::from("你好"), 6);
        assert_eq!(padded.width(), 6);
        let text: String = padded.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.ends_with("  "));
    }

    #[test]
    fn pad_line_to_width_truncates_overwide() {
        let padded = pad_line_to_width(Line::from("你好世界"), 4);
        assert_eq!(padded.width(), 4);
        let text: String = padded.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "你好");
    }

    #[test]
    fn scroll_wide_and_columnar_leaves_no_stale_glyphs() {
        let mut app = App::new("local".into(), CommandConfirmMode::Allowlist);
        // Wide CJK + tab-aligned columns that used to overflow char-based wrap.
        app.messages = vec![
            ChatBlock::User("用户消息".repeat(20)),
            ChatBlock::Agent("代理回复".repeat(20)),
            ChatBlock::Command {
                command: "ps".into(),
                explanation: String::new(),
                output: Some("PID\tTTY\tTIME\tCMD\n1\t??\t0:00.01\t/sbin/launchd\n".repeat(8)),
                approved: true,
            },
            ChatBlock::User("short".into()),
            ChatBlock::Agent("ok".into()),
        ];
        app.message_rev = 1;
        // High scroll → oldest lines (wide CJK) fill the viewport first.
        app.scroll = 40;

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 8);
                render_chat_history(f, &mut app, area);
            })
            .unwrap();

        let before: Vec<String> = (0..8)
            .map(|y| chat_row(terminal.backend().buffer(), y, 40))
            .collect();
        assert!(
            before
                .iter()
                .any(|r| r.contains('用') || r.contains('代') || r.contains("PID") || r.contains("CMD")),
            "first frame should show wide/columnar content: {before:?}"
        );

        // Back to bottom: short lines must fully replace prior wide cells.
        app.scroll = 0;
        terminal
            .draw(|f| {
                let area = Rect::new(0, 0, 40, 8);
                render_chat_history(f, &mut app, area);
            })
            .unwrap();

        let buf = terminal.backend().buffer();
        for y in 0..8 {
            let row = chat_row(buf, y, 40);
            assert_eq!(row.chars().count(), 40, "row {y} must be fully painted: {row:?}");
            assert!(!row.contains('\t'), "tab leaked on row {y}: {row:?}");
            if row.trim().chars().count() < 10 {
                assert!(
                    !row.contains('用')
                        && !row.contains('代')
                        && !row.contains("PID")
                        && !row.contains("launchd"),
                    "stale wide/columnar glyphs on row {y}: {row:?}"
                );
            }
        }
    }
}

