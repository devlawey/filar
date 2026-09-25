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
//! failure is not cached. A cached connection that is lost mid-command is
//! dropped the same way, so the next command reconnects instead of reusing
//! a dead session.
//!
//! # Credentials
//!
//! Each host connects with its own credentials, resolved when the fleet
//! opened ([`FleetCredentials`], #436). A host without them has no task:
//! it is never contacted, the summary marks it skipped, and the radius
//! does not name it. The connector is handed the host's own target and
//! nothing else, so one host's secret cannot reach another.
//!
//! Cancelling the operation on the hosts is #437. [`cancel`](CommandExecutor::cancel)
//! here forwards Ctrl-C to every connected host, no more.

use std::sync::Arc;

use futures::future::BoxFuture;
use futures::stream::{self, StreamExt};
use tokio::sync::Mutex;

use filar_core::config::SshTarget;
use filar_core::error::{CoreError, Result};
use filar_core::fleet_checks::FleetCheck;
use filar_core::fleet_op::{FleetOperation, OperationId};
use filar_transport::readonly::check_read_only;
use filar_transport::{is_connection_lost, CommandExecutor, CommandResult};

use crate::fleet_creds::FleetCredentials;
use crate::fleet_fold::fold;
use crate::fleet_result::{NotAsked, OperationResult};
use crate::fleet_run::{run_on_fleet, HostRun, HostTask};
use crate::preprocess::PreprocessorRegistry;

/// Opens a connection to one fleet host.
///
/// The executor it returns is used as is, so it must already carry the
/// read-only wrapper (`filar_transport::readonly::ReadOnlyExecutor`).
pub type HostConnector =
    Arc<dyn Fn(SshTarget) -> BoxFuture<'static, Result<Arc<dyn CommandExecutor>>> + Send + Sync>;

/// One `run` = one read-only question to every host of a fleet.
pub struct FleetExecutor {
    /// Id of the operation this executor was built from — the one whose
    /// composition the UI shows as the radius.
    source: OperationId,
    /// The composition frozen when the fleet was entered.
    fleet: FleetOperation,
    /// Who takes part and with which target, resolved at fleet entry.
    credentials: FleetCredentials,
    connect: HostConnector,
    /// Live connections by member index. Held only to read or fill slots,
    /// never across a command, so [`cancel`](CommandExecutor::cancel) is
    /// never blocked by a running fan-out.
    hosts: Mutex<Vec<Option<Arc<dyn CommandExecutor>>>>,
}

impl FleetExecutor {
    /// An executor over `fleet`'s hosts, connecting each through `connect`
    /// with the target `credentials` resolved for it.
    ///
    /// Refuses credentials resolved for another operation: pairing them by
    /// position would hand one fleet's secrets to another fleet's hosts.
    pub fn new(
        fleet: &FleetOperation,
        credentials: FleetCredentials,
        connect: HostConnector,
    ) -> Result<Self> {
        if credentials.operation() != fleet.id() {
            return Err(CoreError::Other(format!(
                "fleet credentials are for operation {}, not {}",
                credentials.operation(),
                fleet.id()
            )));
        }
        Ok(Self {
            source: fleet.id(),
            hosts: Mutex::new(vec![None; fleet.len()]),
            fleet: fleet.reopen(),
            credentials,
            connect,
        })
    }

    /// The operation this executor was built from. A caller holding one
    /// executor per fleet checks this against the fleet it is about to run
    /// in, so a command can never go to a composition other than the one
    /// on screen.
    pub fn built_from(&self) -> OperationId {
        self.source
    }

    /// Names of the hosts every command goes to — the radius the gate shows.
    /// A host without credentials is not in it: nothing is sent there.
    pub fn radius(&self) -> Vec<String> {
        self.credentials
            .participants(&self.fleet)
            .into_iter()
            .map(str::to_string)
            .collect()
    }

