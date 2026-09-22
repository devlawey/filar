//! Asking again the hosts that said nothing (#429).
//!
//! In a real estate of machines a host misses the first question for dull
//! reasons: it is mid-reboot, the link blipped, the box was busy enough to
//! miss its deadline. Reporting that as the final word makes the fleet
//! look worse than it is, and makes the person re-run the whole operation
//! to find out.
//!
//! # Why repeating is safe here, and only here
//!
//! A fleet operation is read-only by construction —
//! `filar_transport::ReadOnlyExecutor` (#419) is what each host's
//! executor is wrapped in, and its allowlist is compiled into the binary.
//! Running a read-only command twice is the same as running it once, so a
//! retry cannot double an effect: there is no effect to double. That is
//! the whole licence for this module, and it is why nothing here is
//! reusable for a write: outside the read-only gate, a silent repeat is
//! exactly the wrong behaviour.
//!
//! # Only silence is repeated
//!
//! [`HostState::worth_retrying`] decides, and it says yes to two states
//! only: [`TimedOut`][HostState::TimedOut] and
//! [`NoContact`][HostState::NoContact]. An execution error is **not**
//! retried — the command reached the host and failed there, so asking
//! again produces the same failure a second time and hides nothing new.
//! Success and divergence have their answer, and a host nobody asked has
//! nothing to repeat.
//!
//! The rule lives in [`HostState`] (#428), in one place, rather than being
//! restated here as a match on [`HostRun`][crate::fleet_run::HostRun]: two
//! copies of the same rule are two things to drift apart.
//!
//! # What a round here adds over the executor's own recovery
//!
//! The licence to repeat comes from the error contract, not from any one
//! transport: [`CoreError::ConnectionLost`] means the connection went
//! before the command was dispatched, so nothing ran remotely and a
//! repeat is safe. An executor is free to act on that itself — some
//! re-dial once inside a single [`run`][filar_transport::CommandExecutor::run]
//! call — and whether a particular one does is its own business, behind
//! the trait (AGENTS.md invariant 4). This module never asks.
//!
//! What it adds is time. An executor's own recovery answers "the socket
//! died just now"; a round here, **after a pause**, answers "the machine
//! is coming back in a few seconds", which no amount of instant re-dialling
//! reaches — a reboot outlives them all.
//!
//! The flip side is worth stating rather than discovering: this module
//! asks the executor again, it does not reach into its connection. Given
//! an executor that holds a dead connection and never renews it, every
//! round fails identically and the extra rounds cost a pause each for
//! nothing. Raised in review.
//!
//! # The operation cannot hang on retries
//!
//! [`RetryPolicy::attempts`] counts *total* attempts, not extra ones, and
//! is at least one. The whole run is therefore bounded by
//! `attempts × per_host_timeout + (attempts − 1) × pause` per host, with
//! `max_parallel` hosts at a time — a number that follows from the group's
//! own limits (#418) and needs no separate clock.
//!
//! A round that has nobody left to ask ends the loop early, so a fleet
//! that answers on the first try pays nothing for retries being enabled:
//! not even the pause.

use std::time::Duration;

use filar_core::error::{CoreError, Result};
use filar_core::fleet_op::FleetOperation;

use crate::fleet_result::HostState;
use crate::fleet_run::{run_on_fleet, FleetRunReport, HostTask};

// ---------------------------------------------------------------------------
// RetryPolicy
// ---------------------------------------------------------------------------

/// How many times to ask, and how long to wait in between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    attempts: u32,
    pause: Duration,
}

impl RetryPolicy {
    /// Total attempts a host gets by default, first one included.
    ///
    /// Three is a starting point, not a tuned number: it covers the blip
    /// and the short reboot window without keeping a person waiting on a
    /// machine that is simply down. The group (#418) carries no retry
    /// fields yet, so the caller decides; when it does, this default is
    /// what it should override.
    pub const DEFAULT_ATTEMPTS: u32 = 3;

    /// Default wait between rounds.
    ///
    /// Long enough that an immediate repeat does not just hit the same
    /// half-second of trouble, short enough to stay inside a person's
    /// patience for a fleet summary.
    pub const DEFAULT_PAUSE: Duration = Duration::from_secs(2);

    /// A policy of `attempts` total tries with `pause` between rounds.
    ///
    /// `attempts` is clamped up to one: zero attempts would mean asking
    /// nobody anything, which is not a retry policy but a way to get an
    /// empty report out of a group full of hosts.
    pub fn new(attempts: u32, pause: Duration) -> Self {
        Self {
            attempts: attempts.max(1),
            pause,
        }
    }

