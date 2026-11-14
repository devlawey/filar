//! The replay-plan preview (#668): a runbook's steps shown against the tab
//! they would run on, before anything runs.
//!
//! The header — target and the tab's effective confirmation mode — and the
//! footer stay in place; the plan between them scrolls like a long
//! confirmation (#658).

use filar_core::CommandConfirmMode;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::runbook_replay::ReplayPreview;
use crate::app::App;

const MAX_WIDTH: u16 = 96;

/// Draw the preview over the frame.
pub(crate) fn render_replay_preview(f: &mut Frame, app: &App, area: Rect) {
    let Some(preview) = app.replay_preview.as_ref() else {
        return;
    };
    let width = MAX_WIDTH.min(area.width.saturating_sub(4)).max(20);
    let inner_width = width.saturating_sub(2) as usize;
    let header = header_lines(app);
    let body = body_lines(app, preview);
    let body_rows: usize = body.iter().map(|l| wrapped_rows(l, inner_width)).sum();
    // header + rule + body + rule + 2 footer rows, inside the frame.
    let chrome = header.len() as u16 + 4 + 2;
    let height = (body_rows as u16)
        .saturating_add(chrome)
        .min(area.height.saturating_sub(2))
        .max(chrome + 1);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let frame = Rect::new(x, y, width, height);
    f.render_widget(Clear, frame);
    let title = format!(" Replay plan — {} ", preview.name);
    f.render_widget(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(app.theme.accent))
            .style(Style::default().bg(app.theme.bg))
            .title(Span::styled(
                truncate(&title, inner_width),
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            )),
        frame,
    );
    let inner = Rect::new(
        frame.x + 1,
        frame.y + 1,
        frame.width.saturating_sub(2),
        frame.height.saturating_sub(2),
    );
    let header_h = header.len() as u16;
    f.render_widget(
        Paragraph::new(header),
        Rect::new(inner.x, inner.y, inner.width, header_h),
    );
    let rule = "─".repeat(inner_width);
    f.render_widget(
        Paragraph::new(Span::styled(rule.clone(), app.theme.muted())),
        Rect::new(inner.x, inner.y + header_h, inner.width, 1),
    );
    let body_h = inner.height.saturating_sub(header_h + 4);
    let max_scroll = (body_rows as u16).saturating_sub(body_h);
    let scroll = preview.scroll.min(max_scroll);
    f.render_widget(
        Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .scroll((scroll, 0)),
        Rect::new(inner.x, inner.y + header_h + 1, inner.width, body_h),
    );
    let foot_y = inner.y + header_h + 1 + body_h;
    f.render_widget(
        Paragraph::new(Span::styled(rule, app.theme.muted())),
        Rect::new(inner.x, foot_y, inner.width, 1),
    );
    let shown_to = (scroll + body_h).min(body_rows as u16);
    let rows = if body_rows as u16 > body_h {
        format!(
            "rows {}–{} of {} · ↑↓ PgUp PgDn Home End",
            scroll + 1,
            shown_to,
            body_rows
        )
    } else {
        String::new()
    };
    let status = preview.notice.clone().unwrap_or(rows);
    let status_style = if preview.notice.is_some() {
        app.theme.warning_fg()
    } else {
        app.theme.muted()
    };
    f.render_widget(
        Paragraph::new(Span::styled(truncate(&status, inner_width), status_style)),
        Rect::new(inner.x, foot_y + 1, inner.width, 1),
    );
    f.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(" [ Start ] ", app.theme.muted()),
            Span::styled("not available yet (#669)   ", app.theme.muted()),
            Span::styled(
                "[ Cancel ] Esc",
                Style::default()
                    .fg(app.theme.accent)
                    .add_modifier(Modifier::BOLD),
            ),
        ])),
        Rect::new(inner.x, foot_y + 2, inner.width, 1),
    );
}

