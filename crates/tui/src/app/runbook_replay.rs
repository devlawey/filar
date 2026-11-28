//! Runbook replay, entry point (#668): the runbooks source of the path
//! picker, the plan extraction it asks the runner for, and the plan preview.
//!
//! `/` (or Ctrl+Shift+F) opens the path picker on the target's files; `Tab`
//! switches it to the runbooks in the local export folder
//! ([`crate::runbook_picker`]). Enter on a runbook reads it and asks the
//! runner for its plan ([`filar_agent::extract_replay_plan`], #667) with the
//! tab's model; the preview shows that plan against the tab it would run on.
//! `Start` in the preview begins the replay (#669): one step at a time, each
//! sent to the agent as its own turn, each command through the usual
//! confirmation gate, a pause after every step for the operator's decision.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use filar_agent::{ReplayPlan, StepVerdict, TokenUsage};
use filar_core::{ChatBlock, CommandConfirmMode};
use tokio_util::sync::CancellationToken;

use super::{App, SessionId};
use crate::runbook_picker::RunbookEntry;

/// What the path picker lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PickerSource {
    /// The active target's file system (#351).
    #[default]
    Files,
    /// Runbooks in the local export folder (#668).
    Runbooks,
}

/// The runbooks source of the path picker.
#[derive(Debug, Default)]
pub struct RunbookPickerState {
    /// Runbooks found, newest first.
    pub entries: Vec<RunbookEntry>,
    /// Selected row.
    pub index: usize,
    /// The folder listed, for the title.
    pub dir: String,
    /// Why the list or the last plan could not be made.
    pub error: Option<String>,
    /// More runbooks than the list shows.
    pub truncated: bool,
    /// The runbook whose plan is being extracted, while it is.
    pub extracting: Option<String>,
}

/// A plan for the runner to extract with the tab's model.
#[derive(Debug, Clone)]
pub struct ReplayPlanRequest {
    /// The tab the plan is for.
    pub session_id: SessionId,
    /// Which request this is; a result for an older one is not shown.
    pub request_id: u64,
    /// The runbook's name in the list.
    pub name: String,
    /// The runbook's text.
    pub text: String,
    /// LLM profile the extraction runs on and is billed to.
    pub profile: String,
}

/// What the runner reports back for a [`ReplayPlanRequest`].
#[derive(Debug)]
pub struct ReplayPlanResult {
    /// The tab the plan is for.
    pub session_id: SessionId,
    /// The request answered.
    pub request_id: u64,
    /// The runbook's name in the list.
    pub name: String,
    /// The plan or why there is none; `None` when the extraction was cancelled.
    pub outcome: Option<Result<ReplayPlan, String>>,
    /// Token usage of the call, billed either way.
    pub usage: Option<TokenUsage>,
    /// LLM profile the call ran on.
    pub profile: String,
}

/// The plan preview window.
#[derive(Debug, Clone)]
pub struct ReplayPreview {
    /// The runbook's name in the list.
    pub name: String,
    /// The plan shown.
    pub plan: ReplayPlan,
    /// First body row shown.
    pub scroll: u16,
    /// A one-line note for the footer (why Start did nothing).
    pub notice: Option<String>,
}

/// Rows PgUp/PgDn move in the preview.
const PREVIEW_PAGE: u16 = 10;

/// Where a step of a replay stands (#669).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    /// Not reached yet.
    Pending,
    /// Its turn is with the agent.
    Running,
    /// Done, the check came out healthy.
    Passed,
    /// Done, the check found a problem.
    Failed,
    /// Done, without a clear answer.
    Unclear,
    /// Not done, by the operator's choice.
    Skipped,
    /// Done by the operator by hand.
    Manual,
}

impl StepStatus {
    /// One-character mark for the checklist, the feed and the export.
    pub fn mark(self) -> &'static str {
        match self {
            StepStatus::Pending => "·",
            StepStatus::Running => "*",
            StepStatus::Passed => "✓",
            StepStatus::Failed => "✗",
            StepStatus::Unclear => "?",
            StepStatus::Skipped => "-",
            // Narrow glyphs only: a wide emoji breaks terminal columns. The
            // word next to it says "done by hand".
            StepStatus::Manual => "✓",
        }
    }

    /// Word for the feed and the export.
    pub fn word(self) -> &'static str {
        match self {
            StepStatus::Pending => "not reached",
            StepStatus::Running => "running",
            StepStatus::Passed => "passed",
            StepStatus::Failed => "failed",
            StepStatus::Unclear => "unclear",
            StepStatus::Skipped => "skipped",
            StepStatus::Manual => "done by hand",
        }
    }
}

/// What the replay is waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayPhase {
    /// The current step's turn is with the agent.
    Running,
    /// The step's turn ended: the operator decides what next.
    Paused,
    /// The step is the operator's (a `<secret>`, or no command): they do it.
    Operator,
}

/// A replay in progress on one tab (#669).
#[derive(Debug, Clone)]
pub struct ReplayRun {
    /// The runbook's name in the list.
    pub name: String,
    /// The plan being walked.
    pub plan: ReplayPlan,
    /// One status per step.
    pub statuses: Vec<StepStatus>,
    /// Index of the current step.
    pub current: usize,
    /// What the replay waits for.
    pub phase: ReplayPhase,
    /// The agent's verdict on the current step, once its turn ended.
    pub verdict: Option<(StepVerdict, String)>,
    /// The agent's reason per step, for the summary.
    pub notes: Vec<String>,
    /// The step window is shown (the operator can hide it to talk to the agent).
    pub window_open: bool,
    /// Where the replay started.
    pub target: String,
    /// The tab's confirmation mode when it started.
    pub mode: CommandConfirmMode,
    /// The host the replay runs on (`None`: local); a step is sent only
    /// while the tab is still on it (review).
    pub host: Option<String>,
}

