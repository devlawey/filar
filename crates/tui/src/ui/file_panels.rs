//! File panels (Ctrl+G, #535): two directory views side by side.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::UnicodeWidthStr;

use crate::app::App;
use crate::file_panels::{format_mtime, human_size, CopyPlan, Panel, PanelDialog, PanelStatus, Row, SecretPrompt, Side};
use filar_transport::EntryKind;

const SIZE_COL: usize = 7;
const DATE_COL: usize = 16;

/// Draw the panels over the whole frame.
pub(crate) fn render_file_panels(f: &mut Frame, app: &mut App, area: Rect) {
    let theme = &app.theme;
    let copies = &app.file_copies;
    let Some(fp) = app.file_panels.as_mut() else {
        return;
    };
    f.render_widget(Clear, area);
    f.render_widget(Block::default().style(Style::default().bg(theme.bg)), area);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[0]);

    let active = fp.active;
    render_panel(f, theme, &mut fp.left, active == Side::Left, cols[0]);
    render_panel(f, theme, &mut fp.right, active == Side::Right, cols[1]);

    // Status: the running copy, else the last notice (why a copy cannot
    // start, a copy's result).
    let running = copies.iter().find(|c| c.running());
    let (status, style) = match (running, fp.notice.as_deref()) {
        (Some(c), _) => (
            format!(" {}   Ctrl+Z cancel", c.status_line()),
            Style::default().fg(theme.accent),
        ),
        (None, Some(n)) => (format!(" {n}"), theme.warning_fg()),
        (None, None) => (String::new(), theme.muted()),
    };
    f.render_widget(
        Paragraph::new(Span::styled(truncate(&status, rows[1].width as usize), style)),
        rows[1],
    );

    let hint = fit_hint(rows[2].width as usize);
    f.render_widget(Paragraph::new(Span::styled(hint, theme.muted())), rows[2]);

    match fp.dialog.as_ref() {
        Some(PanelDialog::ConfirmCopy(plan)) => {
            let from = fp.panel(plan.from).label.clone();
            render_confirm(f, theme, plan, &from, area);
        }
        Some(PanelDialog::NewFolder { side, input, error }) => {
            let p = fp.panel(*side);
            let place = format!("{}:{}", p.label, p.path);
            render_new_folder(f, theme, &place, input, error.as_deref(), area);
        }
        None => {}
    }
    if let Some(prompt) = fp.prompt.as_ref() {
        render_prompt(f, theme, prompt, area);
    }
}