/// Target and confirmation mode of the active tab — what a replay would
/// run against.
fn header_lines(app: &App) -> Vec<Line<'static>> {
    let prod = app
        .active_ssh_tags()
        .iter()
        .any(|t| t.eq_ignore_ascii_case("prod"));
    let mut target = vec![
        Span::styled("Target: ", app.theme.muted()),
        Span::styled(
            app.status_target(),
            Style::default()
                .fg(app.theme.fg)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if prod {
        target.push(Span::raw(" "));
        target.push(Span::styled(
            "[prod]",
            Style::default()
                .fg(app.theme.danger)
                .add_modifier(Modifier::BOLD),
        ));
    }
    let mode = app.confirm_mode;
    let mut confirm = vec![
        Span::styled("Confirm: ", app.theme.muted()),
        Span::styled(mode_name(mode), Style::default().fg(app.theme.fg)),
    ];
    if mode == CommandConfirmMode::Never {
        confirm.push(Span::styled(
            "  ⚠ commands will run without approval",
            app.theme.warning_fg().add_modifier(Modifier::BOLD),
        ));
    }
    vec![Line::from(target), Line::from(confirm)]
}

/// `always`, `allowlist`, `explain` or `never`, as config.toml spells it.
pub(crate) fn mode_name(mode: CommandConfirmMode) -> &'static str {
    match mode {
        CommandConfirmMode::Always => "always",
        CommandConfirmMode::Allowlist => "allowlist",
        CommandConfirmMode::Never => "never",
        CommandConfirmMode::Explain => "explain",
    }
}

/// The plan: symptom, preconditions, steps, exit criteria, next actions.
pub(crate) fn body_lines(app: &App, preview: &ReplayPreview) -> Vec<Line<'static>> {
    let plan = &preview.plan;
    let bold = Style::default()
        .fg(app.theme.fg)
        .add_modifier(Modifier::BOLD);
    let muted = app.theme.muted();
    let mut out = vec![Line::from(Span::styled(plan.title.clone(), bold))];
    if let Some(symptom) = &plan.symptom {
        out.push(Line::from(vec![
            Span::styled("When: ", muted),
            Span::raw(symptom.clone()),
        ]));
    }
    section(&mut out, "Preconditions", &plan.preconditions, muted);
    out.push(Line::raw(""));
    out.push(Line::from(Span::styled(
        format!("Steps ({})", plan.steps.len()),
        bold,
    )));
    for step in &plan.steps {
        out.push(Line::from(Span::styled(
            format!("{}. {}", step.index, step.title),
            bold,
        )));
        match &step.command {
            Some(cmd) => {
                let mut line = vec![
                    Span::styled("   $ ", muted),
                    Span::styled(cmd.clone(), app.theme.command_style()),
                ];
                if let Some(rating) = &step.original_safety {
                    let style = if rating.contains('⛔') {
                        app.theme.danger_fg()
                    } else if rating.contains('⚠') {
                        app.theme.warning_fg()
                    } else {
                        muted
                    };
                    line.push(Span::styled(format!("  · in runbook: {rating}"), style));
                }
                out.push(Line::from(line));
            }
            None => out.push(Line::from(Span::styled(
                "   (no command — done by hand)",
                muted,
            ))),
        }
        if step.needs_secret {
            out.push(Line::from(Span::styled(
                "   operator: needs <secret> — you perform this step, the agent never fills it in",
                app.theme.warning_fg(),
            )));
        }
        let others: Vec<&str> = step
            .placeholders
            .iter()
            .map(String::as_str)
            .filter(|p| *p != filar_agent::SECRET_PLACEHOLDER)
            .collect();
        if !others.is_empty() {
            out.push(Line::from(Span::styled(
                format!("   placeholders: {}", others.join(", ")),
                muted,
            )));
        }
        if !step.expect.is_empty() {
            out.push(Line::from(Span::styled(
                format!("   look at: {}", step.expect),
                muted,
            )));
        }
    }
    out.push(Line::raw(""));
    section(&mut out, "Exit criteria", &plan.exit_criteria, muted);
    section(&mut out, "Next actions", &plan.next_actions, muted);
    out
}

fn section(out: &mut Vec<Line<'static>>, name: &str, items: &[String], muted: Style) {
    if items.is_empty() {
        return;
    }
    out.push(Line::from(Span::styled(format!("{name}:"), muted)));
    for item in items {
        out.push(Line::raw(format!("  - {item}")));
    }
}