/// The operator's answer in the step window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplayDecision {
    /// Take the agent's verdict and go on.
    Accept,
    /// Mark passed and go on.
    Passed,
    /// Mark failed and go on.
    Failed,
    /// Run the same step again.
    Retry,
    /// Skip the step.
    Skip,
    /// The operator did it by hand.
    Manual,
    /// End the replay here.
    Stop,
    /// Hide the window to talk to the agent; Enter on an empty line brings it back.
    Hide,
}

impl App {
    /// Whether the picker offers runbooks here: a local or SSH tab, not the
    /// fleet, a cluster or a pod tab (#668).
    pub fn runbooks_available(&self) -> bool {
        !self.in_fleet() && self.kube.is_none()
    }

    /// Where runbooks and exports live: `save_dir`, else the working
    /// directory — the one rule for Ctrl+S, the Explain transcript and the
    /// runbook list.
    pub fn export_base_dir(&self) -> std::path::PathBuf {
        self.save_dir.clone().unwrap_or_else(|| {
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
        })
    }

    /// `Tab` in the path picker: switch between the target's files and the
    /// runbooks. Nothing while a plan is being extracted.
    pub fn toggle_picker_source(&mut self) {
        if !self.runbooks_available() || self.runbook_picker.extracting.is_some() {
            return;
        }
        self.path_picker_source = match self.path_picker_source {
            PickerSource::Files => {
                self.load_runbook_list();
                PickerSource::Runbooks
            }
            PickerSource::Runbooks => PickerSource::Files,
        };
    }

    /// (Re)read the runbook list from the export folder.
    pub fn load_runbook_list(&mut self) {
        let dir = self.export_base_dir();
        let state = &mut self.runbook_picker;
        state.dir = dir.display().to_string();
        state.index = 0;
        state.error = None;
        match crate::runbook_picker::list_runbooks(&dir) {
            Ok((entries, truncated)) => {
                state.entries = entries;
                state.truncated = truncated;
            }
            Err(e) => {
                state.entries.clear();
                state.truncated = false;
                state.error = Some(e);
            }
        }
    }

