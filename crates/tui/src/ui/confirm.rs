//! Modal confirmation dialog with clickable buttons.
//!
//! Rendered as a centered overlay on top of the chat area when the app is in
//! [`Confirming`](crate::app::AppMode::Confirming) mode.  The modal contains:
//! - explanation text (if any),
//! - a destructive warning (if applicable),
//! - the command with `$ ` prefix,
//! - two buttons: `[ Approve (a) ]` and `[ Deny (d) ]`.
//!
//! The selected button (default: Deny — safe) is highlighted with inverted
//! colours.  Tab / ← / → toggle the selection; Enter activates it.
//! Mouse clicks on a button activate it directly; hover highlights the button
//! (underline) without changing the selection — Enter always activates the
//! keyboard-selected button, preserving the safety default.
//!
//! Long commands/explanations are truncated so the modal never exceeds the
//! available area (avoids ratatui buffer OOB panics — #324).

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;

/// Minimum modal height: top/bottom borders + one content row + buttons.
const MIN_MODAL_HEIGHT: u16 = 4;

/// Render the confirmation modal centered over `area` (typically the chat area).
///
/// Stores button rectangles in [`App::confirm_button_areas`] for hit-testing.
/// The modal is always clamped to `area`; oversized commands are truncated with
/// a visible notice so Approve/Deny remain on-screen.
pub(crate) fn render_confirm_modal(f: &mut Frame, app: &mut App, area: Rect) {
    // Clear previous button areas (populated during this render).
    // NOTE: hovered_button is NOT cleared here — it is transient state set by
    // mouse Moved events and must persist across renders for hover styling.
    app.confirm_button_areas.clear();

    if area.width < 32 || area.height < MIN_MODAL_HEIGHT {
        return;
    }

    let Some(confirm) = &app.pending_confirm else {
        return;
    };

    // Snapshot fields we need so we can drop the borrow before mutating `app`
    // further (button areas). Styles still need `app.theme` later.
    let explanation = confirm.explanation.clone();
    let command = confirm.command.clone();
    let destructive = confirm.destructive;

    // --- Estimate modal dimensions ---
    // Minimum width 32 so buttons fit inside borders (30 chars + 2 borders).
    let modal_width = 70u16.min(area.width.saturating_sub(8)).max(32);
    let inner_width = (modal_width.saturating_sub(2)) as usize; // -2 borders

    let command_text = format!("$ {command}");
    let explanation_rows = if !explanation.is_empty() {
        estimate_wrapped_rows(&explanation, inner_width)
    } else {
        0
    };
    let warning_rows = if destructive { 1 } else { 0 };
    let command_rows = estimate_wrapped_rows(&command_text, inner_width);
    let empty_rows = 1; // separator line before buttons

    let natural_content = (explanation_rows + warning_rows + command_rows + empty_rows) as u16;
    // +2 borders +1 buttons
    let natural_height = natural_content.saturating_add(3);

    // Clamp to the host area so Rect never extends past the frame buffer.
    let modal_height = natural_height.min(area.height).max(MIN_MODAL_HEIGHT);
    let max_content_rows = modal_height.saturating_sub(3) as usize;

    let lines = build_modal_lines(
        &explanation,
        &command_text,
        destructive,
        app.theme.fg_style(),
        app.theme.danger_fg(),
        app.theme.warning_fg(),
        app.theme.muted(),
        inner_width,
        max_content_rows,
    );
    let content_height = max_content_rows.min(lines.len()).max(1) as u16;

    // Center within the area (height already ≤ area.height).
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

    // Absolute guard: never write past the frame buffer.
    let frame = f.area();
    if modal_area.x.saturating_add(modal_area.width) > frame.width
        || modal_area.y.saturating_add(modal_area.height) > frame.height
    {
        return;
    }

    f.render_widget(Clear, modal_area);

    let border_color = if destructive {
        app.theme.danger
    } else {
        app.theme.warning
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title(Span::styled(
            " Confirm command ",
            Style::default()
                .fg(border_color)
                .add_modifier(Modifier::BOLD),
        ))
        .border_style(Style::default().fg(border_color));

    let inner = block.inner(modal_area);
    f.render_widget(&block, modal_area);

    if inner.height < 2 || inner.width == 0 {
        return;
    }

    // Split inner area: content + buttons (buttons always get 1 row).
    let content_len = content_height.min(inner.height.saturating_sub(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(content_len),
            Constraint::Length(1),
        ])
        .split(inner);

    let content = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(content, chunks[0]);
    render_buttons(f, app, chunks[1]);
}

/// Build modal body lines, truncating so wrapped height ≤ `max_rows`.
fn build_modal_lines(
    explanation: &str,
    command_text: &str,
    destructive: bool,
    fg: Style,
    danger: Style,
    warning: Style,
    muted: Style,
    inner_width: usize,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if max_rows == 0 {
        return Vec::new();
    }

    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut used = 0usize;

    let push = |lines: &mut Vec<Line<'static>>, used: &mut usize, text: String, style: Style| {
        let rows = estimate_wrapped_rows(&text, inner_width);
        if *used + rows > max_rows {
            return false;
        }
        lines.push(Line::from(Span::styled(text, style)));
        *used += rows;
        true
    };

    if !explanation.is_empty() {
        let truncated = truncate_to_rows(explanation, inner_width, max_rows.saturating_sub(2).max(1));
        if !push(&mut lines, &mut used, truncated, fg) && max_rows > 0 {
            lines.push(Line::from(Span::styled(
                "… (explanation truncated)".to_string(),
                muted,
            )));
            used = used.saturating_add(1).min(max_rows);
        }
    }

    if destructive && used < max_rows {
        let _ = push(
            &mut lines,
            &mut used,
            "WARNING: This command may be destructive!".to_string(),
            danger,
        );
    }

    if used < max_rows {
        let cmd_budget = max_rows.saturating_sub(used).saturating_sub(1).max(1);
        let truncated = truncate_to_rows(command_text, inner_width, cmd_budget);
        let was_cut = truncated != command_text
            || estimate_wrapped_rows(command_text, inner_width) > cmd_budget;
        let _ = push(&mut lines, &mut used, truncated, warning);
        if was_cut && used < max_rows {
            let _ = push(
                &mut lines,
                &mut used,
                "… (command truncated)".to_string(),
                muted,
            );
        }
    }

    // Trailing blank separator when there is room.
    if used < max_rows {
        lines.push(Line::from(""));
    }

    lines
}

