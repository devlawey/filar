//! Runbook replay, entry point (#668): the runbooks source of the path
//! picker, the plan extraction it asks the runner for, and the plan preview.
//!
//! `/` (or Ctrl+Shift+F) opens the path picker on the target's files; `Tab`
//! switches it to the runbooks in the local export folder
//! ([`crate::runbook_picker`]). Enter on a runbook reads it and asks the
//! runner for its plan ([`filar_agent::extract_replay_plan`], #667) with the
//! tab's model; the preview shows that plan against the tab it would run on.
//! Starting the replay is #669 — until then the preview only shows.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use filar_agent::{ReplayPlan, TokenUsage};
use filar_core::ChatBlock;
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

/// Footer note when Start is pressed before replay exists.
pub const START_UNAVAILABLE: &str =
    "Start is not available yet: step-by-step replay comes with #669";

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
            KeyCode::Enter | KeyCode::Char('s' | 'ы') => {
                preview.notice = Some(START_UNAVAILABLE.into());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;
    use filar_agent::ReplayStep;
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
    fn the_preview_scrolls_closes_and_refuses_start() {
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
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            app.replay_preview.as_ref().unwrap().notice.as_deref(),
            Some(START_UNAVAILABLE)
        );
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
}