    /// Keys of the picker while it lists runbooks.
    pub(super) fn handle_runbook_picker_key(&mut self, key: KeyEvent) {
        let extracting = self.runbook_picker.extracting.is_some();
        match key.code {
            KeyCode::Esc if extracting => self.cancel_replay_plan(),
            KeyCode::Esc => self.close_path_picker(),
            KeyCode::Tab => self.toggle_picker_source(),
            // `//` types a literal `/` here too (#670).
            KeyCode::Char('/')
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.close_path_picker();
                self.history_pos = None;
                self.insert_char('/');
            }
            _ if extracting => {}
            KeyCode::Up | KeyCode::Char('k') => {
                self.runbook_picker.index = self.runbook_picker.index.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if self.runbook_picker.index + 1 < self.runbook_picker.entries.len() {
                    self.runbook_picker.index += 1;
                }
            }
            KeyCode::Enter => self.request_replay_plan(),
            _ => {}
        }
    }

    /// Enter on a runbook: read it and queue its plan for the runner.
    fn request_replay_plan(&mut self) {
        let Some(entry) = self
            .runbook_picker
            .entries
            .get(self.runbook_picker.index)
            .cloned()
        else {
            return;
        };
        let text = match crate::runbook_picker::read_runbook(&entry.path) {
            Ok(text) => text,
            Err(e) => {
                self.runbook_picker.error = Some(format!("{}: {e}", entry.display));
                return;
            }
        };
        self.replay_plan_request_id = self.replay_plan_request_id.wrapping_add(1);
        let profile = self
            .llm_profile
            .clone()
            .unwrap_or_else(|| self.default_profile_name.clone());
        self.pending_replay_plan = Some(ReplayPlanRequest {
            session_id: self.sessions[self.active].id,
            request_id: self.replay_plan_request_id,
            name: entry.display.clone(),
            text,
            profile,
        });
        self.runbook_picker.error = None;
        self.runbook_picker.extracting = Some(entry.display);
    }

    /// The plan request for the runner, once.
    pub fn take_pending_replay_plan(&mut self) -> Option<ReplayPlanRequest> {
        self.pending_replay_plan.take()
    }

    /// Stop the extraction in flight, if any; its result will not be shown.
    pub fn cancel_replay_plan(&mut self) {
        if let Some(token) = self.replay_plan_cancel.take() {
            token.cancel();
        }
        self.pending_replay_plan = None;
        self.runbook_picker.extracting = None;
        // A result already on its way is for a request that no longer is.
        self.replay_plan_request_id = self.replay_plan_request_id.wrapping_add(1);
    }

    /// The runner started the extraction: keep its token so Esc can stop it.
    pub fn replay_plan_started(&mut self, token: CancellationToken) {
        self.replay_plan_cancel = Some(token);
    }

    /// The runner's answer for a plan request.
    ///
    /// The usage is charged to the tab whatever happened — the call was
    /// billed. A result for a request that was cancelled or superseded is
    /// otherwise dropped. A plan opens the preview if its tab is still the
    /// active one with the picker open; an error stays in the picker, or
    /// becomes a note in the tab's feed when the picker is gone.
    pub fn finish_replay_plan(&mut self, result: ReplayPlanResult) {
        if let (Some(idx), Some(usage)) = (self.find_session_idx(result.session_id), &result.usage)
        {
            self.record_summary_usage_for(idx, usage, result.profile.clone());
        }
        let Some(outcome) = result.outcome else {
            return;
        };
        if result.request_id != self.replay_plan_request_id {
            return;
        }
        self.replay_plan_cancel = None;
        self.runbook_picker.extracting = None;
        let here = self.sessions[self.active].id == result.session_id;
        let in_picker =
            here && self.path_picker_visible && self.path_picker_source == PickerSource::Runbooks;
        match outcome {
            Ok(plan) if here => {
                self.close_path_picker();
                self.replay_preview = Some(ReplayPreview {
                    name: result.name,
                    plan,
                    scroll: 0,
                    notice: None,
                });
            }
            Ok(_) => self.note_for(
                result.session_id,
                format!("The replay plan for {} is ready, but this tab is no longer active — open it again from that tab.", result.name),
            ),
            Err(e) if in_picker => {
                self.runbook_picker.error = Some(format!("{}: {e}", result.name));
            }
            Err(e) => self.note_for(
                result.session_id,
                format!("No replay plan for {}: {e}", result.name),
            ),
        }
    }

    fn note_for(&mut self, sid: SessionId, note: String) {
        if let Some(idx) = self.find_session_idx(sid) {
            self.sessions[idx].messages.push(ChatBlock::System(note));
            self.sessions[idx].message_rev = self.sessions[idx].message_rev.wrapping_add(1);
        }
    }

    /// Keys of the plan preview; it takes all of them while open.
    pub(super) fn handle_replay_preview_key(&mut self, key: KeyEvent) {
        let Some(preview) = self.replay_preview.as_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc | KeyCode::Char('q' | 'c' | 'й' | 'с') => {
                self.replay_preview = None;
            }
            KeyCode::Up | KeyCode::Char('k') => preview.scroll = preview.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => preview.scroll = preview.scroll.saturating_add(1),
            KeyCode::PageUp => preview.scroll = preview.scroll.saturating_sub(PREVIEW_PAGE),
            KeyCode::PageDown => preview.scroll = preview.scroll.saturating_add(PREVIEW_PAGE),
            KeyCode::Home => preview.scroll = 0,
            // The renderer clamps to the last page.
            KeyCode::End => preview.scroll = u16::MAX,
            KeyCode::Enter | KeyCode::Char('s' | 'ы') => self.start_replay(),
            _ => {}
        }
    }

    /// The replay of the active tab, if one is running.
    pub fn active_replay(&self) -> Option<&ReplayRun> {
        self.replay_runs.get(&self.sessions[self.active].id)
    }

    /// `Start` in the preview: begin the replay on the active tab (#669).
    fn start_replay(&mut self) {
        let refusal = if !self.runbooks_available() {
            Some("Runbook replay runs in local and SSH tabs only.")
        } else if self.agent_running {
            Some("The agent is busy — start the replay when its turn has ended.")
        } else if self.active_replay().is_some() {
            Some("A replay is already running in this tab.")
        } else if self
            .replay_preview
            .as_ref()
            .is_some_and(|p| p.plan.steps.is_empty())
        {
            // `parse_replay_plan` never returns one, but a panic here would
            // take the whole TUI down (review).
            Some("The plan has no steps.")
        } else {
            None
        };
        if let Some(note) = refusal {
            if let Some(p) = self.replay_preview.as_mut() {
                p.notice = Some(note.into());
            }
            return;
        }
        let Some(preview) = self.replay_preview.take() else {
            return;
        };
        let steps = preview.plan.steps.len();
        let target = self.status_target();
        let mode = self.confirm_mode;
        self.push_message(ChatBlock::System(format!(
            "Runbook replay: {} — {steps} steps on {target}, confirm: {}. Each step pauses for your decision; every command goes through the usual confirmation.",
            preview.name,
            crate::ui::confirm_mode_name(mode)
        )));
        let sid = self.sessions[self.active].id;
        self.replay_runs.insert(
            sid,
            ReplayRun {
                name: preview.name,
                statuses: vec![StepStatus::Pending; steps],
                notes: vec![String::new(); steps],
                plan: preview.plan,
                current: 0,
                phase: ReplayPhase::Paused,
                verdict: None,
                window_open: false,
                target,
                mode,
                host: self.ssh_info.clone(),
            },
        );
        self.begin_replay_step();
    }

    /// Hand the current step to the agent, or to the operator when it needs
    /// a `<secret>` or has no command — those never reach the agent.
    fn begin_replay_step(&mut self) {
        let sid = self.sessions[self.active].id;
        let Some(run) = self.replay_runs.get_mut(&sid) else {
            return;
        };
        let Some(step) = run.plan.steps.get(run.current).cloned() else {
            return;
        };
        let total = run.plan.steps.len();
        run.verdict = None;
        if step.needs_secret || step.command.is_none() {
            run.phase = ReplayPhase::Operator;
            run.window_open = true;
            let why = if step.needs_secret {
                "it needs a <secret>"
            } else {
                "it has no command"
            };
            self.push_message(ChatBlock::System(format!(
                "Step {}/{total} «{}» is yours: {why}, so the agent does not run it. Do it yourself, then mark it done by hand or skip it.",
                step.index, step.title
            )));
            return;
        }
        run.phase = ReplayPhase::Running;
        run.window_open = false;
        run.statuses[run.current] = StepStatus::Running;
        let message = filar_agent::replay_step_message(&run.plan, &step);
        self.push_message(ChatBlock::User(message.clone()));
        self.scroll = 0;
        self.begin_agent_request(message);
    }

    /// The agent's turn of the active tab ended (#669): if it was a replay
    /// step, read its verdict and pause for the operator.
    ///
    /// Called for a finished turn with its reply, and for an error or a
    /// cancellation with none — the step then has no verdict.
    pub(crate) fn replay_turn_ended(&mut self, reply: Option<&str>) {
        let sid = self.sessions[self.active].id;
        let Some(run) = self.replay_runs.get_mut(&sid) else {
            return;
        };
        if run.phase != ReplayPhase::Running {
            return;
        }
        let verdict = reply.and_then(filar_agent::parse_step_verdict);
        let Some(step) = run.plan.steps.get(run.current) else {
            return;
        };
        let note = match &verdict {
            Some((v, reason)) => format!(
                "Step {}/{} «{}»: the agent says {}{}{}",
                step.index,
                run.plan.steps.len(),
                step.title,
                verdict_word(*v),
                if reason.is_empty() { "" } else { " — " },
                reason
            ),
            None => format!(
                "Step {}/{} «{}»: no verdict from the agent.",
                step.index,
                run.plan.steps.len(),
                step.title
            ),
        };
        run.phase = ReplayPhase::Paused;
        run.window_open = true;
        run.verdict = verdict;
        self.push_message(ChatBlock::System(note));
    }

    /// Why the next step cannot go out on this tab now, if it cannot: the
    /// tab is connecting, or it is on another host than the replay began on.
    /// The bool says whether the replay has to stop.
    fn replay_target_problem(&self) -> Option<(bool, String)> {
        let run = self.active_replay()?;
        let sid = self.sessions[self.active].id;
        if run.host != self.ssh_info {
            let now = self.ssh_info.as_deref().unwrap_or("local");
            return Some((
                true,
                format!(
                    "The tab is on {now} now, not on {} where the replay began — the replay stops. Start it again from the picker to run it here.",
                    run.host.as_deref().unwrap_or("local")
                ),
            ));
        }
        if self.ctrl_o_pending_session_id == Some(sid) || self.pending_ssh.is_some() {
            return Some((
                false,
                "The tab is connecting — continue once it is connected.".into(),
            ));
        }
        None
    }

    /// The operator's decision in the step window.
    pub fn replay_decide(&mut self, decision: ReplayDecision) {
        let advances = !matches!(decision, ReplayDecision::Hide | ReplayDecision::Stop);
        if advances {
            if let Some((stop, note)) = self.replay_target_problem() {
                self.push_message(ChatBlock::System(note));
                if stop {
                    self.finish_replay(true);
                }
                return;
            }
        }
        let sid = self.sessions[self.active].id;
        let Some(run) = self.replay_runs.get_mut(&sid) else {
            return;
        };
        if run.phase == ReplayPhase::Running {
            return;
        }
        let operator = run.phase == ReplayPhase::Operator;
        let status = match decision {
            ReplayDecision::Hide => {
                run.window_open = false;
                return;
            }
            ReplayDecision::Stop => {
                self.finish_replay(true);
                return;
            }
            ReplayDecision::Retry if !operator => {
                self.begin_replay_step();
                return;
            }
            ReplayDecision::Retry => return,
            ReplayDecision::Accept if operator => StepStatus::Manual,
            ReplayDecision::Accept => match run.verdict.as_ref().map(|(v, _)| *v) {
                Some(StepVerdict::Passed) => StepStatus::Passed,
                Some(StepVerdict::Failed) => StepStatus::Failed,
                _ => StepStatus::Unclear,
            },
            ReplayDecision::Passed | ReplayDecision::Failed if operator => return,
            ReplayDecision::Passed => StepStatus::Passed,
            ReplayDecision::Failed => StepStatus::Failed,
            ReplayDecision::Skip => StepStatus::Skipped,
            ReplayDecision::Manual => StepStatus::Manual,
        };
        run.statuses[run.current] = status;
        run.notes[run.current] = run.verdict.take().map(|(_, r)| r).unwrap_or_default();
        run.current += 1;
        if run.current == run.plan.steps.len() {
            self.finish_replay(false);
        } else {
            self.begin_replay_step();
        }
    }

    /// End the replay of the active tab with a summary in the feed — the
    /// same block the Ctrl+S export carries.
    fn finish_replay(&mut self, stopped: bool) {
        let sid = self.sessions[self.active].id;
        let Some(run) = self.replay_runs.remove(&sid) else {
            return;
        };
        self.push_message(ChatBlock::System(replay_summary(&run, stopped)));
    }

    /// Keys of the step window (#669); it takes them while it is shown.
    pub(super) fn handle_replay_window_key(&mut self, key: KeyEvent) {
        let decision = match key.code {
            KeyCode::Enter => Some(ReplayDecision::Accept),
            KeyCode::Char('p' | 'з') => Some(ReplayDecision::Passed),
            KeyCode::Char('f' | 'а') => Some(ReplayDecision::Failed),
            KeyCode::Char('r' | 'к') => Some(ReplayDecision::Retry),
            KeyCode::Char('s' | 'ы') => Some(ReplayDecision::Skip),
            KeyCode::Char('m' | 'ь') => Some(ReplayDecision::Manual),
            KeyCode::Char('q' | 'й') => Some(ReplayDecision::Stop),
            KeyCode::Esc => Some(ReplayDecision::Hide),
            _ => None,
        };
        if let Some(d) = decision {
            self.replay_decide(d);
        }
    }

    /// Whether the step window takes the keys now.
    /// Only in Normal mode: in the terminal (Ctrl+T) or a dialog the keys
    /// belong elsewhere, so the window is not drawn either (review).
    pub(crate) fn replay_window_active(&self) -> bool {
        self.mode == crate::app::AppMode::Normal
            && self
                .active_replay()
                .is_some_and(|r| r.window_open && r.phase != ReplayPhase::Running)
    }

    /// Enter on an empty input while a replay waits: show the window again.
    pub(super) fn reopen_replay_window(&mut self) -> bool {
        let sid = self.sessions[self.active].id;
        match self.replay_runs.get_mut(&sid) {
            Some(run) if run.phase != ReplayPhase::Running && !run.window_open => {
                run.window_open = true;
                true
            }
            _ => false,
        }
    }

    /// The confirmation dialog's header during a replay step: which step,
    /// its original rating, and the runbook's command when the proposed one
    /// differs from it.
    pub fn replay_confirm_header(&self, proposed: &str) -> Option<(String, Option<String>)> {
        let run = self
            .active_replay()
            .filter(|r| r.phase == ReplayPhase::Running)?;
        let step = run.plan.steps.get(run.current)?;
        // The rating before the title: a long title cut to fit never hides it.
        let mut line = format!("Runbook step {}/{}", step.index, run.plan.steps.len());
        if let Some(rating) = &step.original_safety {
            line.push_str(&format!(" · in runbook: {rating}"));
        }
        line.push_str(&format!(": {}", step.title));
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let runbook = step
            .command
            .as_deref()
            .filter(|c| norm(c) != norm(proposed))
            .map(|c| format!("Runbook: {c}"));
        Some((line, runbook))
    }
}