fn render_panel(f: &mut Frame, theme: &crate::ui::Theme, p: &mut Panel, active: bool, area: Rect) {
    let border = if active {
        Style::default().fg(theme.accent)
    } else {
        theme.muted()
    };
    let mut title = if p.label.is_empty() {
        " — ".to_string()
    } else if p.path.is_empty() {
        format!(" {} ", p.label)
    } else {
        format!(" {}:{} ", p.label, p.path)
    };
    if p.read_only {
        title.push_str("[read-only] ");
    }
    let inner_width = area.width.saturating_sub(2) as usize;
    let title = truncate_left(&title, inner_width);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(border)
        .title(Span::styled(
            title,
            if active {
                Style::default()
                    .fg(theme.accent)
                    .add_modifier(Modifier::BOLD)
            } else {
                theme.muted()
            },
        ));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let mut lines: Vec<Line> = Vec::new();
    match &p.status {
        PanelStatus::Empty => {
            lines.push(Line::from(Span::styled(
                "  Ctrl+O — choose a host",
                theme.muted(),
            )));
        }
        PanelStatus::Connecting => {
            lines.push(Line::from(Span::styled("  Connecting…", theme.muted())));
        }
        PanelStatus::Loading => {
            lines.push(Line::from(Span::styled("  Loading…", theme.muted())));
        }
        PanelStatus::Error(e) => {
            lines.push(Line::from(Span::styled(
                truncate(&format!("  Error: {e}"), inner.width as usize),
                theme.warning_fg(),
            )));
        }
        PanelStatus::Ready => {}
    }
    let header_rows = lines.len();
    let footer_rows = usize::from(p.truncated);
    let list_height = (inner.height as usize).saturating_sub(header_rows + footer_rows);

    let show_list = !p.path.is_empty() && !matches!(p.status, PanelStatus::Connecting);
    if show_list && list_height > 0 {
        // Keep the cursor in view.
        if p.cursor < p.scroll {
            p.scroll = p.cursor;
        } else if p.cursor >= p.scroll + list_height {
            p.scroll = p.cursor + 1 - list_height;
        }
        let width = inner.width as usize;
        let name_width = width.saturating_sub(SIZE_COL + DATE_COL + 4);
        let show_cols = width >= 40;
        for i in p.scroll..(p.scroll + list_height).min(p.row_count()) {
            let Some(row) = p.row(i) else {
                break;
            };
            let selected = active && i == p.cursor;
            let marked = matches!(row, Row::Entry(e) if p.marked.contains(&e.name));
            let (name, size, date, is_dir) = match row {
                Row::Parent => ("..".to_string(), String::new(), String::new(), true),
                Row::Entry(e) => {
                    let is_dir = e.metadata.kind == EntryKind::Dir;
                    let name = match e.metadata.kind {
                        EntryKind::Dir => format!("{}/", e.name),
                        EntryKind::Symlink => format!("{}@", e.name),
                        _ => e.name.clone(),
                    };
                    let size = if is_dir {
                        "<DIR>".to_string()
                    } else {
                        human_size(e.metadata.size)
                    };
                    (name, size, format_mtime(e.metadata.modified), is_dir)
                }
            };
            let lead = if marked { '*' } else { ' ' };
            let text = if show_cols {
                format!(
                    "{lead}{:<nw$} {:>sw$}  {:<dw$}",
                    pad_to(&truncate(&name, name_width), name_width),
                    size,
                    date,
                    nw = name_width,
                    sw = SIZE_COL,
                    dw = DATE_COL
                )
            } else {
                format!("{lead}{}", truncate(&name, width.saturating_sub(1)))
            };
            let mut style = if is_dir {
                Style::default().fg(theme.accent)
            } else {
                Style::default().fg(theme.fg)
            };
            if marked {
                style = theme.warning_fg();
            }
            if selected {
                style = style.bg(theme.selection_bg).add_modifier(Modifier::BOLD);
            } else if !active && i == p.cursor {
                style = style.bg(theme.selection_bg);
            }
            lines.push(Line::from(Span::styled(text, style)));
        }
        if p.row_count() == 0 && matches!(p.status, PanelStatus::Ready) {
            lines.push(Line::from(Span::styled("  (empty)", theme.muted())));
        }
    }
    if p.truncated {
        while lines.len() < (inner.height as usize).saturating_sub(1) {
            lines.push(Line::from(""));
        }
        lines.push(Line::from(Span::styled(
            format!(
                "  … only the first {} entries shown",
                crate::file_panels::MAX_PANEL_ENTRIES
            ),
            theme.warning_fg(),
        )));
    }
    f.render_widget(Paragraph::new(lines), inner);
}