    /// An executor per member with credentials, `None` for the rest: cached
    /// ones, fresh connections for the others. A host that cannot be
    /// connected gets a stand-in that reports the connection error, so it
    /// lands in the summary as "no contact".
    async fn executors(&self) -> Vec<Option<Arc<dyn CommandExecutor>>> {
        let cached = self.hosts.lock().await.clone();
        let limit = self.fleet.max_parallel().max(1) as usize;
        let deadline = self.fleet.per_host_timeout();

        let fresh: Vec<(usize, Result<Arc<dyn CommandExecutor>>)> = stream::iter(
            cached
                .iter()
                .enumerate()
                .filter(|(_, slot)| slot.is_none())
                .map(|(i, _)| i)
                .filter_map(|i| self.credentials.ready(i).map(|t| (i, t.clone())))
                .collect::<Vec<_>>(),
        )
        .map(|(i, target)| {
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
        let mut out = cached;
        for (i, conn) in fresh {
            match conn {
                Ok(exec) => {
                    slots[i] = Some(Arc::clone(&exec));
                    out[i] = Some(exec);
                }
                Err(e) => out[i] = Some(Arc::new(Unreachable(e.to_string()))),
            }
        }
        out
    }
}

impl FleetExecutor {
    /// Drop the cached connection of every host whose command failed with
    /// a lost connection, so the next command connects afresh through the
    /// [`HostConnector`] instead of reusing a dead session. Only the exact
    /// executor this run used is dropped — a slot refilled meanwhile keeps
    /// its newer connection.
    async fn evict_lost(
        &self,
        handles: &[filar_core::fleet_op::HostHandle],
        used: &[Option<Arc<dyn CommandExecutor>>],
        report: &crate::fleet_run::FleetRunReport,
    ) {
        let mut slots = self.hosts.lock().await;
        for outcome in report.outcomes() {
            if !matches!(outcome.run(), HostRun::Failed(e) if is_connection_lost(e)) {
                continue;
            }
            let Some(i) = handles.iter().position(|h| *h == outcome.handle()) else {
                continue;
            };
            let same = match (&slots[i], &used[i]) {
                (Some(cached), Some(used)) => Arc::ptr_eq(cached, used),
                _ => false,
            };
            if same {
                slots[i] = None;
            }
        }
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
        let handles: Vec<_> = op.handles().collect();
        let mut tasks = Vec::new();
        let mut not_asked = Vec::new();
        for (handle, exec) in handles.iter().zip(executors.iter()) {
            match exec {
                Some(exec) => tasks.push(HostTask::new(*handle, Arc::clone(exec), command)),
                None => not_asked.push((*handle, NotAsked::Skipped)),
            }
        }

        let report = run_on_fleet(&mut op, tasks.clone()).await?;
        self.evict_lost(&handles, &executors, &report).await;
        let mut result = OperationResult::build(&op, &report, &not_asked)?;
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
    use filar_core::secrets::StaticSecretProvider;

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
                auth: SshAuth::Key { path: None },
                host_key_policy: Default::default(),
                tags: vec!["web".into()],
            })
            .collect();
        FleetOperation::open(&group, &targets)
    }

    /// An executor over `op` where every host has credentials (key auth).
    fn executor(op: &FleetOperation, connect: HostConnector) -> FleetExecutor {
        let creds = FleetCredentials::resolve(op, &StaticSecretProvider::new());
        FleetExecutor::new(op, creds, connect).expect("credentials match the fleet")
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
        let exec = executor(
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
        let exec = executor(
            &fleet(&["web-1", "web-2"]),
            connector(&["web-2"], &[], attempts.clone(), runs.clone()),
        );
        let out = exec.run("uptime").await.expect("fleet run");
        assert!(out.stdout.contains("1 of 2 hosts answered, 1 did not answer"), "{}", out.stdout);
        assert!(out.stdout.contains("no contact"), "{}", out.stdout);

        exec.run("uptime").await.expect("second run");
        assert_eq!(attempts.load(Ordering::SeqCst), 3, "the failed host is tried again");
    }

    /// Answers once, then behaves like a dropped SSH session.
    struct DropsAfterFirst {
        runs: Arc<AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl CommandExecutor for DropsAfterFirst {
        async fn run(&self, _command: &str) -> Result<CommandResult> {
            if self.runs.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(CommandResult {
                    stdout: "ok".into(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: std::time::Duration::ZERO,
                    cwd: None,
                })
            } else {
                Err(CoreError::ConnectionLost("session closed".into()))
            }
        }
        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn a_dropped_connection_is_reopened_on_the_next_command() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&attempts);
        let connect: HostConnector = Arc::new(move |_t: SshTarget| {
            counter.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Ok(Arc::new(DropsAfterFirst { runs: Arc::new(AtomicUsize::new(0)) })
                    as Arc<dyn CommandExecutor>)
            })
        });
        let exec = executor(&fleet(&["web-1"]), connect);

        exec.run("uptime").await.expect("first run answers");
        let lost = exec.run("uptime").await.expect("second run reports");
        assert!(lost.stdout.contains("no contact"), "{}", lost.stdout);
        assert_eq!(attempts.load(Ordering::SeqCst), 1, "the dead session is still cached here");

