//! Fanning one question out across a fleet operation's hosts (#427).
//!
//! The operation (#426) says who takes part and under which limits, and it
//! needs no runtime to say it. This module is the half that does: run one
//! command per host, never more than the group's `max_parallel` at a time,
//! and give every host its own deadline.
//!
//! # The deadline is per host, not per operation
//!
//! A shared deadline makes the slowest machine the deadline for everyone:
//! one box wedged in I/O and the summary for the other eleven never
//! arrives. Each host is timed on its own, so a wedged one turns into a
//! single [`HostRun::TimedOut`] cell while the rest finish normally. That
//! is also why the fan-out has no overall timeout of its own — the bound
//! on the whole operation is already `hosts / max_parallel` deadlines, and
//! a second, outer clock would only cut short hosts that were answering.
//!
//! # The limit is structural, not advisory
//!
//! At most `max_parallel` host futures exist at any moment: the driver
//! launches a new one only when a slot frees up. Nothing counts
//! connections after the fact and hopes the number stays low —
//! `max_parallel` is what the group's owner set as the acceptable blast
//! radius on the network, and prod and test deserve different treatment
//! (#418).
//!
//! # What dropping the fan-out does, and what it does not
//!
//! The fan-out is structured: the host futures live inside
//! [`run_on_fleet`]'s own future, never in a [`tokio::spawn`], so dropping
//! it drops them and leaves no orphaned task still talking to hosts. That
//! is where the guarantee ends. A drop cannot `await`, so **no
//! cancellation is sent**: a command already written to a host's shell
//! keeps running there, and that shell stays busy with it. Only the
//! per-host deadline path below cancels remotely.
//!
//! So dropping this future is a *local* stop, not a fleet-wide one.
//!
//! # Cancelling the operation (#437)
//!
//! The fleet-wide stop is [`run_on_fleet_until`]: it takes a
//! [`CancellationToken`] and keeps running *through* the cancellation
//! instead of being dropped. Once the token fires, no queued host is
//! launched, and every running host gets the same treatment as one whose
//! deadline expired — a Ctrl-C sent to its shell, and its run future polled
//! until it observes it — so the command stops on the host, not only the
//! wait for it (the lesson of #394, multiplied by the number of hosts).
//! Hosts that answered before the cancellation keep their rows; the rest
//! come out [`HostRun::Cancelled`], and the report is returned as usual.
//!
//! # What this module deliberately does not decide
//!
//! **The command is an input.** Resolving a check's per-OS variant (#425)
//! is the caller's job, so nothing here probes a host on its own
//! initiative. That keeps the open question from #425 — whether the first
//! OS probe on a host needs the user's confirmation gate — where it
//! belongs: with whoever decides to run a probe, not inside a library the
//! decision would then be baked into. `os_probe::detect` still has no
//! caller in the workspace.
//!
//! **Read-only is the executor's guarantee.** Pass each host's own
//! executor, which for a fleet host is the `ReadOnlyExecutor`-wrapped one
//! (#419). This module runs whatever string it is handed; the transport,
//! not this code, is what makes a write impossible.
//!
//! **What an answer means is #428's.** A non-zero exit code is still an
//! answer and arrives as [`HostRun::Answered`]. Success, divergence,
//! execution error, no contact, not applicable and skipped are the outcome
//! set of the operation result, and the mapping from these three runs into
//! those states lands with it.

use std::sync::Arc;
use std::time::Duration;