    /// Ask once and accept the answer — retries off.
    pub fn once() -> Self {
        Self::new(1, Duration::ZERO)
    }

    /// Total attempts per host, first one included. Never zero.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The wait between rounds.
    pub fn pause(&self) -> Duration {
        self.pause
    }
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self::new(Self::DEFAULT_ATTEMPTS, Self::DEFAULT_PAUSE)
    }
}

// ---------------------------------------------------------------------------
// The retrying fan-out
// ---------------------------------------------------------------------------

/// Run `tasks` across `op`, asking the silent hosts again up to
/// [`RetryPolicy::attempts`] times in total.
///
/// Each round is an ordinary fan-out (#427), so the group's parallelism
/// limit and per-host deadline hold inside a retry exactly as they do on
/// the first pass. Between rounds the whole fan-out waits
/// [`RetryPolicy::pause`] once — not per host — because the point of the
/// wait is to let whatever was wrong pass, and that clock runs for
/// everybody at the same time.
///
/// A host that answers on a later round replaces its earlier row, so the
/// summary shows what the fleet finally said, not what it said first. The
/// row order stays the order of the tasks handed in.
///
/// Errors are the same ones [`run_on_fleet`] returns — a task list this
/// operation cannot own — and they are raised before anything runs.
pub async fn run_with_retries(
    op: &mut FleetOperation,
    tasks: Vec<HostTask>,
    policy: RetryPolicy,
) -> Result<FleetRunReport> {
    let mut report = run_on_fleet(op, tasks.clone()).await?;

    for _ in 1..policy.attempts() {
        let again: Vec<HostTask> = tasks
            .iter()
            .filter(|task| {
                report
                    .run_for(task.handle())
                    .is_some_and(|run| HostState::from_run(run).worth_retrying())
            })
            .cloned()
            .collect();

        if again.is_empty() {
            break;
        }

        tokio::time::sleep(policy.pause()).await;
        let round = run_on_fleet(op, again).await?;
        // Same operation by construction — both rounds ran on `op` — so
        // this cannot fail; it is checked rather than assumed because a
        // mismatch would file one host's answer under another's name.
        report.absorb(round).map_err(|error| {
            CoreError::Other(format!("fleet retry: could not merge a retry round: {error}"))
        })?;
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use filar_core::config::{HostGroup, HostKeyPolicy, SshAuth, SshTarget};
    use filar_core::fleet_op::HostHandle;
    use filar_transport::{CommandExecutor, CommandResult};

    use crate::fleet_result::{NotAsked, OperationResult};

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

    fn answer(stdout: &str) -> CommandResult {
        CommandResult {
            stdout: stdout.to_string(),
            stderr: String::new(),
            exit_code: Some(0),
            duration: Duration::from_millis(1),
            cwd: None,
        }
    }

    /// What a host does on a given attempt.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum Attempt {
        Answers,
        /// The command ran and failed — must not be repeated.
        Fails,
        Unreachable,
        Hangs,
    }

    /// A host that behaves differently on each attempt, so a test can say
    /// "silent first, answers second" without timing tricks.
    struct Flaky {
        script: Vec<Attempt>,
        calls: AtomicUsize,
    }

    impl Flaky {
        fn new(script: &[Attempt]) -> Arc<Self> {
            Arc::new(Self {
                script: script.to_vec(),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[filar_transport::async_trait]
    impl CommandExecutor for Flaky {
        async fn run(&self, _command: &str) -> Result<CommandResult> {
            let attempt = self.calls.fetch_add(1, Ordering::SeqCst);
            // Past the end of the script the host keeps doing the last
            // thing it did, so a test cannot pass by running out of steps.
            let behaviour = self
                .script
                .get(attempt)
                .copied()
                .or_else(|| self.script.last().copied())
                .unwrap_or(Attempt::Unreachable);
            match behaviour {
                Attempt::Answers => Ok(answer("5.15.0")),
                Attempt::Fails => Err(CoreError::Other("df: command not found".into())),
                Attempt::Unreachable => {
                    Err(CoreError::ConnectionLost("host is rebooting".into()))
                }
                Attempt::Hangs => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(answer("too late"))
                }
            }
        }

        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    fn tasks_for(op: &FleetOperation, hosts: &[Arc<Flaky>]) -> Vec<HostTask> {
        op.handles()
            .zip(hosts)
            .map(|(handle, host)| {
                HostTask::new(handle, host.clone() as Arc<dyn CommandExecutor>, "uname -r")
            })
            .collect()
    }

    fn states(op: &FleetOperation, report: &FleetRunReport) -> Vec<HostState> {
        let result = OperationResult::build(op, report, &[]).expect("valid result");
        result.rows().iter().map(|row| row.state()).collect()
    }

    // ── The DoD cases ──────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn a_host_that_answers_on_the_second_try_is_a_success_in_the_summary() {
        let targets = targets(2);
        let mut op = FleetOperation::open(&group(2), &targets);
        let steady = Flaky::new(&[Attempt::Answers]);
        let flaky = Flaky::new(&[Attempt::Unreachable, Attempt::Answers]);
        let hosts = vec![steady.clone(), flaky.clone()];

        let tasks = tasks_for(&op, &hosts);

        let report = run_with_retries(
            &mut op,

            tasks,
            RetryPolicy::new(3, Duration::from_secs(2)),
        )
        .await
        .expect("valid tasks");

        assert_eq!(
            states(&op, &report),
            vec![HostState::Success, HostState::Success],
            "the second-try answer replaces the first-try silence"
        );
        assert_eq!(steady.calls(), 1, "a host that answered is not asked again");
        assert_eq!(flaky.calls(), 2, "the silent one is asked exactly once more");

        let summary = OperationResult::build(&op, &report, &[])
            .expect("valid result")
            .summary();
        assert_eq!(summary.answered(), 2);
        assert_eq!(summary.unanswered(), 0);
        assert!(!summary.is_failed());
    }

    #[tokio::test(start_paused = true)]
    async fn attempts_are_bounded_and_the_operation_does_not_hang() {
        let targets = targets(1);
        let mut op = FleetOperation::open(&group(1), &targets);
        // Never answers, on any attempt.
        let dead = Flaky::new(&[Attempt::Unreachable]);

        let tasks = tasks_for(&op, std::slice::from_ref(&dead));
        let report = run_with_retries(
            &mut op,
            tasks,
            RetryPolicy::new(3, Duration::from_secs(2)),
        )
        .await
        .expect("valid tasks");

        assert_eq!(
            dead.calls(),
            3,
            "three attempts in total, not three retries on top of the first"
        );
        assert_eq!(states(&op, &report), vec![HostState::NoContact]);
        let summary = OperationResult::build(&op, &report, &[])
            .expect("valid result")
            .summary();
        assert!(summary.is_failed(), "nobody answered: {}", summary.headline());
    }

    #[tokio::test(start_paused = true)]
    async fn an_execution_error_is_not_repeated() {
        let targets = targets(1);
        let mut op = FleetOperation::open(&group(1), &targets);
        // Would answer on a second attempt — but must never get one.
        let broken = Flaky::new(&[Attempt::Fails, Attempt::Answers]);

        let tasks = tasks_for(&op, std::slice::from_ref(&broken));
        let report = run_with_retries(
            &mut op,
            tasks,
            RetryPolicy::new(3, Duration::from_secs(2)),
        )
        .await
        .expect("valid tasks");

        assert_eq!(
            broken.calls(),
            1,
            "the command reached the host and failed there: asking again \
             would fail the same way"
        );
        assert_eq!(states(&op, &report), vec![HostState::ExecutionError]);
    }

    // ── Everything else ────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn a_timeout_is_retried_too_not_only_a_lost_connection() {
        let targets = targets(1);
        let mut op = FleetOperation::open(&group(1), &targets);
        let wedged = Flaky::new(&[Attempt::Hangs, Attempt::Answers]);

        let tasks = tasks_for(&op, std::slice::from_ref(&wedged));
        let report = run_with_retries(
            &mut op,
            tasks,
            RetryPolicy::new(2, Duration::from_secs(2)),
        )
        .await
        .expect("valid tasks");

        assert_eq!(wedged.calls(), 2);
        assert_eq!(states(&op, &report), vec![HostState::Success]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_fleet_that_answers_at_once_pays_nothing_for_retries() {
        let targets = targets(3);
        let mut op = FleetOperation::open(&group(3), &targets);
        let hosts: Vec<_> = (0..3).map(|_| Flaky::new(&[Attempt::Answers])).collect();

        let tasks = tasks_for(&op, &hosts);
        let started = tokio::time::Instant::now();
        let report = run_with_retries(
            &mut op,
            tasks,
            RetryPolicy::new(5, Duration::from_secs(60)),
        )
        .await
        .expect("valid tasks");
        let elapsed = started.elapsed();

        assert_eq!(report.answered(), 3);
        for host in &hosts {
            assert_eq!(host.calls(), 1);
        }
        assert!(
            elapsed < Duration::from_secs(60),
            "with nobody to ask again the loop ends before the pause, took {elapsed:?}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn one_attempt_means_no_retry_at_all() {
        let targets = targets(1);
        let mut op = FleetOperation::open(&group(1), &targets);
        let flaky = Flaky::new(&[Attempt::Unreachable, Attempt::Answers]);

        let tasks = tasks_for(&op, std::slice::from_ref(&flaky));
        let report = run_with_retries(&mut op, tasks, RetryPolicy::once())
            .await
            .expect("valid tasks");

        assert_eq!(flaky.calls(), 1);
        assert_eq!(states(&op, &report), vec![HostState::NoContact]);
    }

    #[test]
    fn zero_attempts_is_read_as_one() {
        // A policy that asks nobody anything is not a retry policy; it is
        // a way to get an empty report out of a group full of hosts.
        assert_eq!(RetryPolicy::new(0, Duration::ZERO).attempts(), 1);
        assert_eq!(RetryPolicy::default().attempts(), RetryPolicy::DEFAULT_ATTEMPTS);
        assert_eq!(RetryPolicy::default().pause(), RetryPolicy::DEFAULT_PAUSE);
    }

    #[tokio::test(start_paused = true)]
    async fn only_the_silent_hosts_are_asked_again() {
        let targets = targets(4);
        let mut op = FleetOperation::open(&group(4), &targets);
        let hosts = vec![
            Flaky::new(&[Attempt::Answers]),                     // done
            Flaky::new(&[Attempt::Fails]),                        // not retried
            Flaky::new(&[Attempt::Unreachable, Attempt::Answers]), // retried
            Flaky::new(&[Attempt::Hangs, Attempt::Answers]),      // retried
        ];

        let tasks = tasks_for(&op, &hosts);

        let report = run_with_retries(
            &mut op,

            tasks,
            RetryPolicy::new(2, Duration::from_secs(1)),
        )
        .await
        .expect("valid tasks");

        let calls: Vec<usize> = hosts.iter().map(|host| host.calls()).collect();
        assert_eq!(calls, vec![1, 1, 2, 2]);
        assert_eq!(
            states(&op, &report),
            vec![
                HostState::Success,
                HostState::ExecutionError,
                HostState::Success,
                HostState::Success,
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn rows_keep_their_order_and_their_host_after_a_retry() {
        // The merge replaces rows in place; a retry must not reshuffle the
        // report or move an answer onto another host's row.
        let targets = targets(3);
        let mut op = FleetOperation::open(&group(3), &targets);
        let hosts = vec![
            Flaky::new(&[Attempt::Unreachable, Attempt::Answers]),
            Flaky::new(&[Attempt::Answers]),
            Flaky::new(&[Attempt::Unreachable, Attempt::Answers]),
        ];
        let handles: Vec<HostHandle> = op.handles().collect();

        let tasks = tasks_for(&op, &hosts);

        let report = run_with_retries(
            &mut op,

            tasks,
            RetryPolicy::new(2, Duration::from_secs(1)),
        )
        .await
        .expect("valid tasks");

        let order: Vec<HostHandle> = report.outcomes().iter().map(|o| o.handle()).collect();
        assert_eq!(order, handles, "retried rows stay where they were");

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
                ("host-2", HostState::Success),
                ("host-3", HostState::Success),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_host_not_asked_at_all_is_never_retried_into_existence() {
        let targets = targets(2);
        let mut op = FleetOperation::open(&group(2), &targets);
        let asked = Flaky::new(&[Attempt::Unreachable, Attempt::Answers]);
        let handles: Vec<HostHandle> = op.handles().collect();

        let report = run_with_retries(
            &mut op,
            vec![HostTask::new(
                handles[0],
                asked.clone() as Arc<dyn CommandExecutor>,
                "uname -r",
            )],
            RetryPolicy::new(3, Duration::from_secs(1)),
        )
        .await
        .expect("valid tasks");

        assert_eq!(report.len(), 1, "a report row per task, retries included");
        let result = OperationResult::build(
            &op,
            &report,
            &[(handles[1], NotAsked::NotApplicable)],
        )
        .expect("valid result");
        assert_eq!(
            result.state_for(handles[1]),
            Some(HostState::NotApplicable),
            "a host the check does not apply to is not dragged into a retry"
        );
        assert_eq!(result.state_for(handles[0]), Some(HostState::Success));
    }
}
