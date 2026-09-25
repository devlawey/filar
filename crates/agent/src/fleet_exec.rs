//! A [`CommandExecutor`] for a whole fleet (#435).
//!
//! The fleet agent works over the same trait as any other agent — one
//! `run(command)` — so nothing about SSH or about groups leaks into the
//! agent loop (AGENTS.md, invariant 4). Behind that one call this executor
//! does what the fleet layer already knows how to do:
//!
//! 1. refuses anything the read-only allowlist rejects, before any host is
//!    contacted (the per-host `ReadOnlyExecutor` refuses it again — two
//!    independent layers, #419);
//! 2. opens a fresh operation over the composition frozen when the fleet
//!    was entered ([`FleetOperation::reopen`], #426) — never re-resolving
//!    the group's tags;
//! 3. fans the command out under the group's `max_parallel` and per-host
//!    deadline ([`run_on_fleet`], #427);
//! 4. classifies every host, silent ones included ([`OperationResult`],
//!    #428), and folds the answers into one difference table ([`fold`],
//!    #430).
//!
//! What comes back is the summary headline and the fold — **never host
//! output**. The fold of an ad-hoc command compares whole outputs by
//! digest ([`FleetCheck::ad_hoc`]), so the model learns who agrees with
//! whom and nothing a host printed (#430's containment argument).
//!
//! # Connections
//!
//! A host is connected on first use and the connection is kept for the
//! next command. How to connect is the caller's [`HostConnector`] — this
//! crate does not open SSH sessions itself, and the connector is where the
//! read-only wrapper goes on. A host that cannot be reached comes out as
//! "no contact" in the summary and is tried again on the next command; the
//! failure is not cached.
//!
//! Credentials policy (a host without them drops out) is #436; cancelling
//! the operation on the hosts is #437. [`cancel`](CommandExecutor::cancel)
//! here forwards Ctrl-C to every connected host, no more.

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use tokio::sync::Mutex;

use filar_core::config::SshTarget;
use filar_core::error::{CoreError, Result};
use filar_core::fleet_checks::FleetCheck;
use filar_core::fleet_op::FleetOperation;
use filar_transport::readonly::check_read_only;
use filar_transport::{CommandExecutor, CommandResult};

use crate::fleet_fold::fold;
use crate::fleet_result::OperationResult;
use crate::fleet_run::{run_on_fleet, HostTask};
use crate::preprocess::PreprocessorRegistry;

/// Opens a connection to one fleet host.
///
/// The executor it returns is used as is, so it must already carry the
/// read-only wrapper (`filar_transport::readonly::ReadOnlyExecutor`).
pub type HostConnector =
    Arc<dyn Fn(SshTarget) -> BoxFuture<'static, Result<Arc<dyn CommandExecutor>>> + Send + Sync>;

/// One `run` = one read-only question to every host of a fleet.
pub struct FleetExecutor {
    /// The composition frozen when the fleet was entered.
    fleet: FleetOperation,
    connect: HostConnector,
    /// Live connections by member index. Held only to read or fill slots,
    /// never across a command, so [`cancel`](CommandExecutor::cancel) is
    /// never blocked by a running fan-out.
    hosts: Mutex<Vec<Option<Arc<dyn CommandExecutor>>>>,
}

impl FleetExecutor {
    /// An executor over `fleet`'s hosts, connecting each through `connect`.
    pub fn new(fleet: &FleetOperation, connect: HostConnector) -> Self {
        Self {
            hosts: Mutex::new(vec![None; fleet.len()]),
            fleet: fleet.reopen(),
            connect,
        }
    }

    /// Names of the hosts every command goes to — the radius the gate shows.
    pub fn radius(&self) -> Vec<String> {
        self.fleet.members().iter().map(|m| m.name().to_string()).collect()
    }