/// `passed`, `failed` or `unclear`.
fn verdict_word(v: StepVerdict) -> &'static str {
    match v {
        StepVerdict::Passed => "passed",
        StepVerdict::Failed => "failed",
        StepVerdict::Unclear => "unclear",
    }
}

/// The summary block of a finished or stopped replay.
pub fn replay_summary(run: &ReplayRun, stopped: bool) -> String {
    let mut out = format!(
        "Runbook replay: {}\nTarget: {} · confirm: {}\n",
        run.name,
        run.target,
        crate::ui::confirm_mode_name(run.mode)
    );
    for (i, step) in run.plan.steps.iter().enumerate() {
        let status = run.statuses[i];
        out.push_str(&format!(
            "{} {}. {} — {}",
            status.mark(),
            step.index,
            step.title,
            status.word()
        ));
        if !run.notes[i].is_empty() {
            out.push_str(&format!(": {}", run.notes[i]));
        }
        out.push('\n');
    }
    if !run.plan.exit_criteria.is_empty() {
        out.push_str("Exit criteria:\n");
        for c in &run.plan.exit_criteria {
            out.push_str(&format!("  - {c}\n"));
        }
    }
    if stopped {
        out.push_str(&format!(
            "Stopped at step {} of {}.",
            run.current + 1,
            run.plan.steps.len()
        ));
    } else {
        out.push_str("All steps done.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use filar_agent::{ReplayStep, StepVerdict};
    use filar_core::CommandConfirmMode;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    /// A fresh export folder with two runbooks, removed on drop.
    struct Exports(std::path::PathBuf);

    impl Drop for Exports {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn exports(tag: &str) -> Exports {
        let dir = std::env::temp_dir().join(format!("filar_replay_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("web-1")).unwrap();
        std::fs::write(
            dir.join("web-1/a.runbook.md"),
            "# A\n## Symptom\nDisk full.\n",
        )
        .unwrap();
        std::fs::write(dir.join("web-1/a.md"), "an export").unwrap();
        Exports(dir)
    }

    fn app_with(dir: &Exports) -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.save_dir = Some(dir.0.clone());
        app.default_profile_name = "default".into();
        app.open_path_picker(crate::path_picker::PathPickerKind::File);
        app
    }

    fn plan() -> ReplayPlan {
        ReplayPlan {
            title: "Disk full".into(),
            symptom: Some("Writes fail".into()),
            preconditions: vec!["sudo".into()],
            steps: vec![ReplayStep {
                index: 1,
                title: "Check".into(),
                command: Some("df -h".into()),
                expect: "Use%".into(),
                original_safety: Some("safety 98%".into()),
                placeholders: vec![],
                needs_secret: false,
            }],
            exit_criteria: vec!["10% free".into()],
            next_actions: vec![],
        }
    }

    #[test]
    fn tab_switches_the_picker_to_runbooks_and_back() {
        let dir = exports("tab");
        let mut app = app_with(&dir);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.path_picker_source, PickerSource::Runbooks);
        assert_eq!(
            app.runbook_picker.entries.len(),
            1,
            "exports are not runbooks"
        );
        assert_eq!(app.runbook_picker.entries[0].display, "web-1/a.runbook.md");
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.path_picker_source, PickerSource::Files);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.path_picker_visible);
        app.open_path_picker(crate::path_picker::PathPickerKind::File);
        assert_eq!(
            app.path_picker_source,
            PickerSource::Files,
            "a reopened picker starts on files"
        );
    }

    #[test]
    fn without_save_dir_the_list_reads_the_working_directory() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.save_dir = None;
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(app.export_base_dir(), cwd);
    }

    #[test]
    fn the_fleet_and_cluster_tabs_offer_no_runbooks() {
        let dir = exports("fleet");
        let mut app = app_with(&dir);
        app.kube = Some(crate::app::KubeTabState {
            target: filar_core::KubeCluster::default(),
            namespace: None,
            connection_id: 1,
            pod: None,
        });
        assert!(!app.runbooks_available());
        app.toggle_picker_source();
        assert_eq!(app.path_picker_source, PickerSource::Files);
    }

    #[test]
    fn enter_queues_a_plan_with_the_tab_and_its_profile() {
        let dir = exports("enter");
        let mut app = app_with(&dir);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Enter));
        let req = app.take_pending_replay_plan().expect("queued");
        assert_eq!(req.session_id, app.sessions[app.active].id);
        assert_eq!(req.name, "web-1/a.runbook.md");
        assert!(req.text.contains("Disk full."));
        assert_eq!(req.profile, "default");
        assert_eq!(
            app.runbook_picker.extracting.as_deref(),
            Some("web-1/a.runbook.md")
        );
        // Moving and switching wait for the plan.
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.path_picker_source, PickerSource::Runbooks);
    }

    #[test]
    fn esc_while_extracting_cancels_and_keeps_the_list() {
        let dir = exports("cancel");
        let mut app = app_with(&dir);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Enter));
        let req = app.take_pending_replay_plan().unwrap();
        let token = CancellationToken::new();
        app.replay_plan_started(token.clone());
        app.handle_key(key(KeyCode::Esc));
        assert!(token.is_cancelled());
        assert!(
            app.path_picker_visible,
            "first Esc stops the extraction only"
        );
        assert!(app.runbook_picker.extracting.is_none());
        // A late plan for the cancelled request opens nothing.
        app.finish_replay_plan(ReplayPlanResult {
            session_id: req.session_id,
            request_id: req.request_id,
            name: req.name,
            outcome: Some(Ok(plan())),
            usage: None,
            profile: req.profile,
        });
        assert!(app.replay_preview.is_none());
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.path_picker_visible);
    }

    #[test]
    fn a_plan_opens_the_preview_and_an_error_stays_in_the_picker() {
        let dir = exports("finish");
        let mut app = app_with(&dir);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Enter));
        let req = app.take_pending_replay_plan().unwrap();
        app.finish_replay_plan(ReplayPlanResult {
            session_id: req.session_id,
            request_id: req.request_id,
            name: req.name.clone(),
            outcome: Some(Err("no steps found in the runbook".into())),
            usage: None,
            profile: req.profile.clone(),
        });
        assert!(app.path_picker_visible);
        let err = app.runbook_picker.error.clone().expect("error shown");
        assert!(err.contains("no steps"), "{err}");

        app.handle_key(key(KeyCode::Enter));
        let req = app.take_pending_replay_plan().unwrap();
        app.finish_replay_plan(ReplayPlanResult {
            session_id: req.session_id,
            request_id: req.request_id,
            name: req.name,
            outcome: Some(Ok(plan())),
            usage: None,
            profile: req.profile,
        });
        assert!(!app.path_picker_visible);
        let preview = app.replay_preview.as_ref().expect("preview open");
        assert_eq!(preview.plan.steps.len(), 1);
    }

    #[test]
    fn an_unreadable_runbook_is_an_error_in_the_picker() {
        let dir = exports("unreadable");
        let mut app = app_with(&dir);
        app.handle_key(key(KeyCode::Tab));
        std::fs::write(dir.0.join("web-1/a.runbook.md"), [0xff, 0xfe]).unwrap();
        app.handle_key(key(KeyCode::Enter));
        assert!(app.take_pending_replay_plan().is_none());
        assert!(app
            .runbook_picker
            .error
            .as_deref()
            .unwrap_or("")
            .contains("UTF-8"));
    }

    #[test]
    fn the_preview_scrolls_and_closes() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.replay_preview = Some(ReplayPreview {
            name: "x".into(),
            plan: plan(),
            scroll: 0,
            notice: None,
        });
        app.handle_key(key(KeyCode::PageDown));
        assert_eq!(app.replay_preview.as_ref().unwrap().scroll, PREVIEW_PAGE);
        app.handle_key(key(KeyCode::Up));
        assert_eq!(
            app.replay_preview.as_ref().unwrap().scroll,
            PREVIEW_PAGE - 1
        );
        app.handle_key(key(KeyCode::Home));
        assert_eq!(app.replay_preview.as_ref().unwrap().scroll, 0);
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.input.is_empty(), "the preview takes every key");
        app.handle_key(key(KeyCode::Esc));
        assert!(app.replay_preview.is_none());
    }

    #[test]
    fn a_slash_in_the_runbook_list_is_a_literal_slash() {
        let dir = exports("slash");
        let mut app = app_with(&dir);
        app.handle_key(key(KeyCode::Tab));
        app.handle_key(key(KeyCode::Char('/')));
        assert!(!app.path_picker_visible);
        assert_eq!(app.input, "/");
    }

    // ── Replay (#669) ──────────────────────────────────────────────────

    fn replay_plan() -> ReplayPlan {
        let step = |index: usize, command: &str, secret: bool| ReplayStep {
            index,
            title: format!("step {index}"),
            command: Some(command.into()),
            expect: "look".into(),
            original_safety: (index == 3).then(|| "safety 34% ⛔ danger".into()),
            placeholders: vec![],
            needs_secret: secret,
        };
        ReplayPlan {
            title: "Disk full".into(),
            symptom: None,
            preconditions: vec![],
            steps: vec![
                step(1, "df -h /", false),
                step(2, "docker login -p <secret>", true),
                step(3, "systemctl restart <service>", false),
            ],
            exit_criteria: vec!["10% free".into()],
            next_actions: vec![],
        }
    }

    fn started(mode: CommandConfirmMode) -> App {
        let mut app = App::new("test".into(), mode);
        app.confirm_mode = mode;
        app.replay_preview = Some(ReplayPreview {
            name: "web-1/a.runbook.md".into(),
            plan: replay_plan(),
            scroll: 0,
            notice: None,
        });
        app.handle_key(key(KeyCode::Enter));
        app
    }

    fn finish_turn(app: &mut App, reply: &str) {
        app.handle_agent_event(crate::event::TuiEvent::Agent {
            session_id: app.sessions[app.active].id,
            event: filar_agent::AgentEvent::Finished(reply.into()),
        });
    }

    fn run_of(app: &App) -> &ReplayRun {
        app.active_replay().expect("replay running")
    }

    #[test]
    fn start_sends_the_first_step_only_and_marks_it_running() {
        let app = started(CommandConfirmMode::Always);
        assert!(app.replay_preview.is_none());
        let run = run_of(&app);
        assert_eq!(run.phase, ReplayPhase::Running);
        assert_eq!(run.statuses[0], StepStatus::Running);
        let sent = app.pending_input.clone().expect("a turn for the agent");
        assert!(
            sent.contains("step 1 of 3") && sent.contains("Command: df -h /"),
            "{sent}"
        );
        assert!(!sent.contains("systemctl"), "later steps stay out: {sent}");
        assert!(app.agent_running);
    }

    #[test]
    fn a_finished_turn_pauses_with_the_verdict_and_enter_moves_on() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "41% used.\nSTEP RESULT: passed - plenty of space");
        let run = run_of(&app);
        assert_eq!(run.phase, ReplayPhase::Paused);
        assert!(run.window_open);
        assert_eq!(
            run.verdict,
            Some((StepVerdict::Passed, "plenty of space".into()))
        );
        assert!(app.replay_window_active());
        app.pending_input = None;
        app.handle_key(key(KeyCode::Enter));
        let run = run_of(&app);
        assert_eq!(run.statuses[0], StepStatus::Passed);
        // Step 2 needs a <secret>: the operator's, never sent to the agent.
        assert_eq!(run.current, 1);
        assert_eq!(run.phase, ReplayPhase::Operator);
        assert!(
            app.pending_input.is_none(),
            "a <secret> step never reaches the agent"
        );
    }

    #[test]
    fn the_replay_pauses_after_every_step_even_without_confirmation() {
        let mut app = started(CommandConfirmMode::Never);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        assert_eq!(run_of(&app).phase, ReplayPhase::Paused);
        assert_eq!(run_of(&app).current, 0, "never mode does not run ahead");
    }

    #[test]
    fn a_denied_or_cancelled_step_does_not_move_the_checklist() {
        let mut app = started(CommandConfirmMode::Always);
        // Ctrl+Z on the step: the agent reports Cancelled.
        app.handle_agent_event(crate::event::TuiEvent::Agent {
            session_id: app.sessions[app.active].id,
            event: filar_agent::AgentEvent::Cancelled { proposal_id: None },
        });
        app.mode = crate::app::AppMode::Normal;
        let run = run_of(&app);
        assert_eq!(run.current, 0);
        assert_eq!(run.phase, ReplayPhase::Paused);
        assert_eq!(run.verdict, None);
        // Retry sends the same step again.
        app.pending_input = None;
        app.handle_key(key(KeyCode::Char('r')));
        assert_eq!(run_of(&app).statuses[0], StepStatus::Running);
        assert!(app
            .pending_input
            .as_deref()
            .unwrap_or("")
            .contains("step 1 of 3"));
    }

    #[test]
    fn esc_hides_the_window_for_chat_and_an_empty_enter_brings_it_back() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: unclear - no output");
        app.handle_key(key(KeyCode::Esc));
        assert!(!app.replay_window_active());
        app.handle_key(key(KeyCode::Char('h')));
        assert_eq!(app.input, "h", "typing goes to the input while hidden");
        app.input.clear();
        app.cursor_pos = 0;
        app.handle_key(key(KeyCode::Enter));
        assert!(app.replay_window_active());
    }

    #[test]
    fn walking_to_the_end_writes_the_summary_the_export_carries() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - 41% used");
        app.handle_key(key(KeyCode::Enter)); // step 1 accepted → step 2 operator
        app.handle_key(key(KeyCode::Char('m'))); // done by hand → step 3 sent
        assert_eq!(run_of(&app).current, 2);
        finish_turn(&mut app, "STEP RESULT: failed - unit not found");
        app.handle_key(key(KeyCode::Enter));
        assert!(app.active_replay().is_none(), "the replay ended");
        let summary = app
            .messages
            .iter()
            .rev()
            .find_map(|b| match b {
                ChatBlock::System(t)
                    if t.starts_with("Runbook replay: ") && t.contains("All steps") =>
                {
                    Some(t.clone())
                }
                _ => None,
            })
            .expect("summary block");
        assert!(
            summary.contains("✓ 1. step 1 — passed: 41% used"),
            "{summary}"
        );
        assert!(summary.contains("✓ 2. step 2 — done by hand"), "{summary}");
        assert!(
            summary.contains("✗ 3. step 3 — failed: unit not found"),
            "{summary}"
        );
        assert!(summary.contains("10% free"), "{summary}");
        let md = crate::app::messages_to_markdown(&app.messages, "s", &None);
        assert!(md.contains("Runbook replay: web-1/a.runbook.md"), "{md}");
    }

    #[test]
    fn stop_ends_the_replay_where_it_is() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        app.handle_key(key(KeyCode::Char('q')));
        assert!(app.active_replay().is_none());
        let last = match app.messages.last() {
            Some(ChatBlock::System(t)) => t.clone(),
            other => panic!("{other:?}"),
        };
        assert!(last.contains("Stopped at step 1 of 3"), "{last}");
    }

    #[test]
    fn the_confirmation_header_names_the_step_and_a_differing_runbook_command() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Char('m')));
        // Step 3 is with the agent now.
        let (line, runbook) = app
            .replay_confirm_header("systemctl restart nginx")
            .expect("header during a step");
        assert!(
            line.starts_with("Runbook step 3/3 · in runbook: safety 34% ⛔ danger: step 3"),
            "{line}"
        );
        assert!(line.contains("in runbook: safety 34% ⛔ danger"), "{line}");
        assert_eq!(
            runbook.as_deref(),
            Some("Runbook: systemctl restart <service>")
        );
        let (_, same) = app
            .replay_confirm_header("systemctl  restart <service>")
            .unwrap();
        assert_eq!(
            same, None,
            "no second line when the command is the runbook's"
        );
    }

    #[test]
    fn a_second_start_or_a_busy_agent_is_refused_with_a_note() {
        let mut app = started(CommandConfirmMode::Always);
        app.replay_preview = Some(ReplayPreview {
            name: "x".into(),
            plan: replay_plan(),
            scroll: 0,
            notice: None,
        });
        app.handle_key(key(KeyCode::Enter));
        let note = app
            .replay_preview
            .as_ref()
            .and_then(|p| p.notice.clone())
            .unwrap_or_default();
        assert!(
            note.contains("agent is busy") || note.contains("already running"),
            "{note}"
        );
    }

    #[test]
    fn closing_the_tab_ends_its_replay() {
        let mut app = started(CommandConfirmMode::Always);
        let sid = app.sessions[app.active].id;
        app.new_tab();
        app.active = 0;
        app.close_tab();
        assert!(!app.replay_runs.contains_key(&sid));
    }

    #[test]
    fn an_empty_plan_is_refused_not_a_panic() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        let mut plan = replay_plan();
        plan.steps.clear();
        app.replay_preview = Some(ReplayPreview {
            name: "x".into(),
            plan,
            scroll: 0,
            notice: None,
        });
        app.handle_key(key(KeyCode::Enter));
        assert!(app.active_replay().is_none());
        assert_eq!(
            app.replay_preview
                .as_ref()
                .and_then(|p| p.notice.as_deref()),
            Some("The plan has no steps.")
        );
    }

    #[test]
    fn a_changed_host_stops_the_replay_instead_of_sending_the_next_step() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        app.active_session_mut().ssh_info = Some("root@10.0.0.9:22".into());
        app.pending_input = None;
        app.handle_key(key(KeyCode::Enter));
        assert!(app.active_replay().is_none(), "the replay stopped");
        assert!(app.pending_input.is_none(), "nothing went to the new host");
        let text: Vec<String> = app
            .messages
            .iter()
            .filter_map(|b| match b {
                ChatBlock::System(t) => Some(t.clone()),
                _ => None,
            })
            .collect();
        assert!(
            text.iter()
                .any(|t| t.contains("root@10.0.0.9:22") && t.contains("replay stops")),
            "{text:?}"
        );
    }

    #[test]
    fn a_connecting_tab_holds_the_next_step() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        app.pending_ssh = Some(("root".into(), "h".into(), 22));
        app.pending_input = None;
        app.handle_key(key(KeyCode::Enter));
        let run = run_of(&app);
        assert_eq!(run.current, 0, "the step was not decided");
        assert!(app.pending_input.is_none());
    }

    #[test]
    fn ctrl_s_in_the_step_window_is_not_skip() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        app.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL));
        let run = run_of(&app);
        assert_eq!(run.current, 0);
        assert_eq!(
            run.statuses[0],
            StepStatus::Running,
            "the step is still undecided"
        );
    }

    #[test]
    fn the_window_belongs_to_normal_mode_only() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        assert!(app.replay_window_active());
        app.mode = crate::app::AppMode::Interactive;
        assert!(!app.replay_window_active(), "the terminal gets its keys");
    }

    #[test]
    fn an_agent_that_cannot_start_pauses_the_step() {
        let mut app = started(CommandConfirmMode::Always);
        app.agent_start_failed("Profile 'x' not found".into());
        assert_eq!(app.mode, crate::app::AppMode::Normal);
        assert!(!app.agent_running);
        let run = run_of(&app);
        assert_eq!(run.phase, ReplayPhase::Paused);
        assert!(app.replay_window_active(), "retry and stop are reachable");
    }

    #[test]
    fn an_error_turn_also_pauses_the_step() {
        let mut app = started(CommandConfirmMode::Always);
        app.handle_agent_event(crate::event::TuiEvent::Agent {
            session_id: app.sessions[app.active].id,
            event: filar_agent::AgentEvent::Error("network".into()),
        });
        assert_eq!(run_of(&app).phase, ReplayPhase::Paused);
    }

    #[test]
    fn restoring_a_session_into_the_tab_ends_its_replay() {
        let mut app = started(CommandConfirmMode::Always);
        finish_turn(&mut app, "STEP RESULT: passed - ok");
        let saved = crate::runner::session_snapshot(&app, "test", "1", "t");
        app.apply_loaded_session(saved);
        assert!(
            app.active_replay().is_none(),
            "the old checklist does not follow F3"
        );
    }
}
