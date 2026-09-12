//! Session-save progress overlay — modal window shown during Ctrl+S export.
//!
//! Renders a centred modal with a progress bar (`Gauge`) for the raw export,
//! a second bar for the runbook generation the export arms (#409), status
//! text, and a footer hint.  Follows the same overlay convention as
//! `host_select`.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph};
use ratatui::Frame;

use crate::app::{App, RunbookState};

use super::theme::Glyphs;

const OVERLAY_WIDTH: u16 = 50;
/// Height of the export-only layout — unchanged since before #409.
const OVERLAY_HEIGHT: u16 = 8;
/// Height once the runbook takes part: a caption and a bar join the layout.
const OVERLAY_HEIGHT_RUNBOOK: u16 = 11;
const H_MARGIN: u16 = 6;
const V_MARGIN: u16 = 4;

/// What the runbook bar of the save overlay shows (#409).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RunbookBar {
    /// Armed by Ctrl+S; the `.md` is still being written, so the runbook has
    /// not started — and never will if the export fails.
    Waiting,
    /// The LLM call is in flight — indeterminate, animated.
    Generating,
    /// `{stem}.runbook.md` is on disk.
    Saved,
    /// No executed commands to fold — no call was made.
    Skipped,
    /// Ctrl+Z stopped the task.
    Cancelled,
    /// The call or the write failed; the export is unharmed.
    Failed,
}

impl RunbookBar {
    /// Caption following the `Runbook:` label.
    fn caption(self) -> &'static str {
        match self {
            Self::Waiting => "waiting for export",
            Self::Generating => "generating...",
            Self::Saved => "saved",
            Self::Skipped => "skipped (no commands)",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }
}

/// The runbook bar to show for the app's current state, if any (#409).
///
/// `None` means the export has no runbook story at all — the feature is off,
/// no job was armed, and no runbook is in flight — and the overlay renders
/// exactly as it did before #409. A runbook still `Generating` outranks an
/// armed job: the one-at-a-time guard means a second Ctrl+S exports without
/// a fresh job, and the bar keeps showing the runbook already running.
fn runbook_bar(app: &App) -> Option<RunbookBar> {
    if let Some(state) = app.runbook_state {
        return Some(match state {
            RunbookState::Generating => RunbookBar::Generating,
            RunbookState::Saved => RunbookBar::Saved,
            RunbookState::Skipped => RunbookBar::Skipped,
            RunbookState::Cancelled => RunbookBar::Cancelled,
            RunbookState::Failed => RunbookBar::Failed,
        });
    }
    app.runbook_armed.is_some().then_some(RunbookBar::Waiting)
}

/// Render the session-save progress overlay.
pub(crate) fn render_save_overlay(f: &mut Frame, app: &App, area: Rect) {
    let bar = runbook_bar(app);
    let base_height = if bar.is_some() {
        OVERLAY_HEIGHT_RUNBOOK
    } else {
        OVERLAY_HEIGHT
    };
    let width = OVERLAY_WIDTH.min(area.width.saturating_sub(2 * H_MARGIN));
    let height = base_height.min(area.height.saturating_sub(2 * V_MARGIN));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;

    let overlay_area = Rect::new(x, y, width, height);

    f.render_widget(Clear, overlay_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(app.theme.accent))
        .style(Style::default().bg(app.theme.bg))
        .title(Span::styled(
            " Saving Session ",
            Style::default()
                .fg(app.theme.accent)
                .add_modifier(Modifier::BOLD),
        ));

    // Inner area for content (inside the border).
    let inner = block.inner(overlay_area);

    f.render_widget(block, overlay_area);

    // Row 0: the export's percentage — and its caption once a second bar
    // needs distinguishing (#409).
    let progress = app.save_progress as u16;
    let mut pct_spans = vec![Span::raw("  ")];
    if bar.is_some() {
        pct_spans.push(Span::styled("Export ", app.theme.muted()));
    }
    pct_spans.push(Span::styled(
        format!("{:>3}%", progress),
        Style::default().fg(app.theme.fg),
    ));
    if let Some(row) = row_area(inner, 0) {
        f.render_widget(Paragraph::new(Line::from(pct_spans)), row);
    }

    // Row 1: progress bar of the raw export — its semantics are unchanged.
    let gauge_area = Rect::new(inner.x + 1, inner.y + 1, inner.width.saturating_sub(2), 1);
    if let Some(gauge_area) = clip(gauge_area, inner) {
        let gauge = Gauge::default()
            .block(Block::default())
            .gauge_style(app.theme.success_fg())
            .style(Style::default().fg(app.theme.fg_dim))
            .ratio((f64::from(progress) / 100.0).clamp(0.0, 1.0));
        f.render_widget(gauge, gauge_area);
    }

    // Rows 3-4, only when the export has a runbook story: the runbook's
    // caption and bar. The caption carries the state that the export's own
    // status line used to name, so nothing is announced twice (#409).
    let mut status_dy = 3;
    if let Some(bar) = bar {
        let caption_style = match bar {
            RunbookBar::Generating => app.theme.warning_fg(),
            RunbookBar::Saved => app.theme.success_fg(),
            RunbookBar::Failed => app.theme.danger_fg(),
            RunbookBar::Waiting | RunbookBar::Skipped | RunbookBar::Cancelled => app.theme.muted(),
        };
        if let Some(row) = row_area(inner, 3) {
            let line = Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("Runbook: {}", bar.caption()), caption_style),
            ]);
            f.render_widget(Paragraph::new(line), row);
        }
        let bar_area = Rect::new(inner.x + 1, inner.y + 4, inner.width.saturating_sub(2), 1);
        if let Some(bar_area) = clip(bar_area, inner) {
            draw_runbook_bar(f, bar_area, bar, app);
        }
        status_dy = 6;
    }

    // Status row: the export itself. At 100% the `.md` is on disk; the
    // runbook's own state lives on its bar's caption, and its failure never
    // breaks the export (#401, #409).
    let status = if let Some(ref err) = app.save_error {
        format!("Error: {err}")
    } else if progress < 100 {
        "Saving...".to_string()
    } else {
        "Done!".to_string()
    };
    if let Some(row) = row_area(inner, status_dy) {
        f.render_widget(
            Paragraph::new(Span::styled(status, app.theme.muted())),
            row,
        );
    }

    // Last row inside border: footer hint.
    if let Some(row) = row_area(inner, inner.height.saturating_sub(1)) {
        f.render_widget(
            Paragraph::new(Span::styled(" Esc to close ", app.theme.muted())),
            row,
        );
    }
}

