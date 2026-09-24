//! Side-panel renderer (#431).
//!
//! Draws whatever [`crate::side_panel::PanelContent`] the panel holds. For
//! operations: the "operation → hosts" tree on top, the selected host's
//! output tail below. Every state carries a glyph *and* a word, so nothing
//! is told apart by colour alone — the ASCII glyph set included.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::App;
use crate::ops::{HostOpState, Operation};
use crate::side_panel::{tree_rows, PanelContent, TreeRow};
use crate::ui::text::strip_emoji;
use crate::ui::theme::Glyphs;

/// Render the side panel into `area`.
pub(crate) fn render_side_panel(f: &mut Frame, app: &App, area: Rect) {
    render_side_panel_with_glyphs(f, app, area, app.theme.glyphs());
}

/// Render with an explicit glyph set so tests can pin the ASCII fallback.
fn render_side_panel_with_glyphs(f: &mut Frame, app: &App, area: Rect, glyphs: &Glyphs) {
    f.render_widget(Clear, area);
    let focused = app.side_panel.open;
    let border = if focused {
        Style::default().fg(app.theme.accent)
    } else {
        app.theme.muted()
    };
    let title = match app.side_panel.content {
        PanelContent::Operations => " operations ",
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(border)
        .title(Span::styled(title, app.theme.dim()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    match app.side_panel.content {
        PanelContent::Operations => render_operations(f, app, inner, glyphs),
    }
}

/// Glyph for a state.
pub(crate) fn state_glyph(state: HostOpState, glyphs: &Glyphs) -> &'static str {
    match state {
        HostOpState::Running => glyphs.op_running,
        HostOpState::Done => glyphs.op_done,
        HostOpState::Failed => glyphs.op_failed,
        HostOpState::Cancelled => glyphs.op_cancelled,
    }
}

fn state_style(app: &App, state: HostOpState) -> Style {
    match state {
        HostOpState::Running => Style::default().fg(app.theme.accent),
        HostOpState::Done => app.theme.success_fg(),
        HostOpState::Failed => Style::default().fg(app.theme.danger),
        HostOpState::Cancelled => app.theme.muted(),
    }
}

fn render_operations(f: &mut Frame, app: &App, area: Rect, glyphs: &Glyphs) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let ops = &app.operations;
    if ops.is_empty() {
        let lines = vec![
            Line::from(Span::styled(" No background jobs.", app.theme.muted())),
            Line::from(Span::styled(" ^J / Esc close", app.theme.muted())),
        ];
        f.render_widget(Paragraph::new(lines), area);
        return;
    }

    let rows = tree_rows(ops);
    // Tree on top — at most half the panel, so the tail always has room.
    let tree_h = (rows.len() as u16).min((area.height / 2).max(1));
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(tree_h),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(area);

    let width = area.width as usize;
    let selected = app.side_panel.selected.min(rows.len().saturating_sub(1));
    // Keep the selected row visible.
    let offset = selected.saturating_sub(tree_h.saturating_sub(1) as usize);
    let tree_lines: Vec<Line> = rows
        .iter()
        .enumerate()
        .skip(offset)
        .take(tree_h as usize)
        .map(|(i, row)| {
            let (indent, glyph, state, text) = row_text(ops, *row, glyphs);
            let used = indent.width() + glyph.width();
            let mut line = Line::from(vec![
                Span::raw(indent),
                Span::styled(glyph, state_style(app, state)),
                Span::raw(fit(&text, width.saturating_sub(used))),
            ]);
            if i == selected && app.side_panel.open {
                line = line.style(Style::default().add_modifier(Modifier::REVERSED));
            }
            line
        })
        .collect();
    f.render_widget(Paragraph::new(tree_lines), chunks[0]);

    let sep = glyphs.separator.repeat(width / glyphs.separator.width().max(1));
    f.render_widget(Paragraph::new(Span::styled(sep, app.theme.muted())), chunks[1]);

    let (op_idx, host_idx) = rows[selected].target();
    render_tail(f, app, chunks[2], &ops[op_idx], host_idx, glyphs);
}

/// Indent, glyph, state and text of a tree row.
fn row_text(ops: &[Operation], row: TreeRow, glyphs: &Glyphs) -> (&'static str, &'static str, HostOpState, String) {
    match row {
        TreeRow::Operation(i) => {
            let op = &ops[i];
            let state = op.state();
            (
                "",
                state_glyph(state, glyphs),
                state,
                format!(" {} {}", op.id, one_line(&op.label)),
            )
        }
        TreeRow::Host(i, h) => {
            let host = &ops[i].hosts[h];
            (
                "  ",
                state_glyph(host.state, glyphs),
                host.state,
                format!(" {} {} {}", host.name, glyphs.middle_dot, host_state_text(host)),
            )
        }
    }
}

/// "running", "done", "failed (exit 3)", "running (as of last poll)" …
fn host_state_text(host: &crate::ops::OpHost) -> String {
    let mut s = host.state.label().to_string();
    if let Some(code) = host.exit_code {
        if host.state == HostOpState::Failed {
            s.push_str(&format!(" (exit {code})"));
        }
    }
    if host.stale {
        s.push_str(" (as of last poll)");
    }
    s
}

fn render_tail(f: &mut Frame, app: &App, area: Rect, op: &Operation, host_idx: usize, glyphs: &Glyphs) {
    let Some(host) = op.hosts.get(host_idx) else {
        return;
    };
    if area.height == 0 {
        return;
    }
    let width = area.width as usize;
    let glyph = state_glyph(host.state, glyphs);
    let header = Line::from(vec![
        Span::styled(glyph.to_string(), state_style(app, host.state)),
        Span::styled(
            fit(
                &format!(
                    " {} {d} {} {d} {}",
                    host.name,
                    op.id,
                    host_state_text(host),
                    d = glyphs.middle_dot
                ),
                width.saturating_sub(glyph.width()),
            ),
            app.theme.dim(),
        ),
    ]);
    let body_h = area.height.saturating_sub(1) as usize;
    let clean = strip_emoji(&host.tail).replace('\t', "    ");
    let all: Vec<&str> = clean.lines().collect();
    let mut lines = vec![header];
    if all.is_empty() {
        lines.push(Line::from(Span::styled(" (no output yet)", app.theme.muted())));
    } else {
        for l in &all[all.len().saturating_sub(body_h)..] {
            lines.push(Line::from(Span::raw(fit(&format!(" {l}"), width))));
        }
    }
    f.render_widget(Paragraph::new(lines), area);
}

/// First line of a (possibly multi-line) label.
fn one_line(s: &str) -> String {
    let first = s.lines().next().unwrap_or("");
    let cleaned = strip_emoji(first);
    if s.lines().nth(1).is_some() {
        format!("{cleaned} …")
    } else {
        cleaned
    }
}

/// Truncate `s` to `width` display columns, marking the cut with `…`.
fn fit(s: &str, width: usize) -> String {
    if s.width() <= width {
        return s.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = c.width().unwrap_or(0);
        if used + w > width.saturating_sub(1) {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::SessionId;
    use crate::ops::OpHost;
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn host(name: &str, state: HostOpState, exit: Option<i32>, tail: &str) -> OpHost {
        OpHost { name: name.into(), state, exit_code: exit, tail: tail.into(), stale: false }
    }

    fn app_with_ops() -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.operations = vec![
            Operation {
                session: SessionId(1),
                id: "job-1".into(),
                label: "tail -f /var/log/syslog".into(),
                hosts: vec![host("web-01", HostOpState::Running, None, "line a\nline b")],
            },
            Operation {
                session: SessionId(1),
                id: "job-2".into(),
                label: "make".into(),
                hosts: vec![host("web-01", HostOpState::Done, Some(0), "")],
            },
            Operation {
                session: SessionId(1),
                id: "job-3".into(),
                label: "false".into(),
                hosts: vec![host("db", HostOpState::Failed, Some(1), "")],
            },
            Operation {
                session: SessionId(1),
                id: "job-4".into(),
                label: "sleep 99".into(),
                hosts: vec![host("db", HostOpState::Cancelled, None, "")],
            },
        ];
        app
    }

    fn render(app: &App, w: u16, h: u16, glyphs: &Glyphs) -> String {
        let mut term = Terminal::new(TestBackend::new(w, h)).unwrap();
        term.draw(|f| render_side_panel_with_glyphs(f, app, f.area(), glyphs))
            .unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..h {
            for x in 0..w {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn every_state_is_distinguishable_in_ascii_mode() {
        let app = app_with_ops();
        let text = render(&app, 40, 20, &Glyphs::ASCII);
        for bad in ["▸", "✓", "✗", "■", "·", "─"] {
            assert!(!text.contains(bad), "ASCII mode must not draw {bad:?}:\n{text}");
        }
        // Glyph + word per state — no colour needed.
        assert!(text.contains("> job-1"), "{text}");
        assert!(text.contains("+ job-2"), "{text}");
        assert!(text.contains("! job-3"), "{text}");
        assert!(text.contains("~ job-4"), "{text}");
        assert!(text.contains("web-01 - running"), "{text}");
        assert!(text.contains("db - failed (exit 1)"), "{text}");
        assert!(text.contains("db - cancelled"), "{text}");
    }

    #[test]
    fn tail_of_the_selected_host_is_shown_below_the_tree() {
        let mut app = app_with_ops();
        app.side_panel.selected = 1; // job-1's host row
        let text = render(&app, 40, 20, &Glyphs::UNICODE);
        assert!(text.contains("line a"), "{text}");
        assert!(text.contains("line b"), "{text}");
        app.side_panel.selected = 2; // job-2, no output
        let text = render(&app, 40, 20, &Glyphs::UNICODE);
        assert!(text.contains("(no output yet)"), "{text}");
    }

    #[test]
    fn stale_remote_state_is_labelled() {
        let mut app = app_with_ops();
        app.operations[0].hosts[0].stale = true;
        app.side_panel.selected = 0;
        let text = render(&app, 60, 20, &Glyphs::UNICODE);
        assert!(text.contains("as of last poll"), "{text}");
    }

    #[test]
    fn empty_panel_says_so() {
        let app = App::new("test".into(), CommandConfirmMode::Always);
        let text = render(&app, 40, 6, &Glyphs::UNICODE);
        assert!(text.contains("No background jobs."), "{text}");
    }

    #[test]
    fn host_output_cannot_inject_control_sequences() {
        let mut app = app_with_ops();
        app.operations[0].hosts[0].tail = "ok\x1b[2Jboom\r".into();
        app.side_panel.selected = 1;
        let text = render(&app, 40, 20, &Glyphs::UNICODE);
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(text.contains("ok[2Jboom"), "{text}");
    }

    #[test]
    fn fit_truncates_by_display_width() {
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 4), "abc");
        assert_eq!(fit("абвгд", 3), "аб…");
        assert_eq!(fit("x", 0), "");
    }
}