        let back = exec.run("uptime").await.expect("third run");
        assert_eq!(attempts.load(Ordering::SeqCst), 2, "the lost host was reconnected");
        assert!(back.stdout.contains("1 of 1 hosts answered"), "{}", back.stdout);
    }

    #[tokio::test]
    async fn a_write_reaches_no_host() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let exec = executor(
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

    fn password_target(name: &str) -> SshTarget {
        SshTarget {
            name: name.into(),
            host: format!("{name}.example"),
            port: 22,
            user: "admin".into(),
            auth: SshAuth::Password { password: None },
            host_key_policy: Default::default(),
            tags: vec!["web".into()],
        }
    }

    fn password_fleet(names: &[&str]) -> FleetOperation {
        let group = HostGroup {
            name: "web".into(),
            match_tags: vec!["web".into()],
            max_parallel: 4,
            per_host_timeout_secs: 5,
            ..HostGroup::default()
        };
        let targets: Vec<SshTarget> = names.iter().map(|n| password_target(n)).collect();
        FleetOperation::open(&group, &targets)
    }

    /// `(host, password)` of every connection a connector opened.
    type Seen = Arc<std::sync::Mutex<Vec<(String, Option<String>)>>>;

    /// Records `(host, password)` for every connection it opens.
    fn recording_connector(seen: Seen) -> HostConnector {
        Arc::new(move |target: SshTarget| {
            let password = match &target.auth {
                SshAuth::Password { password } => password.clone(),
                _ => None,
            };
            seen.lock().expect("lock").push((target.name.clone(), password));
            Box::pin(async {
                Ok(Arc::new(Fixed { stdout: "same", runs: Arc::new(AtomicUsize::new(0)) })
                    as Arc<dyn CommandExecutor>)
            })
        })
    }

    #[tokio::test]
    async fn a_host_without_credentials_is_skipped_and_the_fleet_runs_on_the_rest() {
        let op = password_fleet(&["web-1", "web-2", "web-3"]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_target:web-1", "pw-1");
        store.insert("ssh_target:web-3", "pw-3");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let exec = FleetExecutor::new(
            &op,
            FleetCredentials::resolve(&op, &store),
            recording_connector(Arc::clone(&seen)),
        )
        .expect("executor");
        assert_eq!(exec.radius(), ["web-1", "web-3"], "the radius names only who is asked");

        let out = exec.run("uname -r").await.expect("fleet run");
        assert!(out.stdout.contains("2 of 3 hosts answered"), "{}", out.stdout);
        assert!(out.stdout.contains("1 skipped"), "{}", out.stdout);
        assert_eq!(out.exit_code, Some(0), "a skipped host does not fail the fleet");
        let contacted: Vec<String> = seen.lock().expect("lock").iter().map(|(n, _)| n.clone()).collect();
        assert!(!contacted.contains(&"web-2".to_string()), "web-2 was never contacted");
    }

    #[tokio::test]
    async fn each_host_is_connected_with_its_own_secret_only() {
        let op = password_fleet(&["web-1", "web-2"]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_target:web-1", "secret-of-web-1");
        store.insert("ssh_target:web-2", "secret-of-web-2");
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let exec = FleetExecutor::new(
            &op,
            FleetCredentials::resolve(&op, &store),
            recording_connector(Arc::clone(&seen)),
        )
        .expect("executor");

        let out = exec.run("uptime").await.expect("fleet run");
        let mut seen = seen.lock().expect("lock").clone();
        seen.sort();
        assert_eq!(
            seen,
            [
                ("web-1".to_string(), Some("secret-of-web-1".to_string())),
                ("web-2".to_string(), Some("secret-of-web-2".to_string())),
            ]
        );
        // What goes back to the agent (and so to the transcript and the
        // model's context) carries no secret.
        assert!(!out.stdout.contains("secret-of"), "{}", out.stdout);
        assert!(!out.stderr.contains("secret-of"), "{}", out.stderr);
    }

    #[test]
    fn credentials_of_another_fleet_are_refused() {
        let first = password_fleet(&["web-1"]);
        let second = password_fleet(&["web-1"]);
        let creds = FleetCredentials::resolve(&first, &StaticSecretProvider::new());
        let attempts = Arc::new(AtomicUsize::new(0));
        let runs = Arc::new(AtomicUsize::new(0));
        let refused = FleetExecutor::new(&second, creds, connector(&[], &[], attempts, runs));
        assert!(refused.is_err());
    }
}
