//! The step window of a runbook replay (#669): the checklist, the current
//! step and the agent's verdict, and the keys that decide what comes next.
//!
//! It shows after every step — whatever the confirmation mode — and while a
//! step is the operator's. `Esc` hides it to talk to the agent; Enter on an
//! empty input brings it back.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::Frame;

use crate::app::runbook_replay::{ReplayPhase, ReplayRun, StepStatus};
use crate::app::App;

const MAX_WIDTH: u16 = 90;

/// Draw the window over the frame.
pub(crate) fn render_replay_window(f: &mut Frame, app: &App, area: Rect) {
    let Some(run) = app.active_replay() else {
        return;
    };
    let width = MAX_WIDTH.min(area.width.saturating_sub(4)).max(24);
    // Inside the frame, with a row to spare above and below it.
    let room = area.height.saturating_sub(4).max(3) as usize;
    let lines = window_lines(app, run, room);
    let inner_w = width.saturating_sub(2) as usize;
    let rows: usize = lines
        .iter()
        .map(|l| l.width().max(1).div_ceil(inner_w.max(1)))
        .sum();
    let height = (rows as u16 + 2).min(area.height.saturating_sub(2)).max(5);
    let x = area.x + area.width.saturating_sub(width) / 2;
    let y = area.y + area.height.saturating_sub(height) / 2;
    let frame = Rect::new(x, y, width, height);
    f.render_widget(Clear, frame);
    let title = format!(
        " Runbook replay — step {}/{} ",
        (run.current + 1).min(run.plan.steps.len()),
        run.plan.steps.len()
    );
    f.render_widget(
        Paragraph::new(lines).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(app.theme.accent))
                .style(Style::default().bg(app.theme.bg))
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(app.theme.accent)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        frame,
    );
}

/// The checklist, the current step's state and the keys, in at most `room`
/// rows: the current step and the keys always fit, and a checklist longer
/// than the rest of the room shows the steps around the current one (review).
pub(crate) fn window_lines(app: &App, run: &ReplayRun, room: usize) -> Vec<Line<'static>> {
    let muted = app.theme.muted();
    let bold = Style::default()
        .fg(app.theme.fg)
        .add_modifier(Modifier::BOLD);
    let mut tail = tail_lines(app, run);
    let mut out = vec![Line::from(vec![
        Span::styled(run.name.clone(), bold),
        Span::styled(format!("  · {}", run.target), muted),
    ])];
    let total = run.plan.steps.len();
    let avail = room.saturating_sub(1 + tail.len()).max(1);
    let (from, to) = if total <= avail {
        (0, total)
    } else {
        // Two rows go to the "… earlier" / "… more" marks.
        let shown = avail.saturating_sub(2).max(1);
        let from = run.current.saturating_sub(shown / 2).min(total - shown);
        (from, from + shown)
    };
    if from > 0 {
        out.push(Line::from(Span::styled(
            format!("  … {from} earlier"),
            muted,
        )));
    }
    for (i, step) in run.plan.steps.iter().enumerate().take(to).skip(from) {
        let status = run.statuses.get(i).copied().unwrap_or(StepStatus::Pending);
        let style = match status {
            StepStatus::Passed | StepStatus::Manual => app.theme.success_fg(),
            StepStatus::Failed => app.theme.danger_fg(),
            StepStatus::Unclear => app.theme.warning_fg(),
            _ if i == run.current => bold,
            _ => muted,
        };
        let cursor = if i == run.current { ">" } else { " " };
        out.push(Line::from(Span::styled(
            format!("{cursor} {} {}. {}", status.mark(), step.index, step.title),
            style,
        )));
    }
    if to < total {
        out.push(Line::from(Span::styled(
            format!("  … {} more", total - to),
            muted,
        )));
    }
    out.append(&mut tail);
    out
}

