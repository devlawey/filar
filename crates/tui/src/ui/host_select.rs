//! Host-selection overlay — modal window for choosing an SSH target.
//!
//! Opened via `Ctrl+O` in Normal mode. Shows `local` plus all configured
//! `[[ssh_targets]]`, grouped under their primary tag with group headers.
//! Typing filters by substring (name / `user@host:port` / tags), `Tab`
//! cycles the tag filter, Up/Down navigate and Enter confirms (#415).

use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph};
use ratatui::Frame;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{App, HostSelectRow};
use crate::ui::theme::Glyphs;

/// Upper bound for the overlay width; long rows widen it up to this.
const OVERLAY_MAX_WIDTH: u16 = 84;
/// Lower bound so the search line and footer hints always fit.
const OVERLAY_MIN_WIDTH: u16 = 46;
const OVERLAY_H_MARGIN: u16 = 6;
const OVERLAY_V_MARGIN: u16 = 4;

/// Render the host-selection overlay on top of the current frame.
pub(crate) fn render_host_select(f: &mut Frame, app: &App, area: Rect) {
    render_host_select_with_glyphs(f, app, area, app.theme.glyphs());
}

/// Render with an explicit glyph set so tests can pin ASCII fallbacks (#415).
fn render_host_select_with_glyphs(f: &mut Frame, app: &App, area: Rect, glyphs: &Glyphs) {
    let rows = app.host_select_rows();

    // Width: measured on the unfiltered view so the frame does not jump when
    // rows drop out of the filtered view while the query is typed.
    let content_width = app
        .host_select_rows_filtered("", None)
        .iter()
        .map(|r| row_line(app, r, glyphs).width())
        .max()
        .unwrap_or(0) as u16;
    let max_width = OVERLAY_MAX_WIDTH.min(area.width.saturating_sub(2 * OVERLAY_H_MARGIN));
    let width = (content_width + 2).clamp(OVERLAY_MIN_WIDTH.min(max_width), max_width);

    // Height: border (2) + search line (1) + rows + footer (1). An empty
    // view still shows the "(no matches)" line, so it reserves one row —
    // otherwise the footer hints are squeezed out entirely (#415 live run).
    let cap = area.height.saturating_sub(2 * OVERLAY_V_MARGIN);
    let height = (rows.len().max(1) as u16).saturating_add(4).min(cap);
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    // In the fleet, choosing a host opens it in a new tab (#433).
    let title = if app.in_fleet() {
        " Open a fleet host in a new tab (Ctrl+O) "
    } else {
        " Select host (Ctrl+O) "
    };
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            title,
            Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(overlay_area);
    f.render_widget(block, overlay_area);
    if inner.height == 0 {
        return;
    }
    let chunks = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .split(inner);

    // ── Search line: query (or placeholder) on the left, cursor position on
    // the right. Inner width — the block borders take two columns.
    f.render_widget(
        Paragraph::new(search_line(app, &rows, chunks[0].width)),
        chunks[0],
    );

    // ── List: grouped rows; group headers are not selectable.
    let mut items: Vec<ListItem> = Vec::with_capacity(rows.len().max(1));
    if rows.is_empty() {
        items.push(ListItem::new(Line::from(vec![
            Span::raw("  "),
            Span::styled("(no matches)", app.theme.muted()),
        ])));
    } else {
        for r in &rows {
            items.push(ListItem::new(row_line(app, r, glyphs)));
        }
    }

    let mut state = ListState::default();
    state.select(app.host_select_visible_pos(&rows));

    let list = List::new(items).highlight_style(Style::default().bg(app.theme.selection_bg));
    f.render_stateful_widget(list, chunks[1], &mut state);

    // ── Footer hints (the Tab segment shows the active tag filter).
    f.render_widget(
        Paragraph::new(Span::styled(footer_text(app, glyphs), app.theme.muted())),
        chunks[2],
    );
}

/// Build the search line: ` search: <query|placeholder>` with the cursor
/// position (`n/visible`) right-aligned (#415).
fn search_line(app: &App, rows: &[HostSelectRow], width: u16) -> Line<'static> {
    let selectable: Vec<usize> = rows
        .iter()
        .filter_map(HostSelectRow::selection_index)
        .collect();
    let pos = selectable
        .iter()
        .position(|&i| i == app.host_select_index)
        .map(|p| p + 1)
        .unwrap_or(0);
    let counter = format!("{}/{}", pos, selectable.len());

    let label = " search: ";
    let total = width as usize;
    let body_max = total.saturating_sub(label.len() + counter.len() + 2);
    let (body, style) = if app.host_select_query.is_empty() {
        ("type to filter".to_string(), app.theme.muted())
    } else {
        (fit_tail(&app.host_select_query, body_max), app.theme.fg_style())
    };
    let body_width = UnicodeWidthStr::width(body.as_str());
    let pad = total.saturating_sub(label.len() + body_width + counter.len() + 1);
    Line::from(vec![
        Span::styled(label, app.theme.muted()),
        Span::styled(body, style),
        Span::raw(" ".repeat(pad)),
        Span::styled(counter, app.theme.muted()),
        Span::raw(" "),
    ])
}

