//! The "operation → hosts" model behind the side panel (#431).
//!
//! Every long-running thing the panel shows is an [`Operation`] spanning one
//! or more hosts. A background job on one tab is the degenerate case — an
//! operation of size one — so a fleet operation later (#432, #438) is the
//! same shape with more hosts, not a second list to reconcile with this one.
//!
//! The model is plain data built from snapshots; nothing here talks to a
//! host. Where the data comes from (the agent's background-job registry
//! today) is the caller's business — see [`from_background_jobs`].

use filar_agent::background::{JobSnapshot, JobState};

use crate::app::SessionId;

/// State of one host within an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOpState {
    /// Still running (for a remote job: as of the agent's last poll).
    Running,
    /// Finished with exit code 0.
    Done,
    /// Finished with a non-zero exit code.
    Failed,
    /// Cancelled before it finished.
    Cancelled,
}

impl HostOpState {
    /// Short word for the state, shown next to the glyph so no state is
    /// told apart by colour alone.
    pub fn label(self) -> &'static str {
        match self {
            HostOpState::Running => "running",
            HostOpState::Done => "done",
            HostOpState::Failed => "failed",
            HostOpState::Cancelled => "cancelled",
        }
    }
}

/// One host's part in an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpHost {
    /// Host name as the user knows it (tab / target name).
    pub name: String,
    /// Where this host stands.
    pub state: HostOpState,
    /// Exit code, once finished.
    pub exit_code: Option<i32>,
    /// Tail of this host's output.
    pub tail: String,
    /// `true` when the state is only as fresh as the agent's last poll.
    pub stale: bool,
}

/// A unit of work shown in the panel: a label and the hosts it runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Operation {
    /// Tab the operation belongs to.
    pub session: SessionId,
    /// Operation id within its tab (`job-N` for background jobs).
    pub id: String,
    /// What runs (the command).
    pub label: String,
    /// Participating hosts; never empty for a background job.
    pub hosts: Vec<OpHost>,
}

impl Operation {
    /// The operation's overall state: running while any host runs, then
    /// failed if any host failed, cancelled if any was cancelled, else done.
    pub fn state(&self) -> HostOpState {
        let any = |st: HostOpState| self.hosts.iter().any(|h| h.state == st);
        if any(HostOpState::Running) {
            HostOpState::Running
        } else if any(HostOpState::Failed) {
            HostOpState::Failed
        } else if any(HostOpState::Cancelled) {
            HostOpState::Cancelled
        } else {
            HostOpState::Done
        }
    }
}

/// Counts of operations by state — the status-bar counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpCounts {
    pub running: usize,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
}

impl OpCounts {
    /// Count `ops` by their overall state.
    pub fn of(ops: &[Operation]) -> Self {
        let mut c = Self::default();
        for op in ops {
            match op.state() {
                HostOpState::Running => c.running += 1,
                HostOpState::Done => c.done += 1,
                HostOpState::Failed => c.failed += 1,
                HostOpState::Cancelled => c.cancelled += 1,
            }
        }
        c
    }

    /// Total number of operations.
    pub fn total(&self) -> usize {
        self.running + self.done + self.failed + self.cancelled
    }
}

/// Build one single-host operation per background job of a tab.
pub fn from_background_jobs(
    session: SessionId,
    host: &str,
    jobs: Vec<JobSnapshot>,
) -> Vec<Operation> {
    jobs.into_iter()
        .map(|job| {
            let (state, exit_code) = match job.state {
                JobState::Running => (HostOpState::Running, None),
                JobState::Done { exit_code } => (HostOpState::Done, Some(exit_code)),
                JobState::Failed { exit_code } => (HostOpState::Failed, Some(exit_code)),
                JobState::Cancelled => (HostOpState::Cancelled, None),
            };
            Operation {
                session,
                id: job.job_id,
                label: job.command,
                hosts: vec![OpHost {
                    name: host.to_string(),
                    state,
                    exit_code,
                    tail: job.output_tail,
                    stale: job.remote && state == HostOpState::Running,
                }],
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(id: &str, state: JobState, remote: bool) -> JobSnapshot {
        JobSnapshot {
            job_id: id.into(),
            command: format!("cmd {id}"),
            state,
            output_tail: "out".into(),
            remote,
        }
    }

    fn host(state: HostOpState) -> OpHost {
        OpHost { name: "h".into(), state, exit_code: None, tail: String::new(), stale: false }
    }

    #[test]
    fn a_background_job_is_an_operation_of_one_host() {
        let ops = from_background_jobs(
            SessionId(7),
            "web-01",
            vec![
                snap("job-1", JobState::Running, true),
                snap("job-2", JobState::Failed { exit_code: 3 }, false),
            ],
        );
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].hosts.len(), 1);
        assert_eq!(ops[0].hosts[0].name, "web-01");
        assert_eq!(ops[0].label, "cmd job-1");
        assert!(ops[0].hosts[0].stale, "running remote job is only as fresh as the last poll");
        assert_eq!(ops[1].state(), HostOpState::Failed);
        assert_eq!(ops[1].hosts[0].exit_code, Some(3));
        assert!(!ops[1].hosts[0].stale);
    }

    #[test]
    fn finished_remote_job_is_not_stale() {
        let ops = from_background_jobs(
            SessionId(1),
            "db",
            vec![snap("job-1", JobState::Done { exit_code: 0 }, true)],
        );
        assert!(!ops[0].hosts[0].stale);
    }

    #[test]
    fn operation_state_aggregates_hosts() {
        let op = |hosts: Vec<OpHost>| Operation {
            session: SessionId(1),
            id: "op".into(),
            label: "x".into(),
            hosts,
        };
        use HostOpState::*;
        assert_eq!(op(vec![host(Done), host(Running)]).state(), Running);
        assert_eq!(op(vec![host(Done), host(Failed), host(Cancelled)]).state(), Failed);
        assert_eq!(op(vec![host(Done), host(Cancelled)]).state(), Cancelled);
        assert_eq!(op(vec![host(Done), host(Done)]).state(), Done);
    }

    #[test]
    fn counts_by_overall_state() {
        let ops = from_background_jobs(
            SessionId(1),
            "h",
            vec![
                snap("job-1", JobState::Running, false),
                snap("job-2", JobState::Running, false),
                snap("job-3", JobState::Done { exit_code: 0 }, false),
                snap("job-4", JobState::Cancelled, false),
            ],
        );
        let c = OpCounts::of(&ops);
        assert_eq!((c.running, c.done, c.failed, c.cancelled), (2, 1, 0, 1));
        assert_eq!(c.total(), 4);
    }
}