/// Truncate `text` so its wrapped row count is ≤ `max_rows`.
fn truncate_to_rows(text: &str, width: usize, max_rows: usize) -> String {
    if max_rows == 0 || width == 0 {
        return String::new();
    }
    if estimate_wrapped_rows(text, width) <= max_rows {
        return text.to_string();
    }

    let mut out = String::new();
    let mut rows = 0usize;
    for (i, line) in text.split('\n').enumerate() {
        let line_rows = estimate_wrapped_rows(line, width);
        if rows + line_rows > max_rows {
            // Fit a prefix of this line into remaining rows.
            let remain = max_rows.saturating_sub(rows);
            if remain == 0 {
                break;
            }
            let chars_budget = remain.saturating_mul(width).saturating_sub(1); // room for …
            let prefix: String = line.chars().take(chars_budget).collect();
            if i > 0 || !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&prefix);
            out.push('…');
            break;
        }
        if i > 0 {
            out.push('\n');
        }
        out.push_str(line);
        rows += line_rows;
    }
    if out.is_empty() {
        let chars_budget = max_rows.saturating_mul(width).saturating_sub(1);
        let prefix: String = text.chars().take(chars_budget).collect();
        format!("{prefix}…")
    } else {
        out
    }
}

/// Render the Approve and Deny buttons, storing their areas for hit-testing.
fn render_buttons(f: &mut Frame, app: &mut App, area: Rect) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    let approve_label = "[ Approve (a) ]";
    let deny_label = "[ Deny (d) ]";
    let spacing = "   ";

    let approve_len = approve_label.chars().count() as u16;
    let deny_len = deny_label.chars().count() as u16;
    let spacing_len = spacing.chars().count() as u16;
    let total_len = approve_len + spacing_len + deny_len;

    let start_x = area.x + (area.width.saturating_sub(total_len)) / 2;

    let approve_area = Rect::new(start_x, area.y, approve_len.min(area.width), 1);
    let deny_x = start_x.saturating_add(approve_len).saturating_add(spacing_len);
    let deny_area = Rect::new(deny_x, area.y, deny_len.min(area.width.saturating_sub(deny_x.saturating_sub(area.x))), 1);

    // Only store hit zones that sit inside the host area.
    if approve_area.width > 0 {
        app.confirm_button_areas.push((approve_area, true));
    }
    if deny_area.width > 0 && deny_area.x < area.x.saturating_add(area.width) {
        app.confirm_button_areas.push((deny_area, false));
    }

    let approve_selected = app.confirm_selected;
    let approve_hovered = app.hovered_button == Some(true);
    let approve_style = if approve_selected {
        Style::default()
            .fg(app.theme.surface)
            .bg(app.theme.success)
            .add_modifier(Modifier::BOLD)
    } else if approve_hovered {
        Style::default()
            .fg(app.theme.success)
            .bg(app.theme.surface)
            .add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(app.theme.success)
            .bg(app.theme.surface)
    };

    let deny_selected = !app.confirm_selected;
    let deny_hovered = app.hovered_button == Some(false);
    let deny_style = if deny_selected {
        Style::default()
            .fg(app.theme.surface)
            .bg(app.theme.danger)
            .add_modifier(Modifier::BOLD)
    } else if deny_hovered {
        Style::default()
            .fg(app.theme.danger)
            .bg(app.theme.surface)
            .add_modifier(Modifier::UNDERLINED)
    } else {
        Style::default()
            .fg(app.theme.danger)
            .bg(app.theme.surface)
    };

    f.render_widget(
        Paragraph::new(approve_label).style(approve_style),
        approve_area,
    );
    let spacing_area = Rect::new(
        start_x.saturating_add(approve_len),
        area.y,
        spacing_len.min(area.width.saturating_sub(approve_len)),
        1,
    );
    if spacing_area.width > 0 {
        f.render_widget(
            Paragraph::new(spacing).style(app.theme.muted()),
            spacing_area,
        );
    }
    if deny_area.width > 0 {
        f.render_widget(
            Paragraph::new(deny_label).style(deny_style),
            deny_area,
        );
    }
}