    /// An executor per member: cached ones, fresh connections for the rest.
    /// A host that cannot be connected gets a stand-in that reports the
    /// connection error, so it lands in the summary as "no contact".
    async fn executors(&self) -> Vec<Arc<dyn CommandExecutor>> {
        let cached = self.hosts.lock().await.clone();
        let limit = self.fleet.max_parallel().max(1) as usize;
        let deadline = self.fleet.per_host_timeout();
        let members = self.fleet.members();

        let fresh: Vec<(usize, Result<Arc<dyn CommandExecutor>>)> = stream::iter(
            cached
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.is_none())
                .map(|(i, _)| i)
                .collect::<Vec<_>>(),
        )
        .map(|i| {
            let target = members[i].target().clone();
            let connect = Arc::clone(&self.connect);
            async move {
                let name = target.name.clone();
                let conn = match tokio::time::timeout(deadline, connect(target)).await {
                    Ok(result) => result,
                    Err(_) => Err(CoreError::ConnectionLost(format!(
                        "{name}: connecting took longer than {}s",
                        deadline.as_secs()
                    ))),
                };
                (i, conn)
            }
        })
        .buffer_unordered(limit)
        .collect()
        .await;

        let mut slots = self.hosts.lock().await;
        let mut out: Vec<Arc<dyn CommandExecutor>> = cached
            .into_iter()
            .map(|slot| slot.unwrap_or_else(|| Arc::new(Unreachable(String::new()))))
            .collect();
        for (i, conn) in fresh {
            match conn {
                Ok(exec) => {
                    slots[i] = Some(Arc::clone(&exec));
                    out[i] = exec;
                }
                Err(e) => out[i] = Arc::new(Unreachable(e.to_string())),
            }
        }
        out
    }
}

#[async_trait::async_trait]
impl CommandExecutor for FleetExecutor {
    async fn run(&self, command: &str) -> Result<CommandResult> {
        check_read_only(command).map_err(|reason| {
            CoreError::Other(format!(
                "read-only policy: {reason} — command not sent to any host of the fleet"
            ))
        })?;
        let check = FleetCheck::ad_hoc(command).map_err(CoreError::Other)?;

        let started = std::time::Instant::now();
        let mut op = self.fleet.reopen();
        let executors = self.executors().await;
        let tasks: Vec<HostTask> = op
            .handles()
            .zip(executors)
            .map(|(handle, exec)| HostTask::new(handle, exec, command))
            .collect();

        let report = run_on_fleet(&mut op, tasks.clone()).await?;
        let mut result = OperationResult::build(&op, &report, &[])?;
        let registry = PreprocessorRegistry::with_builtins();
        let table = fold(&check, &tasks, &report, &mut result, &registry)?;
        let summary = result.summary();

        Ok(CommandResult {
            stdout: format!("{}\n{}", summary.headline(), table),
            stderr: String::new(),
            exit_code: Some(if summary.is_failed() { 1 } else { 0 }),
            duration: started.elapsed(),
            // Every host has its own working directory; the fleet has none.
            cwd: None,
        })
    }

    async fn cancel(&self) -> Result<()> {
        let connected: Vec<_> = self.hosts.lock().await.iter().flatten().cloned().collect();
        for exec in connected {
            let _ = exec.cancel().await;
        }
        Ok(())
    }
}

/// Stand-in for a host that could not be connected: every command fails
/// with the connection error, which the result reads as "no contact".
struct Unreachable(String);

#[async_trait::async_trait]
impl CommandExecutor for Unreachable {
    async fn run(&self, _command: &str) -> Result<CommandResult> {
        Err(CoreError::ConnectionLost(self.0.clone()))
    }

    async fn cancel(&self) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use filar_core::config::{HostGroup, SshAuth};

    use super::*;

