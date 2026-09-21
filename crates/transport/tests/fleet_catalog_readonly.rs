//! The seam between the fleet-check catalog (#424) and the read-only gate
//! (#419).
//!
//! The catalog is a hand-editable file: anyone who can write it can declare
//! any command at all. The claim that this does not widen the attack
//! surface rests entirely on `ReadOnlyExecutor` refusing whatever the
//! allowlist does not cover, *before* anything reaches a host. That claim
//! belongs to neither crate alone — `filar-core` cannot see the gate and
//! `filar-transport` does not own the catalog — so it is tested here, where
//! both are in scope.
//!
//! Two directions, both necessary:
//! - every built-in check is *runnable* under the gate (a shipped check the
//!   gate refuses would be dead weight);
//! - a user check declaring a write is *refused* by the gate, with the inner
//!   executor never touched.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use filar_core::fleet_checks::FleetCheckCatalog;
use filar_core::Result;
use filar_transport::readonly::check_read_only;
use filar_transport::{CommandExecutor, CommandResult, ReadOnlyExecutor};

/// Inner executor that records calls instead of running anything.
///
/// A real executor would have to actually run a write for this test to be
/// meaningful, which is exactly what must not happen — so "was it forwarded"
/// is the observable, not "what did it do".
#[derive(Default)]
struct RecordingExecutor {
    calls: AtomicUsize,
}

impl RecordingExecutor {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[filar_transport::async_trait]
impl CommandExecutor for RecordingExecutor {
    async fn run(&self, _command: &str) -> Result<CommandResult> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CommandResult {
            stdout: String::new(),
            stderr: String::new(),
            exit_code: Some(0),
            duration: Duration::from_millis(0),
            cwd: None,
        })
    }

    async fn cancel(&self) -> Result<()> {
        Ok(())
    }
}

fn gated() -> (ReadOnlyExecutor, Arc<RecordingExecutor>) {
    let inner = Arc::new(RecordingExecutor::default());
    let exec = ReadOnlyExecutor::new(inner.clone());
    (exec, inner)
}

/// Every command the shipped catalog declares must survive the gate — as a
/// pure predicate first, so a failure names the command rather than an
/// executor error.
#[test]
fn every_builtin_check_passes_the_read_only_gate() {
    let catalog = FleetCheckCatalog::builtin();
    assert!(!catalog.is_empty(), "built-in catalog must not be empty");

    for check in catalog.checks() {
        if let Err(reason) = check_read_only(check.command()) {
            panic!(
                "built-in check '{}' declares a command the read-only gate refuses: \
                 {} — command was: {}",
                check.name(),
                reason,
                check.command()
            );
        }
    }
}

/// The same set, through the real executor rather than the predicate.
#[tokio::test]
async fn every_builtin_check_is_forwarded_by_the_executor() {
    let catalog = FleetCheckCatalog::builtin();
    let (exec, inner) = gated();

    for check in catalog.checks() {
        exec.run(check.command())
            .await
            .unwrap_or_else(|e| panic!("built-in check '{}' was refused: {e}", check.name()));
    }
    assert_eq!(
        inner.calls(),
        catalog.checks().len(),
        "every built-in check should have reached the inner executor"
    );
}

/// The DoD case: a check *can* declare a forbidden command — the catalog is
/// a declaration, not a permission — and the transport refuses it before the
/// host sees anything.
#[tokio::test]
async fn a_check_declaring_a_forbidden_command_is_refused_by_the_transport() {
    let dir = std::env::temp_dir().join(format!(
        "filar_fleet_readonly_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fleet_checks.toml");
    std::fs::write(
        &path,
        r#"
[[check]]
name = "wipe-logs"
description = "A write, declared in a perfectly valid catalog entry"
command = "rm -rf /var/log"

[[check]]
name = "smuggled"
description = "A reader with a write smuggled in behind a separator"
command = "cat /etc/hostname; rm -rf /var/log"
"#,
    )
    .unwrap();

    let catalog = FleetCheckCatalog::load(Some(&path));
    let _ = std::fs::remove_dir_all(&dir);

    // The catalog itself accepts both: it validates shape, not policy.
    assert!(
        catalog.rejected().is_empty(),
        "catalog should accept the entries: {:?}",
        catalog.rejected()
    );

    let (exec, inner) = gated();
    for name in ["wipe-logs", "smuggled"] {
        let check = catalog.get(name).expect("check missing from catalog");
        let error = exec
            .run(check.command())
            .await
            .expect_err("the read-only gate must refuse this command");
        let rendered = error.to_string();
        assert!(
            rendered.contains("read-only policy"),
            "refusal should name the policy, got: {rendered}"
        );
        assert!(
            rendered.contains("not sent to the host"),
            "refusal should say nothing was sent, got: {rendered}"
        );
    }
    assert_eq!(
        inner.calls(),
        0,
        "no forbidden command may reach the inner executor"
    );
}

/// A user check that stays inside the allowlist is forwarded like any
/// built-in — the gate constrains what a catalog can do, it does not make
/// user checks second-class.
#[tokio::test]
async fn a_read_only_user_check_is_forwarded() {
    let dir = std::env::temp_dir().join(format!(
        "filar_fleet_readonly_ok_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("fleet_checks.toml");
    std::fs::write(
        &path,
        r#"
[[check]]
name = "sshd-config"
description = "Effective sshd configuration"
command = "cat /etc/ssh/sshd_config"
"#,
    )
    .unwrap();

    let catalog = FleetCheckCatalog::load(Some(&path));
    let _ = std::fs::remove_dir_all(&dir);

    let check = catalog.get("sshd-config").expect("user check missing");
    let (exec, inner) = gated();
    exec.run(check.command()).await.expect("should be forwarded");
    assert_eq!(inner.calls(), 1);
}