use filar_core::error::{CoreError, Result};
use filar_core::fleet_op::{FleetOperation, HostHandle, HostProgress, OperationId};
use filar_transport::{CommandExecutor, CommandResult};
use futures::stream::{FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

/// One host's share of a fan-out: which member, how to reach it, what to
/// run there.
///
/// A task per *participating* host, not necessarily one per member: a host
/// with no credentials, or one no variant of the check applies to, simply
/// has no task and stays [`HostProgress::Pending`].
///
/// `Clone` is cheap — an `Arc` bump and a command string — and it exists
/// so a retry round can ask the same host the same thing again (#429)
/// without the caller rebuilding the task from parts and risking a
/// different command the second time.
#[derive(Clone)]
pub struct HostTask {
    handle: HostHandle,
    executor: Arc<dyn CommandExecutor>,
    command: String,
}

impl HostTask {
    /// Aim `command` at the member `handle` refers to, over `executor`.
    ///
    /// `handle` must come from the operation the task is run against, and
    /// `executor` should be that host's own — including its read-only
    /// wrapper (#419), which this module does not add.
    pub fn new(
        handle: HostHandle,
        executor: Arc<dyn CommandExecutor>,
        command: impl Into<String>,
    ) -> Self {
        Self {
            handle,
            executor,
            command: command.into(),
        }
    }

    /// The member this task belongs to.
    pub fn handle(&self) -> HostHandle {
        self.handle
    }

    /// The command as it will be issued, verbatim.
    pub fn command(&self) -> &str {
        &self.command
    }
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// What came back from one host.
#[derive(Debug)]
pub enum HostRun {
    /// The command ran and the host answered.
    ///
    /// Including when it answered badly: a non-zero exit code is an
    /// answer, and reading it is #428's job.
    Answered(CommandResult),
    /// The host's own deadline expired before an answer arrived. The
    /// command was asked to cancel, best-effort, so the host's shell is
    /// not left waiting on it.
    TimedOut(Duration),
    /// No answer at all: the executor failed before one could arrive.
    ///
    /// The error is kept rather than flattened to a string because it
    /// carries the distinction #428 needs —
    /// [`CoreError::ConnectionLost`] is "no contact", anything else is an
    /// execution error, and only the first is worth retrying (#429).
    Failed(CoreError),
    /// The operation was cancelled before this host answered (#437): it was
    /// running and got a Ctrl-C, or it was still queued and never started.
    Cancelled,
}

/// One host's row in the report.
#[derive(Debug)]
pub struct HostOutcome {
    handle: HostHandle,
    run: HostRun,
}

impl HostOutcome {
    /// Which member this row is about.
    pub fn handle(&self) -> HostHandle {
        self.handle
    }

    /// What came back from it.
    pub fn run(&self) -> &HostRun {
        &self.run
    }
}

/// What one fan-out produced.
///
/// Rows keep the order of the tasks handed in, not the order the hosts
/// happened to answer in: a summary that reshuffles itself by network
/// timing is unreadable next to the panel (#431).
///
/// The report names the operation it came from, so a consumer can refuse
/// one that belongs to somebody else. Checking the rows is not enough for
/// that: an **empty** foreign report has no row to catch it on, and
/// reading it as "nobody was asked" would be a false summary about an
/// operation that ran (#428 review).
#[derive(Debug)]
pub struct FleetRunReport {
    operation: OperationId,
    outcomes: Vec<HostOutcome>,
}

impl FleetRunReport {
    /// The operation this report is about.
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    /// Take `later`'s rows in place of this report's, host by host.
    ///
    /// For a retry round (#429): the hosts asked again keep their new row,
    /// the hosts that were not asked again keep the row they had, and the
    /// order stays the order of the first round. A host in `later` that
    /// this report has no row for is added at the end, so nothing is
    /// silently dropped.
    ///
    /// Refuses a report for a different operation. Merging rows across
    /// operations would file one fleet's answers under another's hosts —
    /// the same mistake #428's `build` refuses on its own input.
    pub fn absorb(&mut self, later: FleetRunReport) -> Result<()> {
        if later.operation != self.operation {
            return Err(CoreError::Other(format!(
                "fleet run: cannot absorb a report for operation {} into {}",
                later.operation, self.operation
            )));
        }
        for outcome in later.outcomes {
            match self
                .outcomes
                .iter_mut()
                .find(|existing| existing.handle == outcome.handle)
            {
                Some(existing) => *existing = outcome,
                None => self.outcomes.push(outcome),
            }
        }
        Ok(())
    }

    /// Every row, in task order.
    pub fn outcomes(&self) -> &[HostOutcome] {
        &self.outcomes
    }

    /// How many hosts were asked.
    pub fn len(&self) -> usize {
        self.outcomes.len()
    }

    /// Whether nobody was asked — an empty operation, or no task for any
    /// member.
    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }

    /// The run for one member, if it had a task.
    pub fn run_for(&self, handle: HostHandle) -> Option<&HostRun> {
        self.outcomes
            .iter()
            .find(|o| o.handle == handle)
            .map(HostOutcome::run)
    }

    /// How many hosts answered, well or badly.
    pub fn answered(&self) -> usize {
        self.count(|run| matches!(run, HostRun::Answered(_)))
    }

    /// How many hosts ran out of time.
    pub fn timed_out(&self) -> usize {
        self.count(|run| matches!(run, HostRun::TimedOut(_)))
    }

    /// How many hosts could not be reached or failed before answering.
    pub fn failed(&self) -> usize {
        self.count(|run| matches!(run, HostRun::Failed(_)))
    }

    /// How many hosts the operation's cancellation stopped (#437).
    pub fn cancelled(&self) -> usize {
        self.count(|run| matches!(run, HostRun::Cancelled))
    }

    fn count(&self, predicate: impl Fn(&HostRun) -> bool) -> usize {
        self.outcomes
            .iter()
            .filter(|o| predicate(&o.run))
            .count()
    }
}

// ---------------------------------------------------------------------------
// The fan-out
// ---------------------------------------------------------------------------

/// Run `tasks` across `op`'s hosts, at most [`max_parallel`] at a time,
/// each on its own [`per_host_timeout`] deadline.
///
/// Progress is written into `op` as it happens: a host becomes
/// [`Running`][HostProgress::Running] when a slot opens for it and
/// [`Done`][HostProgress::Done] when its run ends, so a member that the
/// fan-out never reached stays `Pending` and says so.
///
/// A `max_parallel` of zero is read as one, not as none: the field is
/// public and a group that never went through
/// [`HostGroup::validate`][filar_core::config::HostGroup::validate] can
/// carry a zero, and taking it literally would launch nothing at all and
/// return an empty report for a group full of hosts.
///
/// Returns an error, before running anything, for a task list this
/// operation cannot own: a handle from another operation, or two tasks for
/// the same host. Both are caller bugs that would otherwise show up as a
/// quietly wrong summary — one host's answer filed under another's name.
/// A member with no task is not an error.
///
/// [`max_parallel`]: FleetOperation::max_parallel
/// [`per_host_timeout`]: FleetOperation::per_host_timeout
pub async fn run_on_fleet(
    op: &mut FleetOperation,
    tasks: Vec<HostTask>,
) -> Result<FleetRunReport> {
    run_on_fleet_until(op, tasks, &CancellationToken::new()).await
}

/// [`run_on_fleet`] that stops when `cancel` fires (#437).
///
/// After cancellation no queued host is launched, every running host is
/// interrupted on the host itself (see the module docs), and the report
/// still comes back: rows of hosts that finished first are kept, the rest
/// are [`HostRun::Cancelled`]. Cancelling before the call leaves every
/// host cancelled without contacting any.
pub async fn run_on_fleet_until(
    op: &mut FleetOperation,
    tasks: Vec<HostTask>,
    cancel: &CancellationToken,
) -> Result<FleetRunReport> {
    run_on_fleet_observed(op, tasks, cancel, &mut |_, _| {}).await
}

/// [`run_on_fleet_until`] that also reports every host the moment its run
/// ends (#439), so a status line can count answers while the operation is
/// still going rather than only once it is over. `on_host` sees each
/// finished host exactly once; hosts the cancellation kept from starting
/// are not reported (they never ran).
pub async fn run_on_fleet_observed(
    op: &mut FleetOperation,
    tasks: Vec<HostTask>,
    cancel: &CancellationToken,
    on_host: &mut (dyn FnMut(HostHandle, &HostRun) + Send),
) -> Result<FleetRunReport> {
    validate_tasks(op, &tasks)?;

    let deadline = op.per_host_timeout();
    // A group that never went through `HostGroup::validate` can carry a
    // zero here; one host at a time is the honest reading of "no more than
    // zero at a time", and it cannot stall the fan-out the way a literal
    // zero would.
    let limit = op.max_parallel().max(1) as usize;

    let handles: Vec<HostHandle> = tasks.iter().map(HostTask::handle).collect();
    let mut finished: Vec<(usize, HostRun)> = Vec::with_capacity(tasks.len());

    let mut queue = tasks.into_iter().enumerate();
    let mut in_flight = FuturesUnordered::new();

    loop {
        // Fill the free slots. The limit holds structurally: no more than
        // `limit` futures exist here at once, so no more than `limit`
        // connections are in use.
        // Nothing new starts once the operation is cancelled; the queue
        // is drained into `Cancelled` rows below.
        while in_flight.len() < limit && !cancel.is_cancelled() {
            match queue.next() {
                Some((index, task)) => {
                    op.set_progress(handles[index], HostProgress::Running);
                    in_flight.push(run_one(index, task, deadline, cancel));
                }
                None => break,
            }
        }

        match in_flight.next().await {
            Some((index, run)) => {
                op.set_progress(handles[index], HostProgress::Done);
                on_host(handles[index], &run);
                finished.push((index, run));
            }
            // Nothing in flight and nothing left to launch.
            None => break,
        }
    }

    // Hosts the cancellation kept from ever starting.
    for (index, _) in queue {
        op.set_progress(handles[index], HostProgress::Done);
        finished.push((index, HostRun::Cancelled));
    }

    // Answers arrive in whatever order the network allows; the report is
    // ordered by task. Sorting the collected rows rather than filling a
    // slot per task keeps the happy path free of an "unreachable" unwrap.
    finished.sort_by_key(|(index, _)| *index);
    let outcomes = finished
        .into_iter()
        .map(|(index, run)| HostOutcome {
            handle: handles[index],
            run,
        })
        .collect();

    Ok(FleetRunReport {
        operation: op.id(),
        outcomes,
    })
}

/// Ask one host, under its own deadline, until `cancel` fires.
///
/// The `run` future is pinned and polled *through* the timeout rather than
/// handed to it, so that it outlives the deadline. That is not a style
/// choice: an executor registers its cancellation waiter inside `run`
/// (`LocalExecutor` selects on a `Notify`), and a `timeout` that owns the
/// future drops that waiter when the deadline fires. `cancel()` would then
/// find nobody listening, `Notify::notify_one` would store a permit
/// instead, and the *next* command on that host would consume it and come
/// back as "cancelled by user" though nothing cancelled it — a timed-out
/// host poisoning its own next check, and its retry (#429) with it. Found
/// in review.
///
/// The operation's cancellation (#437) takes the same path for the same
/// reason: the host is interrupted and its run future drained, and the
/// cell reads [`HostRun::Cancelled`].
async fn run_one(
    index: usize,
    task: HostTask,
    deadline: Duration,
    cancel: &CancellationToken,
) -> (usize, HostRun) {
    let run_future = task.executor.run(&task.command);
    tokio::pin!(run_future);

    // `biased`: an answer that is already there wins over a cancellation
    // that arrives in the same poll — a finished result is kept, not
    // thrown away.
    let outcome = tokio::select! {
        biased;
        finished = tokio::time::timeout(deadline, &mut run_future) => Some(finished),
        _ = cancel.cancelled() => None,
    };
    let run = match outcome {
        Some(Ok(Ok(result))) => HostRun::Answered(result),
        Some(Ok(Err(error))) => HostRun::Failed(error),
        Some(Err(_)) => {
            interrupt(&task, &mut run_future, deadline, "timed out").await;
            HostRun::TimedOut(deadline)
        }
        None => {
            interrupt(&task, &mut run_future, deadline, "was cancelled").await;
            HostRun::Cancelled
        }
    };
    (index, run)
}

/// Stop a command still running on `task`'s host.
///
/// The command keeps running there, and the persistent shell stays busy,
/// until something interrupts it — walking away leaves the session
/// unusable for the next check. Cancellation is best-effort and bounded by
/// the host's deadline: a host too wedged to accept a Ctrl-C must not
/// become a second wedge here.
async fn interrupt<F>(task: &HostTask, run_future: &mut std::pin::Pin<&mut F>, deadline: Duration, why: &str)
where
    F: std::future::Future<Output = Result<CommandResult>>,
{
    match tokio::time::timeout(deadline, task.executor.cancel()).await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(
            handle = ?task.handle,
            %error,
            "fleet host {why} and could not be cancelled"
        ),
        Err(_) => tracing::warn!(
            handle = ?task.handle,
            "fleet host {why} and did not answer the cancellation either"
        ),
    }
    // Let the run future observe the cancellation it was sent. Keeping it
    // alive is only half the fix: a notified waiter that is dropped before
    // it is polled hands the notification on, and with no other waiter it
    // lands back as a stored permit — the same poisoning by a longer
    // route. Polling here consumes it. Bounded by the same deadline, and
    // its answer is discarded: whatever it says now, the cell is decided.
    if tokio::time::timeout(deadline, run_future.as_mut()).await.is_err() {
        tracing::warn!(
            handle = ?task.handle,
            "fleet host did not finish even after being cancelled"
        );
    }
}