/// Estimate how many terminal rows `text` will occupy after wrapping at
/// `width` columns.  Uses char count (not byte length) for correctness with
/// multi-byte glyphs, matching ratatui's `Wrap { trim: false }` behaviour.
fn estimate_wrapped_rows(text: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    text.split('\n')
        .map(|line| {
            let chars = line.chars().count();
            if chars == 0 {
                1
            } else {
                chars.div_ceil(width)
            }
        })
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, AppMode, PendingConfirm};
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;
    use tokio::sync::oneshot;

    fn render_confirm(app: &mut App, width: u16, height: u16) {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                // Simulate chat area inset like the real layout (status + help).
                let chat = Rect::new(0, 2, width, height.saturating_sub(4).max(MIN_MODAL_HEIGHT));
                app.chat_area = chat;
                render_confirm_modal(f, app, chat);
            })
            .expect("confirm render must not panic");
    }

    fn pending(command: &str, destructive: bool) -> PendingConfirm {
        let (tx, _rx) = oneshot::channel();
        PendingConfirm {
            command: command.to_string(),
            explanation: "long command".into(),
            destructive,
            respond_to: tx,
        }
    }

    #[test]
    fn huge_command_does_not_panic_on_small_terminal() {
        let mut app = App::new("local".into(), CommandConfirmMode::Allowlist);
        let huge = format!(
            "cat > /tmp/Modelfile.dev <<'EOF'\n{}\nEOF\necho done",
            "PARAMETER x 1\n".repeat(80)
        );
        app.pending_confirm = Some(pending(&huge, false));
        app.mode = AppMode::Confirming;
        render_confirm(&mut app, 120, 30);
        assert!(
            !app.confirm_button_areas.is_empty(),
            "buttons must remain hittable after truncate"
        );
        for (rect, _) in &app.confirm_button_areas {
            assert!(
                rect.y < 30,
                "button y={} must stay inside 120x30 frame",
                rect.y
            );
        }
    }

    #[test]
    fn destructive_huge_command_clamped() {
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        let huge = "rm -rf /\n".repeat(100);
        app.pending_confirm = Some(pending(&huge, true));
        app.mode = AppMode::Confirming;
        render_confirm(&mut app, 80, 24);
        assert!(!app.confirm_button_areas.is_empty());
    }

    #[test]
    fn tiny_area_skips_modal_without_panic() {
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        app.pending_confirm = Some(pending("echo hi", false));
        app.mode = AppMode::Confirming;
        let backend = TestBackend::new(40, 10);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                render_confirm_modal(f, &mut app, Rect::new(0, 0, 20, 2));
            })
            .unwrap();
        assert!(app.confirm_button_areas.is_empty());
    }

    #[test]
    fn truncate_to_rows_shortens_long_text() {
        let text = "abcdefghijklmnopqrstuvwxyz".repeat(10);
        let out = truncate_to_rows(&text, 10, 3);
        assert!(estimate_wrapped_rows(&out, 10) <= 3);
        assert!(out.contains('…'));
    }
}