/// Rows a line takes when wrapped at `width` (at least one).
fn wrapped_rows(line: &Line<'_>, width: usize) -> usize {
    let w = line.width();
    if width == 0 || w == 0 {
        1
    } else {
        w.div_ceil(width)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() > max {
        s.chars()
            .take(max.saturating_sub(1).max(1))
            .collect::<String>()
            + "…"
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_agent::{ReplayPlan, ReplayStep};
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn step(index: usize, command: Option<&str>, rating: Option<&str>, secret: bool) -> ReplayStep {
        ReplayStep {
            index,
            title: format!("step {index}"),
            command: command.map(Into::into),
            expect: if index == 1 {
                "Use% of /".into()
            } else {
                String::new()
            },
            original_safety: rating.map(Into::into),
            placeholders: if secret {
                vec!["<user>".into(), "<secret>".into()]
            } else {
                vec![]
            },
            needs_secret: secret,
        }
    }

    fn app_with_preview(mode: CommandConfirmMode) -> App {
        let mut app = App::new("local".into(), mode);
        app.confirm_mode = mode;
        app.replay_preview = Some(ReplayPreview {
            name: "web-1/a.runbook.md".into(),
            plan: ReplayPlan {
                title: "Disk full".into(),
                symptom: Some("Writes fail with ENOSPC".into()),
                preconditions: vec!["sudo on <host>".into()],
                steps: vec![
                    step(1, Some("df -h /"), Some("safety 98%"), false),
                    step(
                        2,
                        Some("systemctl restart <service>"),
                        Some("safety 34% ⛔ danger"),
                        false,
                    ),
                    step(3, Some("docker login -u <user> -p <secret>"), None, true),
                    step(4, None, None, false),
                ],
                exit_criteria: vec!["Use% below 90%".into()],
                next_actions: vec![],
            },
            scroll: 0,
            notice: None,
        });
        app
    }

    fn screen(app: &App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|f| render_replay_preview(f, app, f.area()))
            .unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| {
                (0..w)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_preview_shows_target_mode_steps_ratings_and_the_operator_step() {
        let app = app_with_preview(CommandConfirmMode::Always);
        let s = screen(&app, 110, 40);
        for want in [
            "Replay plan — web-1/a.runbook.md",
            "Target: local",
            "Confirm: always",
            "Steps (4)",
            "$ df -h /",
            "in runbook: safety 98%",
            // A wide glyph takes two cells in the test buffer.
            "in runbook: safety 34% ⛔",
            "operator: needs <secret>",
            "placeholders: <user>",
            "(no command — done by hand)",
            "Exit criteria:",
            "[ Cancel ] Esc",
            "not available yet",
        ] {
            assert!(s.contains(want), "{want:?} missing:\n{s}");
        }
        assert!(!s.contains("without approval"), "only never warns");
    }

    #[test]
    fn never_is_called_out() {
        let app = app_with_preview(CommandConfirmMode::Never);
        let s = screen(&app, 110, 40);
        assert!(s.contains("Confirm: never"), "{s}");
        assert!(s.contains("commands will run without approval"), "{s}");
    }

    #[test]
    fn a_prod_target_is_marked() {
        let mut app = app_with_preview(CommandConfirmMode::Always);
        app.ssh_info = Some("root@10.0.0.5:22".into());
        app.ssh_targets = vec![filar_core::SshTarget {
            name: "db-1".into(),
            host: "10.0.0.5".into(),
            user: "root".into(),
            port: 22,
            auth: filar_core::SshAuth::Agent,
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: vec!["prod".into()],
        }];
        let s = screen(&app, 110, 40);
        assert!(s.contains("[prod]"), "{s}");
    }

    #[test]
    fn a_short_terminal_scrolls_and_keeps_header_and_footer() {
        let mut app = app_with_preview(CommandConfirmMode::Always);
        let s = screen(&app, 80, 16);
        assert!(
            s.contains("Confirm: always") && s.contains("[ Cancel ]"),
            "{s}"
        );
        assert!(s.contains("rows 1–"), "{s}");
        if let Some(p) = app.replay_preview.as_mut() {
            p.scroll = u16::MAX;
        }
        let s = screen(&app, 80, 16);
        assert!(
            s.contains("Use% below 90%"),
            "End reaches the last rows:\n{s}"
        );
        assert!(s.contains("Target: local"), "{s}");
    }

    #[test]
    fn a_start_notice_replaces_the_rows_line() {
        let mut app = app_with_preview(CommandConfirmMode::Always);
        if let Some(p) = app.replay_preview.as_mut() {
            p.notice = Some(crate::app::runbook_replay::START_UNAVAILABLE.into());
        }
        let s = screen(&app, 110, 40);
        assert!(s.contains("Start is not available yet"), "{s}");
    }
}
