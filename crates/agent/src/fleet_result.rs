//! Where each host stands once an operation is over (#428).
//!
//! The fan-out (#427) reports three shapes of run: the host answered, its
//! deadline expired, or the executor failed. That is what *happened on the
//! wire*; this module turns it into what a person reads — and the reading
//! has seven states, not three.
//!
//! # An unreachable host is a cell in the table, not a failed operation
//!
//! Решение по развилке 30. In a real estate of machines somebody is always
//! rebooting. An operation that fails because one host out of twelve is
//! down would refuse more often than it works, so a host that never
//! answered is a *state*: the other eleven still produce a summary.
//!
//! The operation itself has failed only when **nobody answered** while at
//! least one host was asked — see [`OperationSummary::is_failed`]. Nothing
//! came back, so there is nothing to compare and nothing to report.
//!
//! # The count of silent hosts is not optional
//!
//! [`OperationSummary::headline`] always names how many hosts did not
//! answer, including when that number is zero. A summary that says "all
//! the same everywhere" while three machines were unreachable is a lie
//! about those three, and the shape of the summary must not allow it.
//!
//! # Eight states, and what separates them
//!
//! | State | The host… |
//! |---|---|
//! | [`Success`][HostState::Success] | answered, command exited 0 |
//! | [`Divergent`][HostState::Divergent] | answered, and its value differs from the rest (#430 decides this) |
//! | [`ExecutionError`][HostState::ExecutionError] | answered with a non-zero exit, or the executor failed for a reason other than contact |
//! | [`TimedOut`][HostState::TimedOut] | was asked and its own deadline expired first |
//! | [`NoContact`][HostState::NoContact] | could not be reached at all |
//! | [`NotApplicable`][HostState::NotApplicable] | was not asked: no command variant covers its OS family (#425) |
//! | [`Skipped`][HostState::Skipped] | was not asked, for any other reason — no credentials being the usual one |
//! | [`Cancelled`][HostState::Cancelled] | the user cancelled the operation before it answered (#437) |
//!
//! They partition into four groups that the summary rules are written in
//! terms of: **answered** (the first three — the host said something),
//! **unanswered** (asked, said nothing), **not asked**, and **cancelled**.
//! Cancelled is its own group on purpose: a host the user stopped did not
//! go silent — counting it as unanswered would call an operation nobody
//! let finish "failed", and would make the retry (#429) ask again what the
//! user just stopped. "Unanswered" is
//! deliberately the narrow set: a command that ran and exited 1 told us
//! something real, and hiding that under the same word as a dead machine
//! would lose the difference that matters when reading a fleet.
//!
//! # No raw output lives here
//!
//! A row carries a state and a host name, never the bytes a host printed.
//! Folding values into a comparison — and deciding who is
//! [`Divergent`][HostState::Divergent] — is #430, whose whole point is
//! that only the folded table ever reaches the model.

use std::collections::BTreeMap;
use std::fmt;

use filar_core::error::{CoreError, Result};
use filar_core::fleet_op::{FleetOperation, HostHandle, OperationId};
use filar_transport::is_connection_lost;

use crate::fleet_run::{FleetRunReport, HostRun};

// ---------------------------------------------------------------------------
// HostState
// ---------------------------------------------------------------------------

/// Where one host ended up in an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostState {
    /// Answered, and the command exited cleanly.
    Success,
    /// Answered, and its value is not the one the others gave.
    ///
    /// Only the aggregation (#430) can know this: one host's answer says
    /// nothing about agreement. [`from_run`][Self::from_run] therefore
    /// never returns it — it arrives through
    /// [`OperationResult::mark_divergent`].
    Divergent,
    /// The command ran and failed, or the executor failed for a reason
    /// that was not loss of contact.
    ///
    /// A non-zero exit code lands here rather than in
    /// [`Success`][Self::Success]: the host answered, and the answer is
    /// that the command did not work.
    ExecutionError,
    /// Asked, and its own deadline expired before an answer (#427).
    TimedOut,
    /// Never reached — the connection was lost or could not be made.
    ///
    /// Separated from [`ExecutionError`][Self::ExecutionError] because
    /// only this one and [`TimedOut`][Self::TimedOut] are worth retrying
    /// (#429): re-running a command that failed on the host would fail
    /// again.
    NoContact,
    /// Not asked: no command variant covers its OS family (#425).
    NotApplicable,
    /// Not asked for any other reason — no credentials, most often.
    Skipped,
    /// Stopped by the operation's cancellation before it answered (#437):
    /// interrupted while running, or never started.
    Cancelled,
}