/// Reject a task list the operation cannot own, before anything runs.
fn validate_tasks(op: &FleetOperation, tasks: &[HostTask]) -> Result<()> {
    let mut seen: Vec<HostHandle> = Vec::with_capacity(tasks.len());
    for task in tasks {
        if op.member(task.handle).is_none() {
            return Err(CoreError::Other(format!(
                "fleet run: task for a host that is not in operation {}",
                op.id()
            )));
        }
        if seen.contains(&task.handle) {
            return Err(CoreError::Other(format!(
                "fleet run: two tasks for the same host in operation {}",
                op.id()
            )));
        }
        seen.push(task.handle);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use filar_core::config::{HostGroup, HostKeyPolicy, SshAuth, SshTarget};
    use filar_transport::CommandResult;

    use super::*;

    // ── Fixtures ───────────────────────────────────────────────

    fn group(max_parallel: u32, per_host_timeout_secs: u64) -> HostGroup {
        HostGroup {
            name: "fleet".into(),
            match_tags: vec!["prod".into()],
            max_parallel,
            per_host_timeout_secs,
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
            duration: Duration::from_millis(0),
            cwd: None,
        }
    }

    /// Shared across the fake hosts of one test: how many are inside
    /// `run` right now, and the most there have ever been at once.
    #[derive(Default)]
    struct Concurrency {
        in_flight: AtomicUsize,
        peak: AtomicUsize,
    }

    impl Concurrency {
        /// Count one host as in flight until the returned guard drops.
        ///
        /// A guard rather than a matching `leave()` call, because a host
        /// that times out has its `run` future dropped mid-await: a manual
        /// decrement after the await would never happen and the count
        /// would drift up for the rest of the test.
        fn enter(self: &Arc<Self>) -> InFlight {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            InFlight(self.clone())
        }

        fn peak(&self) -> usize {
            self.peak.load(Ordering::SeqCst)
        }

        fn in_flight(&self) -> usize {
            self.in_flight.load(Ordering::SeqCst)
        }
    }

    struct InFlight(Arc<Concurrency>);

    impl Drop for InFlight {
        fn drop(&mut self) {
            self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// What a fake host does when asked.
    enum Behaviour {
        /// Answers after `Duration`.
        Answers(Duration, &'static str),
        /// Never answers — the caller's deadline is what ends it.
        Hangs,
        /// Fails without answering.
        Fails(&'static str),
        /// The connection is gone.
        Unreachable,
    }

    struct FakeHost {
        behaviour: Behaviour,
        concurrency: Arc<Concurrency>,
        commands: std::sync::Mutex<Vec<String>>,
        cancels: AtomicUsize,
    }

    impl FakeHost {
        fn new(behaviour: Behaviour, concurrency: Arc<Concurrency>) -> Arc<Self> {
            Arc::new(Self {
                behaviour,
                concurrency,
                commands: std::sync::Mutex::new(Vec::new()),
                cancels: AtomicUsize::new(0),
            })
        }

        fn commands(&self) -> Vec<String> {
            self.commands.lock().expect("test mutex").clone()
        }

        fn cancels(&self) -> usize {
            self.cancels.load(Ordering::SeqCst)
        }
    }

    #[filar_transport::async_trait]
    impl CommandExecutor for FakeHost {
        async fn run(&self, command: &str) -> Result<CommandResult> {
            self.commands
                .lock()
                .expect("test mutex")
                .push(command.to_string());
            let _in_flight = self.concurrency.enter();
            match &self.behaviour {
                Behaviour::Answers(delay, stdout) => {
                    tokio::time::sleep(*delay).await;
                    Ok(answer(stdout))
                }
                Behaviour::Hangs => {
                    // Longer than any deadline a test sets.
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Ok(answer("too late"))
                }
                Behaviour::Fails(message) => Err(CoreError::Other((*message).to_string())),
                Behaviour::Unreachable => {
                    Err(CoreError::ConnectionLost("host is rebooting".into()))
                }
            }
        }

        async fn cancel(&self) -> Result<()> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    // ── Cancelling the operation (#437) ────────────────────────

    #[tokio::test(start_paused = true)]
    async fn cancelling_interrupts_running_hosts_skips_the_queue_and_keeps_answers() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(6);
        let mut op = FleetOperation::open(&group(2, 30), &targets);

        // host-1 answers quickly; the rest would never finish on their own.
        let hosts: Vec<_> = op
            .handles()
            .enumerate()
            .map(|(i, handle)| {
                let behaviour = if i == 0 {
                    Behaviour::Answers(Duration::from_millis(10), "5.15.0")
                } else {
                    Behaviour::Hangs
                };
                (handle, FakeHost::new(behaviour, concurrency.clone()))
            })
            .collect();
        let tasks = hosts
            .iter()
            .map(|(handle, host)| {
                HostTask::new(*handle, host.clone() as Arc<dyn CommandExecutor>, "uname -r")
            })
            .collect();

        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(1)).await;
            trigger.cancel();
        });
        let report = run_on_fleet_until(&mut op, tasks, &cancel).await.expect("valid tasks");

        // The answer that arrived before the cancellation is kept.
        assert!(matches!(report.run_for(hosts[0].0), Some(HostRun::Answered(r)) if r.stdout == "5.15.0"));
        // host-2 and host-3 were running: each got a Ctrl-C on the host.
        for (handle, host) in &hosts[1..3] {
            assert!(matches!(report.run_for(*handle), Some(HostRun::Cancelled)));
            assert_eq!(host.cancels(), 1, "a running host is interrupted, not abandoned");
        }
        // host-4..6 were queued: never started, never contacted.
        for (handle, host) in &hosts[3..] {
            assert!(matches!(report.run_for(*handle), Some(HostRun::Cancelled)));
            assert!(host.commands().is_empty(), "the queue does not start after a cancel");
        }
        assert_eq!(report.answered(), 1);
        assert_eq!(report.cancelled(), 5);
        assert_eq!(report.len(), 6, "every host has a row");
        assert_eq!(op.count_at(HostProgress::Done), 6);
        assert_eq!(concurrency.in_flight(), 0, "nothing is left running");
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_cancelled_before_it_starts_contacts_nobody() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(3);
        let mut op = FleetOperation::open(&group(3, 30), &targets);
        let hosts: Vec<_> = op
            .handles()
            .map(|h| (h, FakeHost::new(Behaviour::Answers(Duration::ZERO, "x"), concurrency.clone())))
            .collect();
        let tasks = hosts
            .iter()
            .map(|(h, host)| HostTask::new(*h, host.clone() as Arc<dyn CommandExecutor>, "uptime"))
            .collect();
        let cancel = CancellationToken::new();
        cancel.cancel();

        let report = run_on_fleet_until(&mut op, tasks, &cancel).await.expect("valid tasks");
        assert_eq!(report.cancelled(), 3);
        for (_, host) in &hosts {
            assert!(host.commands().is_empty());
        }
    }

    // ── The DoD cases ──────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn twelve_hosts_with_a_limit_of_three_never_exceed_three_connections() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(12);
        let mut op = FleetOperation::open(&group(3, 30), &targets);
        assert_eq!(op.len(), 12);

        let hosts: Vec<_> = op
            .handles()
            .map(|handle| {
                let host = FakeHost::new(
                    Behaviour::Answers(Duration::from_millis(50), "ok"),
                    concurrency.clone(),
                );
                (handle, host)
            })
            .collect();
        let tasks = hosts
            .iter()
            .map(|(handle, host)| {
                HostTask::new(*handle, host.clone() as Arc<dyn CommandExecutor>, "uname -r")
            })
            .collect();

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");

        assert_eq!(report.len(), 12);
        assert_eq!(report.answered(), 12);
        assert_eq!(
            concurrency.peak(),
            3,
            "no more than the group's max_parallel connections at once"
        );
        assert_eq!(op.count_at(HostProgress::Done), 12);
        for (_, host) in &hosts {
            assert_eq!(host.commands(), vec!["uname -r".to_string()]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_host_times_out_alone_while_the_others_finish() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(4);
        let mut op = FleetOperation::open(&group(4, 30), &targets);

        let wedged = FakeHost::new(Behaviour::Hangs, concurrency.clone());
        let mut tasks = Vec::new();
        let mut healthy = Vec::new();
        for (index, handle) in op.handles().enumerate() {
            if index == 1 {
                tasks.push(HostTask::new(
                    handle,
                    wedged.clone() as Arc<dyn CommandExecutor>,
                    "df -h",
                ));
                continue;
            }
            let host = FakeHost::new(
                Behaviour::Answers(Duration::from_millis(10), "fine"),
                concurrency.clone(),
            );
            tasks.push(HostTask::new(
                handle,
                host.clone() as Arc<dyn CommandExecutor>,
                "df -h",
            ));
            healthy.push((handle, host));
        }

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");

        assert_eq!(report.timed_out(), 1, "only the wedged host ran out of time");
        assert_eq!(report.answered(), 3, "the rest answered normally");
        assert_eq!(op.count_at(HostProgress::Done), 4, "including the wedged one");

        let wedged_handle = op.handles().nth(1).expect("second member");
        match report.run_for(wedged_handle).expect("a row for the wedged host") {
            HostRun::TimedOut(after) => {
                assert_eq!(*after, Duration::from_secs(30), "its own deadline, per host")
            }
            other => panic!("expected a timeout, got {other:?}"),
        }
        assert_eq!(
            wedged.cancels(),
            1,
            "a timed-out command is cancelled, not abandoned on the host"
        );
        for (_, host) in &healthy {
            assert_eq!(host.cancels(), 0, "a host that answered is not cancelled");
        }
        assert_eq!(
            concurrency.in_flight(),
            0,
            "a timed-out host's future is dropped, so its slot goes back to the limit"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_wedged_host_does_not_hold_a_slot_for_the_queue_behind_it() {
        // The leak this guards against: if a host that ran out of time
        // kept its slot, a group of twelve with three wedged machines
        // would finish at one third of its limit — or not at all.
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(6);
        let mut op = FleetOperation::open(&group(2, 30), &targets);

        let tasks = op
            .handles()
            .enumerate()
            .map(|(index, handle)| {
                let behaviour = if index < 2 {
                    Behaviour::Hangs
                } else {
                    Behaviour::Answers(Duration::from_millis(10), "ok")
                };
                HostTask::new(
                    handle,
                    FakeHost::new(behaviour, concurrency.clone()) as Arc<dyn CommandExecutor>,
                    "uptime",
                )
            })
            .collect();

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");

        assert_eq!(report.timed_out(), 2);
        assert_eq!(
            report.answered(),
            4,
            "the four behind the wedged pair still got asked"
        );
        assert_eq!(op.count_at(HostProgress::Done), 6);
        assert_eq!(concurrency.in_flight(), 0);
    }

    /// A host whose cancellation works the way the real executors' does:
    /// `run` selects on a `Notify`, `cancel` notifies it.
    ///
    /// Mirrors `LocalExecutor` (`crates/transport/src/local.rs`), whose
    /// `run` holds `cancel_notify.notified()` in a `tokio::select!` and
    /// whose `cancel` calls `notify_one`. The point of the mirror is the
    /// permit: `notify_one` with no waiter registered *stores* one, and the
    /// next `notified()` consumes it immediately.
    struct NotifyHost {
        cancel_notify: tokio::sync::Notify,
        work: Duration,
        runs: AtomicUsize,
        cancels: AtomicUsize,
    }

    impl NotifyHost {
        fn new(work: Duration) -> Arc<Self> {
            Arc::new(Self {
                cancel_notify: tokio::sync::Notify::new(),
                work,
                runs: AtomicUsize::new(0),
                cancels: AtomicUsize::new(0),
            })
        }
    }

    #[filar_transport::async_trait]
    impl CommandExecutor for NotifyHost {
        async fn run(&self, _command: &str) -> Result<CommandResult> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            tokio::select! {
                _ = tokio::time::sleep(self.work) => Ok(answer("ok")),
                _ = self.cancel_notify.notified() => {
                    Err(CoreError::Other("command cancelled by user".into()))
                }
            }
        }

        async fn cancel(&self) -> Result<()> {
            self.cancels.fetch_add(1, Ordering::SeqCst);
            self.cancel_notify.notify_one();
            Ok(())
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_timed_out_host_does_not_poison_its_next_command() {
        // The bug this pins down: if the `run` future is dropped when the
        // deadline fires, `cancel()` finds no waiter, the notification is
        // stored as a permit, and the *next* command on the same host
        // consumes it and reports itself cancelled though nothing
        // cancelled it. On a fleet host that is the next check — and, once
        // #429 lands, the retry of this very one.
        let targets = targets(1);
        let mut op = FleetOperation::open(&group(1, 30), &targets);
        let host = NotifyHost::new(Duration::from_secs(3600));

        let handle = op.handles().next().expect("member");
        let report = run_on_fleet(
            &mut op,
            vec![HostTask::new(
                handle,
                host.clone() as Arc<dyn CommandExecutor>,
                "uptime",
            )],
        )
        .await
        .expect("valid tasks");

        assert_eq!(report.timed_out(), 1);
        assert_eq!(host.cancels.load(Ordering::SeqCst), 1);

        // The same executor, asked again — the next check on that host.
        let second = host.run("uname -r").await;
        assert!(
            second.is_ok(),
            "a command after a timeout must not inherit the cancellation, got: {second:?}"
        );
        assert_eq!(host.runs.load(Ordering::SeqCst), 2);
    }

    // ── Everything else ────────────────────────────────────────

    #[tokio::test(start_paused = true)]
    async fn an_error_before_an_answer_keeps_its_kind() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(2);
        let mut op = FleetOperation::open(&group(2, 30), &targets);

        let gone = FakeHost::new(Behaviour::Unreachable, concurrency.clone());
        let broken = FakeHost::new(Behaviour::Fails("no such file"), concurrency.clone());
        let handles: Vec<_> = op.handles().collect();
        let tasks = vec![
            HostTask::new(handles[0], gone as Arc<dyn CommandExecutor>, "ss -H -n"),
            HostTask::new(handles[1], broken as Arc<dyn CommandExecutor>, "ss -H -n"),
        ];

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");

        assert_eq!(report.failed(), 2);
        assert_eq!(report.answered(), 0);
        // #428 divides these two; #429 retries only the first. The error
        // kind is what makes that possible, so it must survive the run.
        match report.run_for(handles[0]).expect("row") {
            HostRun::Failed(CoreError::ConnectionLost(_)) => {}
            other => panic!("expected no contact, got {other:?}"),
        }
        match report.run_for(handles[1]).expect("row") {
            HostRun::Failed(CoreError::Other(message)) => assert_eq!(message, "no such file"),
            other => panic!("expected an execution error, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_without_a_task_stays_pending() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(3);
        let mut op = FleetOperation::open(&group(3, 30), &targets);

        // The middle host has no credentials, so nobody asks it anything.
        let handles: Vec<_> = op.handles().collect();
        let tasks = vec![
            HostTask::new(
                handles[0],
                FakeHost::new(Behaviour::Answers(Duration::ZERO, "ok"), concurrency.clone())
                    as Arc<dyn CommandExecutor>,
                "uptime",
            ),
            HostTask::new(
                handles[2],
                FakeHost::new(Behaviour::Answers(Duration::ZERO, "ok"), concurrency.clone())
                    as Arc<dyn CommandExecutor>,
                "uptime",
            ),
        ];

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");

        assert_eq!(report.len(), 2, "the report has a row per task, not per member");
        assert!(report.run_for(handles[1]).is_none());
        assert_eq!(op.count_at(HostProgress::Done), 2);
        assert_eq!(
            op.count_at(HostProgress::Pending),
            1,
            "a host nobody asked is not silently Done"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_empty_operation_runs_nothing() {
        let mut op = FleetOperation::open(&group(3, 30), &[]);

        let report = run_on_fleet(&mut op, Vec::new()).await.expect("no tasks");

        assert!(report.is_empty());
        assert_eq!(report.len(), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn a_foreign_or_duplicated_handle_is_refused_before_anything_runs() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(2);
        let mut mine = FleetOperation::open(&group(2, 30), &targets);
        let theirs = FleetOperation::open(&group(2, 30), &targets);

        let host = FakeHost::new(Behaviour::Answers(Duration::ZERO, "ok"), concurrency.clone());
        let foreign = vec![HostTask::new(
            theirs.handles().next().expect("member"),
            host.clone() as Arc<dyn CommandExecutor>,
            "uptime",
        )];
        let error = run_on_fleet(&mut mine, foreign)
            .await
            .expect_err("a handle from another operation must not be run");
        assert!(
            error.to_string().contains("not in operation"),
            "the error must name the problem, got: {error}"
        );

        let handle = mine.handles().next().expect("member");
        let duplicated = vec![
            HostTask::new(handle, host.clone() as Arc<dyn CommandExecutor>, "uptime"),
            HostTask::new(handle, host.clone() as Arc<dyn CommandExecutor>, "uptime"),
        ];
        let error = run_on_fleet(&mut mine, duplicated)
            .await
            .expect_err("one host cannot hold two rows of the same report");
        assert!(
            error.to_string().contains("two tasks for the same host"),
            "the error must name the problem, got: {error}"
        );

        assert!(
            host.commands().is_empty(),
            "a refused task list must not have reached a host"
        );
        assert_eq!(
            mine.count_at(HostProgress::Pending),
            2,
            "and must not have moved any progress"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_zero_limit_runs_one_at_a_time_instead_of_stalling() {
        // `HostGroup::validate` rejects a zero, but the field is public and
        // a hand-built group can carry one. One at a time is the honest
        // reading; a literal zero would launch nothing at all.
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(3);
        let mut op = FleetOperation::open(&group(0, 30), &targets);

        let tasks = op
            .handles()
            .map(|handle| {
                HostTask::new(
                    handle,
                    FakeHost::new(
                        Behaviour::Answers(Duration::from_millis(5), "ok"),
                        concurrency.clone(),
                    ) as Arc<dyn CommandExecutor>,
                    "uptime",
                )
            })
            .collect();

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");

        assert_eq!(report.answered(), 3);
        assert_eq!(concurrency.peak(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn rows_keep_task_order_not_answer_order() {
        let concurrency = Arc::new(Concurrency::default());
        let targets = targets(3);
        let mut op = FleetOperation::open(&group(3, 30), &targets);

        // The first task is the slowest, so answer order is the reverse of
        // task order.
        let delays = [80u64, 40, 10];
        let handles: Vec<_> = op.handles().collect();
        let tasks = handles
            .iter()
            .zip(delays)
            .map(|(handle, delay)| {
                HostTask::new(
                    *handle,
                    FakeHost::new(
                        Behaviour::Answers(Duration::from_millis(delay), "ok"),
                        concurrency.clone(),
                    ) as Arc<dyn CommandExecutor>,
                    format!("sleep {delay}"),
                )
            })
            .collect();

        let report = run_on_fleet(&mut op, tasks).await.expect("valid tasks");

        let order: Vec<HostHandle> = report
            .outcomes()
            .iter()
            .map(HostOutcome::handle)
            .collect();
        assert_eq!(order, handles);
    }
}
