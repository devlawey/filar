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
use crate::side_panel::{summary_rows, tree_rows, PanelContent, SummaryRow, TreeRow};
use crate::ui::text::{sanitize_output, strip_emoji, wrap_text};
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
    let content = app.panel_content();
    let title = match content {
        PanelContent::Operations => " operations ",
        PanelContent::FleetSummary => " fleet summary ",
    };
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(border)
        .title(Span::styled(title, app.theme.dim()));
    let inner = block.inner(area);
    f.render_widget(block, area);
    match content {
        PanelContent::Operations => render_operations(f, app, inner, glyphs),
        PanelContent::FleetSummary => render_fleet_summary(f, app, inner, glyphs),
    }
}

/// Most lines an expanded group shows; the rest are counted, not drawn.
const MAX_EXPANDED_SAMPLE_LINES: usize = 200;

/// The fleet's difference table (#438): an aggregate, not twelve streams.
///
/// One entry per group of hosts that answered the same — its hosts and a
/// sample answer, one clamped line until expanded — then every host with
/// no value (`no contact`, `n/a`, `cancelled`, …) in a block of its own,
/// so none of them is lost among the groups. Every mark comes with a word,
/// and the ASCII glyph set draws the same table.
fn render_fleet_summary(f: &mut Frame, app: &App, area: Rect, glyphs: &Glyphs) {
    use filar_agent::fleet_view::GroupRole;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let Some(view) = app.active_session().fleet_view.as_ref() else {
        return;
    };
    let width = area.width as usize;
    let rows = summary_rows(view);
    let selected = app.side_panel.selected.min(rows.len().saturating_sub(1));

    // Header: the command and the headline, which always says how many
    // hosts did not answer (#428).
    let mut head: Vec<Line> = vec![Line::from(Span::styled(
        fit(&format!(" $ {}", one_line(&view.command)), width),
        app.theme.command_style(),
    ))];
    for l in wrap_text(&view.headline, width.saturating_sub(1).max(1)) {
        head.push(Line::from(Span::styled(format!(" {l}"), app.theme.dim())));
    }
    let sep = glyphs.separator.repeat(width / glyphs.separator.width().max(1));
    head.push(Line::from(Span::styled(sep.clone(), app.theme.muted())));

    // Body: one block of lines per selectable row, remembering where each
    // row starts so the selection can be scrolled into view.
    let mut body: Vec<Line> = Vec::new();
    let mut starts: Vec<usize> = Vec::with_capacity(rows.len());
    if view.groups.is_empty() {
        body.push(Line::from(Span::styled(" no host answered", app.theme.muted())));
    }
    for (i, row) in rows.iter().enumerate() {
        if let SummaryRow::Dropped(0) = row {
            body.push(Line::from(Span::styled(" not compared:", app.theme.dim())));
        }
        starts.push(body.len());
        let highlight = i == selected && app.side_panel.open;
        match *row {
            SummaryRow::Group(g) => {
                let group = &view.groups[g];
                let (mark, word, style) = match group.role {
                    GroupRole::Baseline => (glyphs.fleet_same, "same on", app.theme.success_fg()),
                    GroupRole::Differs => (glyphs.fleet_differs, "differs on", Style::default().fg(app.theme.danger)),
                    GroupRole::Split => (glyphs.fleet_split, "split, group of", app.theme.warning_fg()),
                };
                let expanded = app.side_panel.expanded.contains(&g);
                // Host output: control characters and escapes go, like in the
                // feed's command blocks — but every script stays. `strip_emoji`
                // would drop CJK and report a real answer as empty.
                let lines: Vec<String> = sanitize_output(&group.sample)
                    .replace('\t', "    ")
                    .lines()
                    .map(str::to_string)
                    .collect();
                let arrow = if expanded { glyphs.expand_arrow } else { glyphs.collapse_arrow };
                let title = format!(
                    " {} {} {}",
                    word,
                    group.hosts.len(),
                    group.hosts.join(", ")
                );
                let mut line = Line::from(vec![
                    Span::styled(format!("{mark} "), style),
                    Span::styled(arrow, app.theme.muted()),
                    Span::raw(fit(&title, width.saturating_sub(mark.width() + 1 + arrow.width()))),
                ]);
                if highlight {
                    line = line.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                body.push(line);
                if lines.is_empty() {
                    body.push(Line::from(Span::styled("   (empty output)", app.theme.muted())));
                } else if expanded {
                    for l in lines.iter().take(MAX_EXPANDED_SAMPLE_LINES) {
                        for w in wrap_text(l, width.saturating_sub(3).max(1)) {
                            body.push(Line::from(Span::raw(format!("   {w}"))));
                        }
                    }
                    if lines.len() > MAX_EXPANDED_SAMPLE_LINES {
                        body.push(Line::from(Span::styled(
                            format!("   … {} more lines", lines.len() - MAX_EXPANDED_SAMPLE_LINES),
                            app.theme.muted(),
                        )));
                    }
                } else {
                    // One line, clamped; more is said, not hidden.
                    let first = if lines.len() > 1 {
                        format!("{} (+{} lines)", lines[0], lines.len() - 1)
                    } else {
                        lines[0].clone()
                    };
                    body.push(Line::from(Span::raw(fit(&format!("   {first}"), width))));
                }
            }
            SummaryRow::Dropped(d) => {
                let host = &view.dropped[d];
                let (mark, style) = dropped_mark(app, host.state, glyphs);
                let mut line = Line::from(vec![
                    Span::styled(format!(" {mark}"), style),
                    Span::raw(fit(
                        &format!(" {} {} {}", host.host, glyphs.middle_dot, host.state.label()),
                        width.saturating_sub(mark.width() + 1),
                    )),
                ]);
                if highlight {
                    line = line.style(Style::default().add_modifier(Modifier::REVERSED));
                }
                body.push(line);
            }
        }
    }

    let hint = Line::from(Span::styled(
        fit(
            &format!(
 " {}{} select {d} Enter expand {d} PgUp/PgDn scroll {d} Esc close",
                glyphs.arrow_up,
                glyphs.arrow_down,
                d = glyphs.middle_dot
            ),
            width,
        ),
        app.theme.muted(),
    ));

    // Scroll the body so the selected row's first line stays visible, then
    // by `PgUp`/`PgDn` on top — so an expanded answer taller than the panel
    // can be read line by line rather than skipped past.
    let body_h = (area.height as usize).saturating_sub(head.len() + 1);
    let anchor = starts.get(selected).copied().unwrap_or(0);
    let max_offset = body.len().saturating_sub(body_h);
    let offset = (anchor.saturating_sub(body_h.saturating_sub(2)) + app.side_panel.scroll)
        .min(max_offset);
    let mut lines = head;
    lines.extend(body.into_iter().skip(offset).take(body_h));
    while lines.len() + 1 < area.height as usize {
        lines.push(Line::from(""));
    }
    lines.push(hint);
    f.render_widget(Paragraph::new(lines), area);
}

/// Mark for a host without a value: silence and errors stand out, a host
/// nobody asked (or the user stopped) does not.
fn dropped_mark(app: &App, state: filar_agent::fleet_result::HostState, glyphs: &Glyphs) -> (&'static str, Style) {
    use filar_agent::fleet_result::HostState;
    match state {
        HostState::NoContact | HostState::TimedOut | HostState::ExecutionError => {
            (glyphs.op_failed, Style::default().fg(app.theme.danger))
        }
        HostState::Cancelled => (glyphs.op_cancelled, app.theme.muted()),
        _ => (glyphs.middle_dot, app.theme.muted()),
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
        OpHost { name: name.into(), state, exit_code: exit, tail: tail.into(), stale: false, settled: true }
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

    // ── Fleet summary (#438) ───────────────────────────────────────

    #[test]
    fn the_fleet_table_reads_at_eighty_columns() {
        // At 80 columns the drawer is the feed minus an 8-column strip.
        let app = crate::app::test_fleet_app(crate::app::test_fleet_view());
        let text = render(&app, 72, 24, Glyphs::detect());
        assert!(text.contains("fleet summary"), "{text}");
        assert!(text.contains("$ cat /etc/os-release"), "{text}");
        assert!(text.contains("same on 2 web-1, web-2"), "{text}");
        assert!(text.contains("differs on 1 web-3"), "{text}");
        // The long answer is clamped to one line, and says there is more.
        assert!(text.contains("…"), "{text}");
        assert!(!text.contains("VERSION_ID"), "collapsed: only the first line: {text}");
        // Hosts without a value are listed apart, each with its reason.
        assert!(text.contains("not compared:"), "{text}");
        assert!(text.contains("db-1") && text.contains("no contact"), "{text}");
        assert!(text.contains("win-1") && text.contains("n/a"), "{text}");
        for line in text.lines() {
            assert!(line.chars().count() <= 72, "nothing spills past the panel: {line:?}");
        }
    }

    #[test]
    fn an_expanded_group_shows_its_whole_answer() {
        let mut app = crate::app::test_fleet_app(crate::app::test_fleet_view());
        app.side_panel.expanded.insert(1);
        let text = render(&app, 72, 30, Glyphs::detect());
        assert!(text.contains("VERSION_ID=\"22.04\""), "{text}");
        assert!(text.contains("ID=ubuntu"), "{text}");
    }

    #[test]
    fn the_fleet_table_is_drawn_in_ascii_mode() {
        let app = crate::app::test_fleet_app(crate::app::test_fleet_view());
        let text = render(&app, 72, 24, &Glyphs::ASCII);
        for bad in ["≠", "≈", "▸", "▾", "·", "─", "✗", "↑", "↓"] {
            assert!(!text.contains(bad), "ASCII mode must not draw {bad:?}:\n{text}");
        }
        // Marks keep their words, so nothing depends on a glyph or colour.
        assert!(text.contains("= +") && text.contains("same on"), "{text}");
        assert!(text.contains("!=") && text.contains("differs on"), "{text}");
        assert!(text.contains("no contact"), "{text}");
    }

    #[test]
    fn a_non_latin_answer_is_shown_not_reported_empty() {
        let mut view = crate::app::test_fleet_view();
        view.groups[0].sample = "你好，世界".into();
        let app = crate::app::test_fleet_app(view);
        let text = render(&app, 72, 24, Glyphs::detect());
        assert!(!text.contains("(empty output)"), "{text}");
        assert!(text.contains('你') && text.contains('界'), "{text}");
    }

    #[test]
    fn a_tall_expanded_answer_can_be_read_to_the_end() {
        let mut view = crate::app::test_fleet_view();
        view.groups[1].sample = (1..=60).map(|i| format!("line-{i:02}")).collect::<Vec<_>>().join("\n");
        let mut app = crate::app::test_fleet_app(view);
        app.side_panel.open = true;
        app.side_panel.selected = 1;
        app.side_panel.expanded.insert(1);
        let text = render(&app, 72, 20, Glyphs::detect());
        assert!(text.contains("line-01") && !text.contains("line-30"), "{text}");
        // PgDn scrolls the body: the middle of the answer comes into view,
        // and past it the end, without moving to the next row.
        app.side_panel.scroll = 25;
        let text = render(&app, 72, 20, Glyphs::detect());
        assert!(text.contains("line-30"), "{text}");
        app.side_panel.scroll = 1000;
        let text = render(&app, 72, 20, Glyphs::detect());
        assert!(text.contains("line-60"), "{text}");
        assert!(text.contains("PgUp/PgDn"), "the hint names the keys: {text}");
    }
}