impl HostState {
    /// Every state, in the order a summary lists them.
    pub const ALL: [Self; 8] = [
        Self::Success,
        Self::Divergent,
        Self::ExecutionError,
        Self::TimedOut,
        Self::NoContact,
        Self::NotApplicable,
        Self::Skipped,
        Self::Cancelled,
    ];

    /// Classify what the fan-out came back with.
    ///
    /// Never returns [`Divergent`][Self::Divergent] — see that variant.
    pub fn from_run(run: &HostRun) -> Self {
        match run {
            // `exit_code: None` means the command was killed or ended
            // abnormally, which `CommandResult` documents as *not* a
            // normal completion — so it is an execution error, not a
            // success with a missing number.
            HostRun::Answered(result) => match result.exit_code {
                Some(0) => Self::Success,
                _ => Self::ExecutionError,
            },
            HostRun::TimedOut(_) => Self::TimedOut,
            HostRun::Cancelled => Self::Cancelled,
            HostRun::Failed(error) => {
                if is_connection_lost(error) {
                    Self::NoContact
                } else {
                    Self::ExecutionError
                }
            }
        }
    }

    /// Whether the host said something back.
    pub fn answered(&self) -> bool {
        matches!(self, Self::Success | Self::Divergent | Self::ExecutionError)
    }

    /// Whether the host was asked and said nothing.
    pub fn unanswered(&self) -> bool {
        matches!(self, Self::TimedOut | Self::NoContact)
    }

    /// Whether the host was never asked.
    pub fn not_asked(&self) -> bool {
        matches!(self, Self::NotApplicable | Self::Skipped)
    }

    /// Whether the user's cancellation stopped this host (#437) — the
    /// fourth group, apart from silence and from "not asked".
    pub fn cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// Whether a retry could change this state (#429).
    ///
    /// Only silence is worth repeating. An execution error would fail the
    /// same way, and a host nobody asked has nothing to repeat.
    pub fn worth_retrying(&self) -> bool {
        self.unanswered()
    }

    /// Short label for a panel cell or a log line.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Success => "ok",
            Self::Divergent => "differs",
            Self::ExecutionError => "error",
            Self::TimedOut => "timeout",
            Self::NoContact => "no contact",
            Self::NotApplicable => "n/a",
            Self::Skipped => "skipped",
            Self::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for HostState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// Why a member of the operation was not asked anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotAsked {
    /// No command variant covers this host's OS family (#425).
    NotApplicable,
    /// Anything else — no credentials being the usual reason.
    Skipped,
}

impl From<NotAsked> for HostState {
    fn from(reason: NotAsked) -> Self {
        match reason {
            NotAsked::NotApplicable => Self::NotApplicable,
            NotAsked::Skipped => Self::Skipped,
        }
    }
}

/// One host's row in the result.
#[derive(Debug, Clone)]
pub struct HostStateRow {
    handle: HostHandle,
    host: String,
    state: HostState,
}

impl HostStateRow {
    /// The member this row is about.
    pub fn handle(&self) -> HostHandle {
        self.handle
    }

    /// The host's configured name, as the summary prints it.
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Where it ended up.
    pub fn state(&self) -> HostState {
        self.state
    }
}

// ---------------------------------------------------------------------------
// OperationResult
// ---------------------------------------------------------------------------

/// Every member of an operation with the state it ended in.
///
/// One row per member — including the members nobody asked, which is the
/// point: a host missing from the table is a host whose absence nobody
/// notices.
#[derive(Debug, Clone)]
pub struct OperationResult {
    operation: OperationId,
    rows: Vec<HostStateRow>,
}