    struct Fixed {
        stdout: &'static str,
        runs: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl CommandExecutor for Fixed {
        async fn run(&self, _command: &str) -> Result<CommandResult> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(CommandResult {
                stdout: self.stdout.into(),
                stderr: String::new(),
                exit_code: Some(0),
                duration: std::time::Duration::ZERO,
                cwd: None,
            })
        }
        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    fn fleet(names: &[&str]) -> FleetOperation {
        let group = HostGroup {
            name: "web".into(),
            match_tags: vec!["web".into()],
            max_parallel: 2,
            per_host_timeout_secs: 5,
            ..HostGroup::default()
        };
        let targets: Vec<SshTarget> = names
            .iter()
            .map(|n| SshTarget {
                name: (*n).into(),
                host: format!("{n}.example"),
                port: 22,
                user: "admin".into(),
                auth: SshAuth::default(),
                host_key_policy: Default::default(),
                tags: vec!["web".into()],
            })
            .collect();
        FleetOperation::open(&group, &targets)
    }

    /// Connector: `down` hosts refuse, the rest answer `stdout`; counts
    /// connection attempts and runs.
    fn connector(
        down: &'static [&'static str],
        outputs: &'static [(&'static str, &'static str)],
        attempts: Arc<AtomicUsize>,
        runs: Arc<AtomicUsize>,
    ) -> HostConnector {
        Arc::new(move |target: SshTarget| {
            attempts.fetch_add(1, Ordering::SeqCst);
            let runs = Arc::clone(&runs);
            Box::pin(async move {
                if down.contains(&target.name.as_str()) {
                    return Err(CoreError::ConnectionLost(format!("{}: refused", target.name)));
                }
                let stdout = outputs
                    .iter()
                    .find(|(n, _)| *n == target.name)
                    .map(|(_, o)| *o)
                    .unwrap_or("same");
                Ok(Arc::new(Fixed { stdout, runs }) as Arc<dyn CommandExecutor>)
            })
        })
    }

    #[tokio::test]
    async fn one_run_asks_every_host_and_returns_the_fold_not_the_output() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let exec = FleetExecutor::new(
            &fleet(&["web-1", "web-2", "web-3"]),
            connector(&[], &[("web-3", "SECRET-LOOKING TEXT")], attempts.clone(), runs.clone()),
        );
        assert_eq!(exec.radius(), ["web-1", "web-2", "web-3"]);

        let out = exec.run("uname -r").await.expect("fleet run");
        assert_eq!(runs.load(Ordering::SeqCst), 3);
        assert!(out.stdout.contains("3 of 3 hosts answered"), "{}", out.stdout);
        assert!(out.stdout.contains("same on 2 (web-1, web-2)"), "{}", out.stdout);
        assert!(out.stdout.contains("web-3 differs"), "{}", out.stdout);
        assert!(!out.stdout.contains("SECRET-LOOKING"), "host output never reaches the model");
        assert_eq!(out.exit_code, Some(0));

        exec.run("uname -r").await.expect("second run");
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "connections are kept");
    }

    #[tokio::test]
    async fn an_unreachable_host_is_no_contact_and_is_retried_next_time() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let exec = FleetExecutor::new(
            &fleet(&["web-1", "web-2"]),
            connector(&["web-2"], &[], attempts.clone(), runs.clone()),
        );
        let out = exec.run("uptime").await.expect("fleet run");
        assert!(out.stdout.contains("1 of 2 hosts answered, 1 did not answer"), "{}", out.stdout);
        assert!(out.stdout.contains("no contact"), "{}", out.stdout);

        exec.run("uptime").await.expect("second run");
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "the failed host is tried again");
    }

    #[tokio::test]
    async fn a_write_reaches_no_host() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let exec = FleetExecutor::new(
            &fleet(&["web-1", "web-2"]),
            connector(&[], &[], attempts.clone(), runs.clone()),
        );
        for cmd in ["rm -rf /tmp/x", "uname -r && reboot", "cat $(echo /etc/passwd)"] {
            let err = exec.run(cmd).await.expect_err(cmd);
            assert!(err.to_string().contains("not sent to any host"), "{err}");
        }
        assert_eq!(attempts.load(Ordering::SeqCst), 0, "no host was even contacted");
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }
}