/// "Copy … → host:/dir?" — every copy is confirmed (#536).
fn render_confirm(f: &mut Frame, theme: &crate::ui::Theme, plan: &CopyPlan, from: &str, area: Rect) {
    let width = 72.min(area.width.saturating_sub(4));
    let inner = width.saturating_sub(4) as usize;
    let rect = centered(area, width, 7);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from(Span::styled(
            truncate(&format!("Copy {} from {from}", plan.what()), inner),
            Style::default().fg(theme.fg),
        )),
        Line::from(Span::styled(
            truncate_left(&format!("to {}", plan.target()), inner),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            truncate("Enter copy   Esc cancel   (existing files: asked one by one)", inner),
            theme.muted(),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.bg))
        .title(" Copy? ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// "File exists" question of a running copy — drawn over everything, since
/// the copy may run with the panels closed (#536).
pub(crate) fn render_overwrite(f: &mut Frame, app: &App, area: Rect) {
    let Some(copy) = app.file_copies.iter().find(|c| c.asking.is_some()) else {
        return;
    };
    let dst = copy.asking.clone().unwrap_or_default();
    let theme = &app.theme;
    let width = 72.min(area.width.saturating_sub(4));
    let inner = width.saturating_sub(4) as usize;
    let rect = centered(area, width, 7);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from(Span::styled(
            format!("Already exists on {}:", copy.plan.dst_label),
            Style::default().fg(theme.fg),
        )),
        Line::from(Span::styled(
            truncate_left(&dst, inner),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            truncate("o overwrite  s skip  a overwrite all  n skip all  Esc cancel copy", inner),
            theme.muted(),
        )),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.warning_fg())
        .style(Style::default().bg(theme.bg))
        .title(" Replace? ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

fn centered(area: Rect, width: u16, height: u16) -> Rect {
    Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height.min(area.height),
    )
}

/// "New folder in host:/dir" with the name being typed (#546).
fn render_new_folder(
    f: &mut Frame,
    theme: &crate::ui::Theme,
    place: &str,
    input: &str,
    error: Option<&str>,
    area: Rect,
) {
    let width = 72.min(area.width.saturating_sub(4));
    let inner = width.saturating_sub(4) as usize;
    let rect = centered(area, width, 7);
    f.render_widget(Clear, rect);
    let lines = vec![
        Line::from(Span::styled(
            truncate_left(&format!("in {place}"), inner),
            Style::default().fg(theme.fg),
        )),
        Line::from(Span::styled(
            truncate_left(&format!("> {input}_"), inner),
            Style::default().fg(theme.accent).add_modifier(Modifier::BOLD),
        )),
        match error {
            Some(e) => Line::from(Span::styled(truncate(e, inner), theme.warning_fg())),
            None => Line::from(""),
        },
        Line::from(Span::styled("Enter create   Esc cancel", theme.muted())),
    ];
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.bg))
        .title(" New folder ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// Masked password / passphrase box over the panels.
fn render_prompt(f: &mut Frame, theme: &crate::ui::Theme, prompt: &SecretPrompt, area: Rect) {
    let width = 60.min(area.width.saturating_sub(4));
    let height = 6;
    let rect = Rect::new(
        area.x + (area.width.saturating_sub(width)) / 2,
        area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    );
    f.render_widget(Clear, rect);
    let what = if prompt.is_key() {
        format!("Passphrase for the SSH key of {}", prompt.target.name)
    } else {
        format!("Password for {}@{}", prompt.target.user, prompt.target.host)
    };
    let mut lines = vec![Line::from(Span::styled(
        truncate(&what, width.saturating_sub(4) as usize),
        Style::default().fg(theme.fg),
    ))];
    if prompt.retry {
        lines.push(Line::from(Span::styled(
            "Wrong passphrase — try again",
            theme.warning_fg(),
        )));
    } else {
        lines.push(Line::from(""));
    }
    lines.push(Line::from(Span::styled(
        format!("> {}", "*".repeat(prompt.typed_len())),
        Style::default().fg(theme.accent),
    )));
    lines.push(Line::from(Span::styled(
        "Enter connect   Esc cancel",
        theme.muted(),
    )));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent))
        .style(Style::default().bg(theme.bg))
        .title(" Not saved — enter once ");
    f.render_widget(Paragraph::new(lines).block(block), rect);
}

/// The longest key hint that fits the width.
fn fit_hint(width: usize) -> String {
    const VARIANTS: [&str; 5] = [
        " Tab panel  Enter open  Bksp up  Space mark  F5/c copy  F7/m folder  r refresh  Ctrl+O host  Esc close ",
        " Tab  Enter  Bksp  Space mark  F5/c copy  F7/m folder  r  ^O host  Esc ",
        " Tab  Enter  Bksp  Space  F5/c copy  F7/m folder  Esc ",
        " Tab  Enter  Bksp  r  ^O host  Esc ",
        " Esc ",
    ];
    VARIANTS
        .iter()
        .find(|v| v.chars().count() <= width)
        .map(|v| v.to_string())
        .unwrap_or_default()
}

/// Cut `s` to `max` columns with an ellipsis at the end.
fn truncate(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut w = 0;
    for c in s.chars() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw + 1 > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('\u{2026}');
    out
}

/// Cut `s` to `max` columns keeping its end (paths: the tail matters).
fn truncate_left(s: &str, max: usize) -> String {
    if s.width() <= max {
        return s.to_string();
    }
    let chars: Vec<char> = s.chars().collect();
    let mut out: Vec<char> = Vec::new();
    let mut w = 1;
    for &c in chars.iter().rev() {
        let cw = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if w + cw > max {
            break;
        }
        out.push(c);
        w += cw;
    }
    out.push('\u{2026}');
    out.into_iter().rev().collect()
}

/// Pad with spaces to `width` display columns (format! pads by chars).
fn pad_to(s: &str, width: usize) -> String {
    let w = s.width();
    if w >= width {
        s.to_string()
    } else {
        format!("{s}{}", " ".repeat(width - w))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_keeps_width() {
        assert_eq!(truncate("abcdef", 4), "abc\u{2026}");
        assert_eq!(truncate("abc", 4), "abc");
        assert_eq!(
            truncate_left(" host:/very/long/path ", 10),
            "\u{2026}ong/path "
        );
        assert_eq!(pad_to("файл", 6), "файл  ");
    }

    #[test]
    fn the_hint_always_fits() {
        for w in [0, 5, 20, 40, 80, 200] {
            assert!(fit_hint(w).chars().count() <= w);
        }
    }
}