impl OperationResult {
    /// Build the result of `op` from what the fan-out reported.
    ///
    /// `not_asked` names the members that had no task and why. Any member
    /// with neither a row in `report` nor an entry here is recorded as
    /// [`Skipped`][HostState::Skipped]: it was not asked, and no reason
    /// was given.
    ///
    /// Returns an error for input this operation cannot own, and checks
    /// **both** sides of it — the report as well as `not_asked`.
    ///
    /// The report is refused by its operation id, not by scanning its
    /// rows. Scanning was the first fix and it was incomplete: an
    /// **empty** foreign report has no row to catch it on, so every host
    /// came out [`Skipped`][HostState::Skipped] and the result claimed
    /// "nobody was asked" about an operation that ran. Both rounds of that
    /// were found in review.
    ///
    /// The other three refusals are contradictions about one host — a
    /// `not_asked` handle from elsewhere, a member that is both in
    /// `report` and in `not_asked`, and the same member named twice in
    /// `not_asked` with two reasons. Resolving any of them by preference
    /// (last wins, asked wins) would make the summary quietly wrong
    /// instead of loudly refused.
    pub fn build(
        op: &FleetOperation,
        report: &FleetRunReport,
        not_asked: &[(HostHandle, NotAsked)],
    ) -> Result<Self> {
        if report.operation() != op.id() {
            return Err(CoreError::Other(format!(
                "fleet result: report is for operation {}, not {}",
                report.operation(),
                op.id()
            )));
        }

        let mut reasons: BTreeMap<HostHandle, NotAsked> = BTreeMap::new();
        for (handle, reason) in not_asked {
            if op.member(*handle).is_none() {
                return Err(CoreError::Other(format!(
                    "fleet result: not-asked host is not in operation {}",
                    op.id()
                )));
            }
            if report.run_for(*handle).is_some() {
                return Err(CoreError::Other(format!(
                    "fleet result: host is both asked and not asked in operation {}",
                    op.id()
                )));
            }
            if reasons.insert(*handle, *reason).is_some() {
                return Err(CoreError::Other(format!(
                    "fleet result: duplicate not-asked host in operation {}",
                    op.id()
                )));
            }
        }

        // Handles and members are both in composition order, so zipping
        // them pairs each handle with its own member. Taking the name from
        // the member directly rather than looking the handle back up
        // leaves no "member not found" branch to decide what to do with:
        // an `unwrap_or_default` there would have put a nameless host in
        // the summary, and an `expect` would have added a panic path for a
        // case the types already rule out. Found in review.
        let rows = op
            .handles()
            .zip(op.members())
            .map(|(handle, member)| {
                let state = match report.run_for(handle) {
                    Some(run) => HostState::from_run(run),
                    None => reasons
                        .get(&handle)
                        .copied()
                        .map_or(HostState::Skipped, HostState::from),
                };
                HostStateRow {
                    handle,
                    host: member.name().to_string(),
                    state,
                }
            })
            .collect();

        Ok(Self {
            operation: op.id(),
            rows,
        })
    }

    /// Which operation this is the result of.
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    /// Every row, in the operation's composition order.
    pub fn rows(&self) -> &[HostStateRow] {
        &self.rows
    }

    /// The state of one member.
    pub fn state_for(&self, handle: HostHandle) -> Option<HostState> {
        self.rows
            .iter()
            .find(|row| row.handle == handle)
            .map(HostStateRow::state)
    }

    /// Record that a host's answer differs from the rest (#430).
    ///
    /// Only a host that answered cleanly can become
    /// [`Divergent`][HostState::Divergent]: a timeout has no value to
    /// differ with, and an execution error is already a finding of its
    /// own. Returns whether the row moved.
    pub fn mark_divergent(&mut self, handle: HostHandle) -> bool {
        match self
            .rows
            .iter_mut()
            .find(|row| row.handle == handle && row.state == HostState::Success)
        {
            Some(row) => {
                row.state = HostState::Divergent;
                true
            }
            None => false,
        }
    }