/// Footer hints; the `Tab` segment shows the active tag filter (#415).
fn footer_text(app: &App, glyphs: &Glyphs) -> String {
    let tab = match &app.host_select_tag_filter {
        Some(tag) => format!("Tab: {}", tag),
        None => "Tab tag".to_string(),
    };
    format!(
        " {up}{down} {dot} Enter {dot} {tab} {dot} Esc clear ",
        up = glyphs.arrow_up,
        down = glyphs.arrow_down,
        dot = glyphs.middle_dot,
    )
}

/// Keep the tail of `s` inside `max` display columns — the search field
/// scrolls from the left, like a terminal line editor (wide chars counted
/// by display width).
fn fit_tail(s: &str, max: usize) -> String {
    if UnicodeWidthStr::width(s) <= max {
        return s.to_string();
    }
    let mut width = 0usize;
    let mut tail: Vec<char> = Vec::new();
    for ch in s.chars().rev() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > max {
            break;
        }
        width += w;
        tail.push(ch);
    }
    tail.reverse();
    tail.into_iter().collect()
}

/// One rendered list row (#415): a group header, `local`, or an SSH target
/// with its `user@host:port`, auth kind and tags.
fn row_line(app: &App, row: &HostSelectRow, glyphs: &Glyphs) -> Line<'static> {
    match row {
        HostSelectRow::Header { tag, count } => {
            let label = match tag {
                Some(t) => format!("{} ({})", t, count),
                None => format!("no tags ({})", count),
            };
            Line::from(vec![
                Span::raw(format!(" {}{} ", glyphs.separator, glyphs.separator)),
                Span::styled(
                    label,
                    Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD),
                ),
            ])
        }
        HostSelectRow::Local => {
            Line::from(vec![
                cursor_span(app, glyphs, 0),
                current_span(glyphs, app.ssh_info.is_none() && !app.in_fleet()),
                Span::raw(" "),
                Span::styled(
                    "local",
                    Style::default().fg(app.theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled("Local machine", app.theme.muted()),
            ])
        }
        HostSelectRow::Target(i) => {
            let Some(t) = app.ssh_targets.get(*i) else {
                return Line::default();
            };
            let is_current = !app.in_fleet()
                && app
                    .ssh_info
                    .as_ref()
                    .map(|info| *info == format!("{}@{}:{}", t.user, t.host, t.port))
                    .unwrap_or(false);
            let auth_label = match &t.auth {
                filar_core::SshAuth::Agent => "Agent",
                filar_core::SshAuth::Key { .. } => "Key",
                filar_core::SshAuth::Password { .. } => "Password",
            };
            let mut spans = vec![
                cursor_span(app, glyphs, i + 1),
                current_span(glyphs, is_current),
                Span::raw(" "),
                Span::styled(
                    t.name.clone(),
                    Style::default().fg(app.theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(format!("{}@{}:{}", t.user, t.host, t.port), app.theme.muted()),
                Span::raw("  "),
                Span::styled(format!("[{}]", auth_label), app.theme.dim()),
            ];
            for tag in &t.tags {
                spans.push(Span::styled(format!("  #{}", tag), app.theme.dim()));
            }
            Line::from(spans)
        }
        HostSelectRow::GroupsHeader { count } => Line::from(vec![
            Span::raw(format!(" {}{} ", glyphs.separator, glyphs.separator)),
            Span::styled(
                format!("groups — fleet ({count})"),
                Style::default().fg(app.theme.accent).add_modifier(Modifier::BOLD),
            ),
        ]),
        HostSelectRow::Group { group, selection } => {
            let Some(g) = app.host_groups.get(*group) else {
                return Line::default();
            };
            let is_current = app.in_fleet() && app.fleet().map(|f| f.group_name()) == Some(g.name.as_str());
            let members = filar_core::select_hosts_for_group(g, &app.ssh_targets).len();
            let rule: Vec<String> = g.match_tags.iter().map(|t| format!("#{t}")).collect();
            Line::from(vec![
                cursor_span(app, glyphs, *selection),
                current_span(glyphs, is_current),
                Span::raw(" "),
                Span::styled(
                    g.name.clone(),
                    Style::default().fg(app.theme.fg).add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(format!("{members} host(s)"), app.theme.muted()),
                Span::raw("  "),
                Span::styled(rule.join(" "), app.theme.dim()),
            ])
        }
    }
}

/// Row cursor: `▶`/`>` on the row carrying the flat selection index.
fn cursor_span(app: &App, glyphs: &Glyphs, index: usize) -> Span<'static> {
    let cursor = if app.host_select_index == index { glyphs.cursor } else { " " };
    Span::raw(cursor)
}

/// Active-target dot: `●`/`*` with a leading space, blank otherwise.
fn current_span(glyphs: &Glyphs, is_current: bool) -> Span<'static> {
    if is_current {
        Span::raw(format!(" {}", glyphs.current))
    } else {
        Span::raw("  ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn make_target(name: &str, tags: &[&str]) -> filar_core::SshTarget {
        filar_core::SshTarget {
            name: name.into(),
            host: "host".into(),
            port: 22,
            user: "user".into(),
            auth: filar_core::SshAuth::Agent,
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: tags.iter().map(|s| (*s).to_string()).collect(),
        }
    }

    fn render_to_text(app: &App, width: u16, height: u16, glyphs: &Glyphs) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("TestBackend terminal");
        let area = Rect::new(0, 0, width, height);
        terminal
            .draw(|f| render_host_select_with_glyphs(f, app, area, glyphs))
            .expect("draw host select");
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

    #[test]
    fn overlay_renders_without_risky_unicode_in_ascii_mode() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.ssh_targets = vec![
            make_target("web-01", &["prod"]),
            make_target("db-01", &[]),
        ];
        app.host_select_visible = true;
        let text = render_to_text(&app, 80, 24, &Glyphs::ASCII);
        for bad in ['▶', '●', '↑', '↓', '·', '▸', '▾', '•', '❯', '░', '█'] {
            assert!(!text.contains(bad), "ASCII mode must not draw {bad:?}: {text:?}");
        }
        assert!(text.contains("^v"), "footer uses ASCII arrows: {text:?}");
        assert!(text.contains("prod (1)"), "group header must show: {text:?}");
        assert!(text.contains("#prod"), "row tags must show: {text:?}");
    }

    #[test]
    fn overlay_shows_group_headers_and_untagged_bucket() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.ssh_targets = vec![
            make_target("web-01", &["prod"]),
            make_target("db-01", &[]),
        ];
        app.host_select_visible = true;
        let text = render_to_text(&app, 90, 24, &Glyphs::UNICODE);
        assert!(text.contains("prod (1)"), "tagged group header: {text:?}");
        assert!(text.contains("no tags (1)"), "untagged bucket header: {text:?}");
        assert!(text.contains("Local machine"), "local row stays first: {text:?}");
    }

    #[test]
    fn overlay_scrolls_to_keep_selection_visible_with_24_hosts() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.ssh_targets = (1..=24)
            .map(|i| make_target(&format!("fleet-{i:02}"), &[]))
            .collect();
        app.host_select_visible = true;
        app.host_select_index = 22; // near the end of the flat list
        let text = render_to_text(&app, 90, 16, &Glyphs::UNICODE);
        assert!(text.contains("fleet-22"), "selected row must be visible: {text:?}");
        assert!(!text.contains("fleet-01"), "early rows must have scrolled out: {text:?}");
    }

    #[test]
    fn search_line_counter_fits_the_inner_width() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.ssh_targets = (1..=24)
            .map(|i| make_target(&format!("fleet-{i:02}"), &[]))
            .collect();
        app.host_select_visible = true;
        let text = render_to_text(&app, 90, 30, &Glyphs::UNICODE);
        assert!(
            text.contains("1/25"),
            "counter must render fully inside the borders (was clipped by one char), got: {text:?}"
        );
    }

    #[test]
    fn overlay_empty_state_keeps_footer_hints() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.ssh_targets = vec![make_target("web-01", &["prod"])];
        app.host_select_visible = true;
        app.host_select_query = "zzz".into();
        let text = render_to_text(&app, 90, 30, &Glyphs::UNICODE);
        assert!(text.contains("(no matches)"), "empty view explains itself: {text:?}");
        assert!(
            text.contains("Esc clear"),
            "footer hints must survive the empty view: {text:?}"
        );
    }

    #[test]
    fn fit_tail_keeps_trailing_columns() {
        assert_eq!(fit_tail("abcdef", 3), "def");
        assert_eq!(fit_tail("abc", 5), "abc");
        assert_eq!(fit_tail("", 0), "");
        assert_eq!(fit_tail("abcdef", 0), "");
    }
}
