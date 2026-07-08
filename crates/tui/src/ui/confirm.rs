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

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::App;

/// Render the confirmation modal centered over `area` (typically the chat area).
///
/// Stores button rectangles in [`App::confirm_button_areas`] for hit-testing.
pub(crate) fn render_confirm_modal(f: &mut Frame, app: &mut App, area: Rect) {
    // Clear previous button areas (populated during this render).
    // NOTE: hovered_button is NOT cleared here — it is transient state set by
    // mouse Moved events and must persist across renders for hover styling.
    app.confirm_button_areas.clear();

    let Some(confirm) = &app.pending_confirm else {
        return;
    };

    // --- Build content lines (everything except buttons) ---
    let mut lines: Vec<Line> = Vec::new();

    if !confirm.explanation.is_empty() {
        lines.push(Line::from(Span::styled(
            confirm.explanation.as_str(),
            app.theme.fg_style(),
        )));
    }

    if confirm.destructive {
        lines.push(Line::from(Span::styled(
            "WARNING: This command may be destructive!",
            app.theme.danger_fg(),
        )));
    }

    let command_text = format!("$ {}", confirm.command);
    lines.push(Line::from(Span::styled(
        command_text.as_str(),
        app.theme.warning_fg(),
    )));

    lines.push(Line::from(""));

    // --- Estimate modal dimensions ---
    // Minimum width 32 so buttons fit inside borders (30 chars + 2 borders).
    let modal_width = 70u16.min(area.width.saturating_sub(8)).max(32);
    let inner_width = (modal_width.saturating_sub(2)) as usize; // -2 borders

    // Estimate how many rendered rows each text segment will occupy after
    // wrapping, so the content area is tall enough to show everything.
    let explanation_rows = if !confirm.explanation.is_empty() {
        estimate_wrapped_rows(&confirm.explanation, inner_width)
    } else {
        0
    };
    let warning_rows = if confirm.destructive { 1 } else { 0 };
    let command_rows = estimate_wrapped_rows(&command_text, inner_width);
    let empty_rows = 1; // separator line

    let content_height = (explanation_rows + warning_rows + command_rows + empty_rows) as u16;
    // +2 for borders, +1 for buttons line
    let modal_height = content_height + 1 + 2;

    // Center within the area.
    let modal_x = area.x + (area.width.saturating_sub(modal_width)) / 2;
    let modal_y = area.y + (area.height.saturating_sub(modal_height)) / 2;
    let modal_area = Rect::new(modal_x, modal_y, modal_width, modal_height);

    // Clear the area under the modal so chat content doesn't bleed through.
    f.render_widget(Clear, modal_area);

    // Border colour: danger for destructive, warning otherwise.
    let border_color = if confirm.destructive {
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

    // Split inner area: content + buttons.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(content_height),
            Constraint::Length(1), // buttons line
        ])
        .split(inner);

    // Render content (explanation, warning, command, empty line).
    let content = Paragraph::new(lines).wrap(Wrap { trim: false });
    f.render_widget(content, chunks[0]);

    // Render buttons.
    render_buttons(f, app, chunks[1]);
}

/// Render the Approve and Deny buttons, storing their areas for hit-testing.
fn render_buttons(f: &mut Frame, app: &mut App, area: Rect) {
    let approve_label = "[ Approve (a) ]";
    let deny_label = "[ Deny (d) ]";
    let spacing = "   ";

    let approve_len = approve_label.chars().count() as u16;
    let deny_len = deny_label.chars().count() as u16;
    let spacing_len = spacing.chars().count() as u16;
    let total_len = approve_len + spacing_len + deny_len;

    let start_x = area.x + (area.width.saturating_sub(total_len)) / 2;

    let approve_area = Rect::new(start_x, area.y, approve_len, 1);
    let deny_area = Rect::new(
        start_x + approve_len + spacing_len,
        area.y,
        deny_len,
        1,
    );

    // Store for hit-testing.
    app.confirm_button_areas.push((approve_area, true));
    app.confirm_button_areas.push((deny_area, false));

    // Styles: selected button (active for Enter) gets inverted colours
    // (fg↔bg) + BOLD.  Hovered-but-not-selected button gets UNDERLINED to
    // visually distinguish hover from selection.  Unselected, unhovered
    // buttons get surface background with coloured text.
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
    f.render_widget(
        Paragraph::new(spacing).style(app.theme.muted()),
        Rect::new(start_x + approve_len, area.y, spacing_len, 1),
    );
    f.render_widget(
        Paragraph::new(deny_label).style(deny_style),
        deny_area,
    );
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