/// The current step's state and the keys.
fn tail_lines(app: &App, run: &ReplayRun) -> Vec<Line<'static>> {
    let muted = app.theme.muted();
    let bold = Style::default()
        .fg(app.theme.fg)
        .add_modifier(Modifier::BOLD);
    let mut out = vec![Line::raw("")];
    let Some(step) = run
        .plan
        .steps
        .get(run.current.min(run.plan.steps.len().saturating_sub(1)))
    else {
        return out;
    };
    match run.phase {
        ReplayPhase::Operator => {
            out.push(Line::from(Span::styled(
                if step.needs_secret {
                    "This step needs a <secret>: do it yourself — the agent never fills it in."
                } else {
                    "This step has no command: do it yourself."
                },
                app.theme.warning_fg(),
            )));
            if let Some(cmd) = &step.command {
                out.push(Line::from(Span::styled(
                    format!("  $ {cmd}"),
                    app.theme.command_style(),
                )));
            }
            out.push(Line::from(Span::styled(
                "Enter/m done by hand · s skip · q stop · Esc hide",
                muted,
            )));
        }
        _ => {
            let verdict = match &run.verdict {
                Some((v, reason)) => {
                    let word = match v {
                        filar_agent::StepVerdict::Passed => "passed",
                        filar_agent::StepVerdict::Failed => "failed",
                        filar_agent::StepVerdict::Unclear => "unclear",
                    };
                    if reason.is_empty() {
                        format!("Agent: {word}")
                    } else {
                        format!("Agent: {word} — {reason}")
                    }
                }
                None => "Agent: no verdict".to_string(),
            };
            out.push(Line::from(Span::styled(verdict, bold)));
            if !step.expect.is_empty() {
                out.push(Line::from(Span::styled(
                    format!("Look at: {}", step.expect),
                    muted,
                )));
            }
            out.push(Line::from(Span::styled(
                "Enter accept · p passed · f failed · r retry · s skip · m done by hand · q stop · Esc talk",
                muted,
            )));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::runbook_replay::ReplayRun;
    use filar_agent::{ReplayPlan, ReplayStep, StepVerdict};
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    fn run(phase: ReplayPhase) -> ReplayRun {
        let step = |index: usize, secret: bool| ReplayStep {
            index,
            title: format!("step {index}"),
            command: Some(if secret {
                "docker login -p <secret>".into()
            } else {
                "df -h".into()
            }),
            expect: "Use%".into(),
            original_safety: None,
            placeholders: vec![],
            needs_secret: secret,
        };
        ReplayRun {
            name: "web-1/a.runbook.md".into(),
            plan: ReplayPlan {
                title: "t".into(),
                symptom: None,
                preconditions: vec![],
                steps: vec![step(1, false), step(2, true)],
                exit_criteria: vec![],
                next_actions: vec![],
            },
            statuses: vec![StepStatus::Passed, StepStatus::Pending],
            current: if phase == ReplayPhase::Operator { 1 } else { 0 },
            phase,
            verdict: Some((StepVerdict::Passed, "41% used".into())),
            notes: vec![String::new(); 2],
            window_open: true,
            target: "local".into(),
            mode: CommandConfirmMode::Always,
            host: None,
        }
    }

    fn screen(app: &App) -> String {
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| render_replay_window(f, app, f.area())).unwrap();
        let buf = t.backend().buffer().clone();
        (0..30)
            .map(|y| {
                (0..100)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn app_with(r: ReplayRun) -> App {
        let mut app = App::new("local".into(), CommandConfirmMode::Always);
        let sid = app.sessions[app.active].id;
        app.replay_runs.insert(sid, r);
        app
    }

    #[test]
    fn a_paused_step_shows_the_checklist_the_verdict_and_the_keys() {
        let app = app_with(run(ReplayPhase::Paused));
        let s = screen(&app);
        for want in [
            "Runbook replay — step 1/2",
            "✓ 1. step 1",
            "· 2. step 2",
            "Agent: passed — 41% used",
            "Enter accept",
            "r retry",
        ] {
            assert!(s.contains(want), "{want:?}:\n{s}");
        }
    }

    #[test]
    fn an_operator_step_says_it_is_theirs() {
        let app = app_with(run(ReplayPhase::Operator));
        let s = screen(&app);
        assert!(s.contains("needs a <secret>"), "{s}");
        assert!(s.contains("Enter/m done by hand"), "{s}");
        assert!(!s.contains("r retry"), "{s}");
    }

    #[test]
    fn a_long_checklist_keeps_the_current_step_and_the_keys_on_a_short_screen() {
        let mut r = run(ReplayPhase::Operator);
        let template = r.plan.steps[0].clone();
        r.plan.steps = (1..=30)
            .map(|i| ReplayStep {
                index: i,
                title: format!("step {i}"),
                needs_secret: i == 17,
                ..template.clone()
            })
            .collect();
        r.statuses = vec![StepStatus::Pending; 30];
        r.notes = vec![String::new(); 30];
        r.current = 16;
        let app = app_with(r);
        let mut t = Terminal::new(TestBackend::new(80, 24)).unwrap();
        t.draw(|f| render_replay_window(f, &app, f.area())).unwrap();
        let buf = t.backend().buffer().clone();
        let s = (0..24)
            .map(|y| {
                (0..80)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(s.contains("> · 17. step 17"), "{s}");
        assert!(s.contains("earlier") && s.contains("more"), "{s}");
        assert!(s.contains("Enter/m done by hand"), "{s}");
        assert!(
            s.contains("$ df -h"),
            "the operator's command stays readable: {s}"
        );
    }
}