/// A one-row content area at `dy` inside `inner`, clipped so a terminal too
/// small for the overlay cannot take the row — or the render — out of bounds.
fn row_area(inner: Rect, dy: u16) -> Option<Rect> {
    clip(Rect::new(inner.x, inner.y + dy, inner.width, 1), inner)
}

/// Intersection of `area` with `bounds`, if it still has any cells.
fn clip(area: Rect, bounds: Rect) -> Option<Rect> {
    let area = area.intersection(bounds);
    (area.width > 0 && area.height > 0).then_some(area)
}

/// Runbook-bar cells holding the sliding window of the indeterminate state
/// (#409): a quarter-wide segment that sweeps across the track and wraps
/// around, so the bar reads as "working" without a fake percentage. Halving
/// the tick paces the sweep calmer than 60 fps.
fn indeterminate_window(width: usize, tick: u64) -> std::ops::Range<usize> {
    let window = (width / 4).max(1);
    let cycle = width + window;
    let pos = ((tick / 2) % cycle as u64) as usize;
    pos.saturating_sub(window)..pos.min(width)
}

/// Draw the runbook bar: the sliding window (generating), the whole track
/// (saved), or just the track, in the theme's bar glyphs (#409).
fn draw_runbook_bar(f: &mut Frame, area: Rect, bar: RunbookBar, app: &App) {
    let width = area.width as usize;
    let filled = match bar {
        RunbookBar::Generating => indeterminate_window(width, app.tick),
        RunbookBar::Saved => 0..width,
        RunbookBar::Waiting | RunbookBar::Skipped | RunbookBar::Cancelled | RunbookBar::Failed => {
            0..0
        }
    };
    let (filled_style, track_style) = match bar {
        RunbookBar::Generating => (app.theme.warning_fg(), app.theme.dim()),
        RunbookBar::Saved => (app.theme.success_fg(), app.theme.dim()),
        // A failure tints the whole bar red; the others stay calm grey.
        RunbookBar::Failed => (app.theme.danger_fg(), app.theme.danger_fg()),
        RunbookBar::Waiting | RunbookBar::Skipped | RunbookBar::Cancelled => {
            (app.theme.dim(), app.theme.dim())
        }
    };
    let glyphs = Glyphs::detect();
    let buf = f.buffer_mut();
    for i in 0..width {
        let (symbol, style) = if filled.contains(&i) {
            (glyphs.bar_full, filled_style)
        } else {
            (glyphs.bar_empty, track_style)
        };
        buf.set_string(area.x + i as u16, area.y, symbol, style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::{App, RunbookArmed, RunbookState, SessionId};
    use filar_core::CommandConfirmMode;
    use ratatui::backend::TestBackend;
    use ratatui::Terminal;

    /// Render the save overlay into a test buffer and return all visible text.
    fn render_save_text(app: &App) -> String {
        render_save_text_sized(app, 80, 24)
    }

    /// `render_save_text` with an explicit terminal size (small-terminal tests).
    fn render_save_text_sized(app: &App, width: u16, height: u16) -> String {
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|f| {
                let area = f.area();
                render_save_overlay(f, app, area);
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut text = String::new();
        for y in 0..height {
            for x in 0..width {
                text.push(buffer[(x, y)].symbol().chars().next().unwrap_or(' '));
            }
        }
        text
    }

    /// An app with the save overlay up at the given progress.
    fn app_with_overlay(progress: u8) -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.save_overlay_visible = true;
        app.save_progress = progress;
        app
    }

    /// Arm a runbook job; its fields carry no meaning for rendering.
    fn arm_runbook(app: &mut App) {
        app.runbook_armed = Some(RunbookArmed {
            session_id: SessionId(0),
            target_dir: std::path::PathBuf::from("/tmp"),
            messages: Vec::new(),
            session_name: "test".into(),
            ssh_info: None,
            profile: "p".into(),
        });
    }

    #[test]
    fn overlay_shows_saving_when_progress_zero() {
        let app = app_with_overlay(0);
        let text = render_save_text(&app);
        assert!(text.contains("Saving..."), "must show Saving... when progress=0");
    }

    #[test]
    fn overlay_does_not_panic_on_progress_overflow() {
        let app = app_with_overlay(200);
        let text = render_save_text(&app);
        assert!(!text.trim().is_empty(), "overlay must render without panic");
    }

    #[test]
    fn overlay_renders_done_when_complete_no_error() {
        let app = app_with_overlay(100);
        let text = render_save_text(&app);
        assert!(text.contains("Done!"), "must show Done! when complete");
    }

    #[test]
    fn overlay_renders_error_when_save_error_set() {
        let mut app = app_with_overlay(0);
        app.save_error = Some("disk full".into());
        let text = render_save_text(&app);
        assert!(text.contains("Error: disk full"), "must show error message");
    }

    #[test]
    fn overlay_without_a_runbook_shows_no_second_bar() {
        // Feature off (or nothing armed): the pre-#409 layout, one bar.
        let app = app_with_overlay(100);
        let text = render_save_text(&app);
        assert!(text.contains("Done!"));
        assert!(!text.contains("Runbook"), "no runbook story, no runbook bar");
        assert!(
            !text.contains("Export"),
            "the export caption only exists beside a runbook bar"
        );
    }

    #[test]
    fn overlay_save_error_without_a_runbook_shows_no_second_bar() {
        // A failed export drops the armed job (#401): nothing will generate.
        let mut app = app_with_overlay(0);
        app.save_error = Some("disk full".into());
        let text = render_save_text(&app);
        assert!(text.contains("Error: disk full"));
        assert!(!text.contains("Runbook"), "a failed export must not promise a runbook");
    }

    #[test]
    fn overlay_shows_runbook_waiting_while_the_export_is_written() {
        let mut app = app_with_overlay(50);
        arm_runbook(&mut app);
        let text = render_save_text(&app);
        assert!(text.contains("Saving..."), "the export's own status stays");
        assert!(
            text.contains("Runbook: waiting for export"),
            "armed but unsaved → waiting, got: {text}"
        );
    }

    #[test]
    fn overlay_shows_runbook_generating_when_in_flight() {
        let mut app = app_with_overlay(100);
        app.runbook_state = Some(RunbookState::Generating);
        let text = render_save_text(&app);
        assert!(
            text.contains("Runbook: generating"),
            "must show the in-flight runbook on its own bar, got: {text}"
        );
        assert!(text.contains("Done!"), "the export itself is done");
        assert!(
            !text.contains("Generating runbook"),
            "the old status-line wording must not linger next to the bar"
        );
    }

    #[test]
    fn overlay_shows_runbook_skipped_when_no_commands() {
        let mut app = app_with_overlay(100);
        app.runbook_state = Some(RunbookState::Skipped);
        let text = render_save_text(&app);
        assert!(
            text.contains("Runbook: skipped (no commands)"),
            "must explain why no runbook was written"
        );
    }

    #[test]
    fn overlay_shows_runbook_cancelled_and_failed() {
        let mut app = app_with_overlay(100);
        app.runbook_state = Some(RunbookState::Cancelled);
        assert!(render_save_text(&app).contains("Runbook: cancelled"));
        app.runbook_state = Some(RunbookState::Failed);
        assert!(render_save_text(&app).contains("Runbook: failed"));
    }

    #[test]
    fn overlay_renders_done_when_runbook_saved() {
        let mut app = app_with_overlay(100);
        app.runbook_state = Some(RunbookState::Saved);
        let text = render_save_text(&app);
        assert!(text.contains("Done!"), "a saved runbook is still a done save");
        assert!(text.contains("Runbook: saved"));
    }

    #[test]
    fn overlay_runbook_bar_animates_with_the_tick() {
        let mut app = app_with_overlay(100);
        app.runbook_state = Some(RunbookState::Generating);
        app.tick = 10;
        let first = render_save_text(&app);
        app.tick = 100;
        let second = render_save_text(&app);
        assert_ne!(first, second, "the indeterminate window must move with the tick");
    }

    #[test]
    fn overlay_does_not_panic_on_a_tiny_terminal() {
        // 30×12 clips the two-bar layout; 20×6 degenerates the overlay itself.
        let mut app = app_with_overlay(100);
        app.runbook_state = Some(RunbookState::Generating);
        let _ = render_save_text_sized(&app, 30, 12);
        let _ = render_save_text_sized(&app, 20, 6);
        let mut app = app_with_overlay(50);
        arm_runbook(&mut app);
        let _ = render_save_text_sized(&app, 30, 12);
    }
}