    /// Re-read an answered host's state once its answer has been
    /// interpreted — for a check whose exit code does not carry the verdict
    /// (#440: a missing file exits non-zero, and is an answer, not an
    /// error).
    ///
    /// Moves only between the answered states: a host that stayed silent or
    /// was never asked has no answer to re-read. Returns whether the row
    /// moved.
    pub(crate) fn reclassify_answer(&mut self, handle: HostHandle, state: HostState) -> bool {
        if !state.answered() {
            return false;
        }
        match self
            .rows
            .iter_mut()
            .find(|row| row.handle == handle && row.state.answered())
        {
            Some(row) if row.state != state => {
                row.state = state;
                true
            }
            _ => false,
        }
    }

    /// Roll the rows up into counts and the failure verdict.
    pub fn summary(&self) -> OperationSummary {
        let mut counts = BTreeMap::new();
        for row in &self.rows {
            *counts.entry(row.state).or_insert(0usize) += 1;
        }
        OperationSummary {
            operation: self.operation,
            hosts: self.rows.len(),
            counts,
        }
    }
}

// ---------------------------------------------------------------------------
// OperationSummary
// ---------------------------------------------------------------------------

/// What the operation amounts to, in counts.
#[derive(Debug, Clone)]
pub struct OperationSummary {
    operation: OperationId,
    hosts: usize,
    counts: BTreeMap<HostState, usize>,
}

impl OperationSummary {
    /// Which operation this summarises.
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    /// How many hosts took part, in every state together.
    pub fn hosts(&self) -> usize {
        self.hosts
    }

    /// How many hosts are in `state`.
    pub fn count(&self, state: HostState) -> usize {
        self.counts.get(&state).copied().unwrap_or(0)
    }

    /// Every state present, with its count, in [`HostState::ALL`] order.
    pub fn present(&self) -> Vec<(HostState, usize)> {
        HostState::ALL
            .into_iter()
            .filter_map(|state| {
                let count = self.count(state);
                (count > 0).then_some((state, count))
            })
            .collect()
    }

    /// How many hosts said something back.
    pub fn answered(&self) -> usize {
        self.total_where(HostState::answered)
    }

    /// How many hosts were asked and said nothing.
    ///
    /// The number [`headline`][Self::headline] always names.
    pub fn unanswered(&self) -> usize {
        self.total_where(HostState::unanswered)
    }

    /// How many hosts were never asked.
    pub fn not_asked(&self) -> usize {
        self.total_where(HostState::not_asked)
    }

    /// How many hosts the user's cancellation stopped (#437).
    pub fn cancelled(&self) -> usize {
        self.count(HostState::Cancelled)
    }

    /// Whether the operation failed.
    ///
    /// Only when **nobody** answered while at least one host was asked: a
    /// single reachable host is enough to build a summary from, and one
    /// dead machine out of twelve is a cell in the table (развилка 30).
    ///
    /// An operation where nobody was asked at all — every member not
    /// applicable or skipped — has not failed either. Nothing was
    /// attempted, so there is nothing to have failed; the issue's rule is
    /// written about hosts that stayed silent, and reading it to mean "a
    /// check that applies to nobody is a failure" would contradict #425,
    /// where "not applicable" is a state and not an error.
    pub fn is_failed(&self) -> bool {
        self.answered() == 0 && self.unanswered() > 0
    }

    /// One line that always says how many hosts did not answer.
    ///
    /// Including when none did not: the count is never dropped for being
    /// zero, because a summary that stays silent about silence is what
    /// #428 exists to prevent.
    pub fn headline(&self) -> String {
        let states = self
            .present()
            .into_iter()
            .map(|(state, count)| format!("{count} {state}"))
            .collect::<Vec<_>>()
            .join(", ");
        let verdict = if self.is_failed() {
            "failed: no host answered"
        } else if states.is_empty() {
            "no hosts"
        } else {
            &states
        };
        format!(
            "operation {}: {} of {} hosts answered, {} did not answer — {}",
            self.operation,
            self.answered(),
            self.hosts,
            self.unanswered(),
            verdict
        )
    }

    fn total_where(&self, predicate: impl Fn(&HostState) -> bool) -> usize {
        self.counts
            .iter()
            .filter(|(state, _)| predicate(state))
            .map(|(_, count)| *count)
            .sum()
    }
}

impl fmt::Display for OperationSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.headline())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use filar_core::config::{HostGroup, HostKeyPolicy, SshAuth, SshTarget};
    use filar_core::fleet_op::HostProgress;
    use filar_transport::{CommandExecutor, CommandResult};

    use crate::fleet_run::{run_on_fleet, HostTask};

    use super::*;

    // ── Fixtures ───────────────────────────────────────────────

    fn group(max_parallel: u32) -> HostGroup {
        HostGroup {
            name: "fleet".into(),
            match_tags: vec!["prod".into()],
            max_parallel,
            per_host_timeout_secs: 30,
            ..HostGroup::default()
        }
    }

    fn targets(count: usize) -> Vec<SshTarget> {
        (1..=count)
            .map(|i| SshTarget {
                name: format!("host-{i}"),
                host: format!("10.0.0.{i}"),
                port: 22,
                user: "admin".into(),
                auth: SshAuth::default(),
                host_key_policy: HostKeyPolicy::default(),
                tags: vec!["prod".into()],
            })
            .collect()
    }

    fn result(exit_code: Option<i32>) -> CommandResult {
        CommandResult {
            stdout: "5.15.0".into(),
            stderr: String::new(),
            exit_code,
            duration: Duration::from_millis(1),
            cwd: None,
        }
    }

    /// What the fake host does when asked.
    #[derive(Clone, Copy)]
    enum Behaviour {
        Answers(Option<i32>),
        Hangs,
        Unreachable,
        Fails,
    }

    struct FakeHost(Behaviour);

    #[filar_transport::async_trait]
    impl CommandExecutor for FakeHost {
        async fn run(&self, _command: &str) -> Result<CommandResult> {
            match self.0 {
                Behaviour::Answers(code) => Ok(result(code)),
                Behaviour::Hangs => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(result(Some(0)))
                }
                Behaviour::Unreachable => {
                    Err(CoreError::ConnectionLost("host is rebooting".into()))
                }
                Behaviour::Fails => Err(CoreError::Other("df: command not found".into())),
            }
        }

        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    fn host(behaviour: Behaviour) -> Arc<dyn CommandExecutor> {
        Arc::new(FakeHost(behaviour))
    }

    /// Run `behaviours` over a fresh operation, one host each, and build
    /// the result — with `not_asked` members left without a task.
    async fn run_and_build(
        behaviours: &[Behaviour],
        not_asked: &[(usize, NotAsked)],
    ) -> (FleetOperation, OperationResult) {
        let count = behaviours.len() + not_asked.len();
        let targets = targets(count);
        let mut op = FleetOperation::open(&group(count.max(1) as u32), &targets);
        let handles: Vec<HostHandle> = op.handles().collect();

        let asked: Vec<HostHandle> = handles
            .iter()
            .copied()
            .filter(|handle| {
                !not_asked
                    .iter()
                    .any(|(index, _)| handles[*index] == *handle)
            })
            .collect();
        let tasks = asked
            .iter()
            .zip(behaviours)
            .map(|(handle, behaviour)| HostTask::new(*handle, host(*behaviour), "uname -r"))
            .collect();

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");
        let reasons: Vec<(HostHandle, NotAsked)> = not_asked
            .iter()
            .map(|(index, reason)| (handles[*index], *reason))
            .collect();
        let result = OperationResult::build(&op, &report, &reasons).expect("valid result");
        (op, result)
    }

    // ── Classification ─────────────────────────────────────────

    #[test]
    fn a_clean_exit_is_success_and_any_other_is_an_execution_error() {
        assert_eq!(
            HostState::from_run(&HostRun::Answered(result(Some(0)))),
            HostState::Success
        );
        assert_eq!(
            HostState::from_run(&HostRun::Answered(result(Some(2)))),
            HostState::ExecutionError,
            "the host answered, and the answer is that the command failed"
        );
        assert_eq!(
            HostState::from_run(&HostRun::Answered(result(None))),
            HostState::ExecutionError,
            "a killed command did not complete normally"
        );
    }

    #[test]
    fn silence_is_split_by_cause() {
        assert_eq!(
            HostState::from_run(&HostRun::TimedOut(Duration::from_secs(30))),
            HostState::TimedOut
        );
        assert_eq!(
            HostState::from_run(&HostRun::Failed(CoreError::ConnectionLost("gone".into()))),
            HostState::NoContact
        );
        assert_eq!(
            HostState::from_run(&HostRun::Failed(CoreError::Other("no such file".into()))),
            HostState::ExecutionError,
            "a failure that is not loss of contact is not 'no contact'"
        );
    }

    #[test]
    fn only_silence_is_worth_retrying() {
        // #429 leans on exactly this split.
        for state in HostState::ALL {
            assert_eq!(
                state.worth_retrying(),
                matches!(state, HostState::TimedOut | HostState::NoContact),
                "{state} classified wrongly for retry"
            );
        }
    }

    #[test]
    fn the_four_groups_partition_every_state() {
        for state in HostState::ALL {
            let groups = [state.answered(), state.unanswered(), state.not_asked(), state.cancelled()]
                .into_iter()
                .filter(|in_group| *in_group)
                .count();
            assert_eq!(groups, 1, "{state} must be in exactly one group");
        }
    }

    // ── The DoD cases ──────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn every_state_is_represented_and_reaches_the_summary() {
        let (op, mut result) = run_and_build(
            &[
                Behaviour::Answers(Some(0)), // success
                Behaviour::Answers(Some(0)), // becomes divergent below
                Behaviour::Answers(Some(1)), // execution error
                Behaviour::Hangs,            // timeout
                Behaviour::Unreachable,      // no contact
                Behaviour::Fails,            // execution error
            ],
            &[(6, NotAsked::NotApplicable), (7, NotAsked::Skipped)],
        )
        .await;

        let handles: Vec<HostHandle> = op.handles().collect();
        assert!(result.mark_divergent(handles[1]), "a clean answer can differ");

        let summary = result.summary();
        assert_eq!(summary.hosts(), 8);
        assert_eq!(summary.count(HostState::Success), 1);
        assert_eq!(summary.count(HostState::Divergent), 1);
        assert_eq!(summary.count(HostState::ExecutionError), 2);
        assert_eq!(summary.count(HostState::TimedOut), 1);
        assert_eq!(summary.count(HostState::NoContact), 1);
        assert_eq!(summary.count(HostState::NotApplicable), 1);
        assert_eq!(summary.count(HostState::Skipped), 1);

        // All seven, and nothing lost on the way into the roll-up.
        assert_eq!(summary.present().len(), 7);
        let counted: usize = summary.present().iter().map(|(_, count)| count).sum();
        assert_eq!(counted, 8);
        assert_eq!(summary.answered(), 4);
        assert_eq!(summary.unanswered(), 2);
        assert_eq!(summary.not_asked(), 2);
        assert!(!summary.is_failed());
    }

    #[tokio::test(start_paused = true)]
    async fn eleven_of_twelve_still_build_a_summary_and_the_twelfth_is_marked() {
        let mut behaviours = vec![Behaviour::Answers(Some(0)); 11];
        behaviours.push(Behaviour::Unreachable);

        let (op, result) = run_and_build(&behaviours, &[]).await;
        let summary = result.summary();

        assert_eq!(summary.hosts(), 12);
        assert_eq!(summary.answered(), 11);
        assert_eq!(summary.unanswered(), 1);
        assert!(!summary.is_failed(), "eleven answers are a summary, not a failure");

        let handles: Vec<HostHandle> = op.handles().collect();
        assert_eq!(
            result.state_for(handles[11]),
            Some(HostState::NoContact),
            "the silent host is named, not dropped"
        );
        assert!(
            summary.headline().contains("1 did not answer"),
            "the count of silent hosts must be in the headline, got: {}",
            summary.headline()
        );
        assert!(summary.headline().contains("no contact"));
    }

    #[tokio::test(start_paused = true)]
    async fn nobody_answering_is_a_failed_operation() {
        let behaviours = vec![Behaviour::Unreachable; 12];

        let (_, result) = run_and_build(&behaviours, &[]).await;
        let summary = result.summary();

        assert_eq!(summary.answered(), 0);
        assert_eq!(summary.unanswered(), 12);
        assert!(summary.is_failed());
        assert!(
            summary.headline().contains("failed: no host answered"),
            "got: {}",
            summary.headline()
        );
        assert!(summary.headline().contains("12 did not answer"));
    }

    // ── Everything else ────────────────────────────────────────

    #[test]
    fn a_cancelled_host_is_neither_silent_nor_retried() {
        let state = HostState::from_run(&crate::fleet_run::HostRun::Cancelled);
        assert_eq!(state, HostState::Cancelled);
        assert_eq!(state.label(), "cancelled");
        // Its own group: the user stopped it, it did not go silent — so an
        // all-cancelled operation is not "failed" and nothing is retried.
        assert!(state.cancelled() && !state.unanswered());
        assert!(!state.worth_retrying());
    }

    #[tokio::test(start_paused = true)]
    async fn the_headline_names_zero_silent_hosts_too() {
        let (_, result) = run_and_build(&[Behaviour::Answers(Some(0))], &[]).await;

        let headline = result.summary().headline();
        assert!(
            headline.contains("0 did not answer"),
            "the count is stated even when it is zero, got: {headline}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_nobody_was_asked_has_not_failed() {
        // Every member not applicable: the check does not cover their OS
        // family (#425). Nothing was attempted, so nothing failed — and
        // calling this a failure would contradict "not applicable is a
        // state, not an error".
        let (_, result) = run_and_build(
            &[],
            &[(0, NotAsked::NotApplicable), (1, NotAsked::NotApplicable)],
        )
        .await;
        let summary = result.summary();

        assert_eq!(summary.answered(), 0);
        assert_eq!(summary.unanswered(), 0);
        assert_eq!(summary.not_asked(), 2);
        assert!(
            !summary.is_failed(),
            "nothing was asked, so nothing failed: {}",
            summary.headline()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_with_no_task_and_no_reason_is_skipped() {
        let targets = targets(2);
        let mut op = FleetOperation::open(&group(2), &targets);
        let handles: Vec<HostHandle> = op.handles().collect();

        let tasks = vec![HostTask::new(
            handles[0],
            host(Behaviour::Answers(Some(0))),
            "uname -r",
        )];
        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");
        let result = OperationResult::build(&op, &report, &[]).expect("valid result");

        assert_eq!(result.rows().len(), 2, "a row per member, asked or not");
        assert_eq!(result.state_for(handles[1]), Some(HostState::Skipped));
        assert_eq!(
            op.count_at(HostProgress::Pending),
            1,
            "and the operation still says nobody reached it"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn only_a_clean_answer_can_be_marked_divergent() {
        let (op, mut result) = run_and_build(
            &[
                Behaviour::Answers(Some(0)),
                Behaviour::Answers(Some(1)),
                Behaviour::Unreachable,
            ],
            &[],
        )
        .await;
        let handles: Vec<HostHandle> = op.handles().collect();

        assert!(result.mark_divergent(handles[0]));
        assert!(
            !result.mark_divergent(handles[1]),
            "an execution error is already its own finding"
        );
        assert!(
            !result.mark_divergent(handles[2]),
            "a host that never answered has no value to differ with"
        );
        // Marking twice is not a second divergence.
        assert!(!result.mark_divergent(handles[0]));

        let summary = result.summary();
        assert_eq!(summary.count(HostState::Divergent), 1);
        assert_eq!(summary.count(HostState::Success), 0);
        assert_eq!(summary.answered(), 2, "divergent hosts still answered");
    }

    #[tokio::test(start_paused = true)]
    async fn contradictory_or_foreign_input_is_refused() {
        let targets = targets(2);
        let mut mine = FleetOperation::open(&group(2), &targets);
        let theirs = FleetOperation::open(&group(2), &targets);
        let handles: Vec<HostHandle> = mine.handles().collect();

        let tasks = vec![HostTask::new(
            handles[0],
            host(Behaviour::Answers(Some(0))),
            "uname -r",
        )];
        let report = run_on_fleet(&mut mine, tasks).await.expect("valid tasks");

        let foreign = theirs.handles().next().expect("member");
        let error = OperationResult::build(&mine, &report, &[(foreign, NotAsked::Skipped)])
            .expect_err("a handle from another operation must not be accepted");
        assert!(
            error.to_string().contains("not in operation"),
            "got: {error}"
        );

        let error = OperationResult::build(
            &mine,
            &report,
            &[(handles[0], NotAsked::NotApplicable)],
        )
        .expect_err("a host cannot be both asked and not asked");
        assert!(
            error.to_string().contains("both asked and not asked"),
            "got: {error}"
        );

        let error = OperationResult::build(
            &mine,
            &report,
            &[
                (handles[1], NotAsked::NotApplicable),
                (handles[1], NotAsked::Skipped),
            ],
        )
        .expect_err("one host cannot have two reasons for not being asked");
        assert!(
            error.to_string().contains("duplicate not-asked host"),
            "got: {error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_report_from_another_operation_is_refused() {
        // The quiet failure this closes: handles carry their operation's
        // id (#426), so a foreign report matches no member — every host
        // would come out `Skipped` and `build` would return `Ok`. A result
        // claiming "nobody was asked" about an operation that ran is worse
        // than an error, because nothing about it looks wrong.
        let targets = targets(2);
        let mut theirs = FleetOperation::open(&group(2), &targets);
        let mine = FleetOperation::open(&group(2), &targets);

        let tasks = theirs
            .handles()
            .map(|handle| HostTask::new(handle, host(Behaviour::Answers(Some(0))), "uname -r"))
            .collect();
        let their_report = run_on_fleet(&mut theirs, tasks).await.expect("valid tasks");

        let error = OperationResult::build(&mine, &their_report, &[])
            .expect_err("a report from another operation must not be accepted");
        assert!(
            error.to_string().contains("is for operation"),
            "the error must name the problem, got: {error}"
        );

        // And the case scanning the rows could never catch: a foreign
        // report with nothing in it. Reading it as "nobody was asked"
        // would be a false summary about an operation that ran.
        let mut empty_op = FleetOperation::open(&group(2), &targets);
        let empty_foreign = run_on_fleet(&mut empty_op, Vec::new())
            .await
            .expect("no tasks");
        assert!(empty_foreign.is_empty());
        let error = OperationResult::build(&mine, &empty_foreign, &[])
            .expect_err("an empty report from another operation must not be accepted either");
        assert!(
            error.to_string().contains("is for operation"),
            "got: {error}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn every_row_carries_its_own_hosts_name() {
        // The pairing of handle to name is done by zipping two iterators
        // of the operation, so it is worth proving rather than assuming:
        // a row that names the wrong host would misattribute a state, and
        // one host's timeout would be read as another's.
        let targets = targets(4);
        let mut op = FleetOperation::open(&group(4), &targets);
        let handles: Vec<HostHandle> = op.handles().collect();

        // Give the hosts different states so a shifted pairing cannot
        // pass by looking the same everywhere.
        let behaviours = [
            Behaviour::Answers(Some(0)),
            Behaviour::Unreachable,
            Behaviour::Answers(Some(1)),
            Behaviour::Hangs,
        ];
        let tasks = handles
            .iter()
            .zip(behaviours)
            .map(|(handle, behaviour)| HostTask::new(*handle, host(behaviour), "uname -r"))
            .collect();
        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");
        let result = OperationResult::build(&op, &report, &[]).expect("valid result");

        let named: Vec<(&str, HostState)> = result
            .rows()
            .iter()
            .map(|row| (row.host(), row.state()))
            .collect();
        assert_eq!(
            named,
            vec![
                ("host-1", HostState::Success),
                ("host-2", HostState::NoContact),
                ("host-3", HostState::ExecutionError),
                ("host-4", HostState::TimedOut),
            ]
        );
        for (row, member) in result.rows().iter().zip(op.members()) {
            assert_eq!(row.host(), member.name());
            assert_eq!(row.handle(), op.handle_for(member.name()).expect("member"));
        }
    }

    #[test]
    fn labels_are_distinct_so_a_summary_cannot_blur_two_states() {
        let mut labels: Vec<&str> = HostState::ALL.iter().map(HostState::label).collect();
        labels.sort_unstable();
        let distinct = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), distinct);
    }
}
