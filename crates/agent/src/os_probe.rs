//! Asking a host which OS family it belongs to, once (#425).
//!
//! The parsing lives in [`filar_core::os_family`]; this module is the part
//! that needs a host and a runtime: run [`OS_RELEASE_COMMAND`] over the
//! executor, keep the answer, and never ask again.
//!
//! # Why the caching is the point
//!
//! A fleet operation resolves a command variant per check per host. Asking
//! the host its family each time would put a round trip in front of every
//! check — on a group of twelve hosts running eight checks, ninety-six
//! extra commands to learn the same fact. The answer also cannot change
//! under us within a session: a host does not become Alpine halfway
//! through.
//!
//! # A failure to reach the host is not an answer
//!
//! Two failures look alike and must not be treated alike. A command that
//! *ran* and produced nothing placeable — no `/etc/os-release`, an exotic
//! distribution — is a real answer of [`OsFamily::Unknown`], and it is
//! cached: asking again would produce the same nothing. A command that
//! never ran, because the connection dropped, is not an answer at all: it
//! is reported as an error and **not** cached, so a later call can try
//! again once the host is back. Caching that one would quietly mark every
//! check on the host "not applicable" for the rest of the session.

use tokio::sync::OnceCell;

use filar_core::{OsFamily, Result, OS_RELEASE_COMMAND};
use filar_transport::CommandExecutor;

/// Detects and remembers one host's OS family.
///
/// Hold one per host, for as long as the session to that host lasts.
#[derive(Debug, Default)]
pub struct OsFamilyProbe {
    cached: OnceCell<OsFamily>,
}

impl OsFamilyProbe {
    /// A probe that has not asked yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A probe that already knows the answer.
    ///
    /// For restoring a saved fleet session (#442) without re-probing every
    /// host, and for tests.
    pub fn with_known(family: OsFamily) -> Self {
        let cached = OnceCell::new();
        // The cell is fresh, so this cannot be already-initialised.
        let _ = cached.set(family);
        Self { cached }
    }

    /// The family, asking the host only the first time.
    ///
    /// Concurrent callers share one probe: `OnceCell` runs the initialiser
    /// once and the rest await its result, so a burst of checks starting
    /// together still costs one command.
    ///
    /// Returns `Err` only when the command could not be run at all — see
    /// the module docs on why that case is not cached.
    pub async fn detect(&self, exec: &dyn CommandExecutor) -> Result<OsFamily> {
        self.cached
            .get_or_try_init(|| async {
                let result = exec.run(OS_RELEASE_COMMAND).await?;
                // A non-zero exit means the command ran and the host has no
                // readable os-release. That is an answer: Unknown, cached.
                if result.exit_code != Some(0) {
                    tracing::debug!(
                        exit_code = ?result.exit_code,
                        "os-release unreadable; treating the OS family as unknown"
                    );
                    return Ok(OsFamily::Unknown);
                }
                let family = OsFamily::from_os_release(&result.stdout);
                tracing::debug!(%family, "detected OS family");
                Ok(family)
            })
            .await
            .copied()
    }

    /// The remembered family, without asking.
    ///
    /// `None` means no successful probe has happened yet.
    pub fn cached(&self) -> Option<OsFamily> {
        self.cached.get().copied()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use filar_core::CoreError;
    use filar_transport::{CommandResult, StreamEvent};
    use tokio::sync::mpsc;

    use super::*;

    /// What the fake host should do when asked.
    #[derive(Clone)]
    enum Behaviour {
        /// The command ran and printed this, exiting 0.
        Prints(&'static str),
        /// The command ran and failed (no such file).
        Fails,
        /// The command never ran — the connection is gone.
        Unreachable,
    }

    struct FakeHost {
        behaviour: std::sync::Mutex<Behaviour>,
        calls: AtomicUsize,
    }

    impl FakeHost {
        fn new(behaviour: Behaviour) -> Arc<Self> {
            Arc::new(Self {
                behaviour: std::sync::Mutex::new(behaviour),
                calls: AtomicUsize::new(0),
            })
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }

        fn set(&self, behaviour: Behaviour) {
            *self.behaviour.lock().expect("test mutex") = behaviour;
        }
    }

    #[filar_transport::async_trait]
    impl CommandExecutor for FakeHost {
        async fn run(&self, command: &str) -> Result<CommandResult> {
            assert_eq!(
                command, OS_RELEASE_COMMAND,
                "the probe must run the pinned detection command"
            );
            self.calls.fetch_add(1, Ordering::SeqCst);
            let behaviour = self.behaviour.lock().expect("test mutex").clone();
            match behaviour {
                Behaviour::Prints(stdout) => Ok(CommandResult {
                    stdout: stdout.to_string(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: Duration::from_millis(0),
                    cwd: None,
                }),
                Behaviour::Fails => Ok(CommandResult {
                    stdout: String::new(),
                    stderr: "cat: /etc/os-release: No such file or directory".into(),
                    exit_code: Some(1),
                    duration: Duration::from_millis(0),
                    cwd: None,
                }),
                Behaviour::Unreachable => {
                    Err(CoreError::ConnectionLost("channel closed".into()))
                }
            }
        }

        async fn run_streaming(&self, _command: &str) -> Result<mpsc::Receiver<StreamEvent>> {
            unreachable!("the probe does not stream")
        }

        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    const ALPINE: &str = "NAME=\"Alpine Linux\"\nID=alpine\nVERSION_ID=3.20.3\n";
    const ALMA: &str = "NAME=\"AlmaLinux\"\nID=\"almalinux\"\nID_LIKE=\"rhel centos fedora\"\n";

    #[tokio::test]
    async fn the_family_is_detected_from_the_host() {
        let host = FakeHost::new(Behaviour::Prints(ALPINE));
        let probe = OsFamilyProbe::new();
        assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Alpine);

        let host = FakeHost::new(Behaviour::Prints(ALMA));
        let probe = OsFamilyProbe::new();
        assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Rhel);
    }

    /// The reason this type exists: one command per session, not per check.
    #[tokio::test]
    async fn the_host_is_asked_only_once() {
        let host = FakeHost::new(Behaviour::Prints(ALPINE));
        let probe = OsFamilyProbe::new();

        for _ in 0..8 {
            assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Alpine);
        }
        assert_eq!(host.calls(), 1, "the probe must cache the first answer");
        assert_eq!(probe.cached(), Some(OsFamily::Alpine));
    }

    /// Checks start together, so the burst case is the normal case.
    #[tokio::test]
    async fn concurrent_callers_share_one_probe() {
        let host = FakeHost::new(Behaviour::Prints(ALPINE));
        let probe = Arc::new(OsFamilyProbe::new());

        let mut tasks = Vec::new();
        for _ in 0..16 {
            let probe = probe.clone();
            let host = host.clone();
            tasks.push(tokio::spawn(async move {
                probe.detect(host.as_ref()).await.unwrap()
            }));
        }
        for task in tasks {
            assert_eq!(task.await.unwrap(), OsFamily::Alpine);
        }
        assert_eq!(host.calls(), 1);
    }

    /// A command that ran and found nothing is a real answer, so it sticks.
    #[tokio::test]
    async fn a_host_without_os_release_is_unknown_and_cached() {
        let host = FakeHost::new(Behaviour::Fails);
        let probe = OsFamilyProbe::new();

        assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Unknown);
        assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Unknown);
        assert_eq!(host.calls(), 1, "a real answer is not re-asked");
        assert_eq!(probe.cached(), Some(OsFamily::Unknown));
    }

    /// A command that never ran is not an answer: caching it would mark
    /// every variant-carrying check "not applicable" for the whole session.
    #[tokio::test]
    async fn an_unreachable_host_is_an_error_and_is_not_cached() {
        let host = FakeHost::new(Behaviour::Unreachable);
        let probe = OsFamilyProbe::new();

        assert!(probe.detect(host.as_ref()).await.is_err());
        assert_eq!(probe.cached(), None, "a failed probe must not be cached");

        // The host comes back; the next call gets the real answer.
        host.set(Behaviour::Prints(ALMA));
        assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Rhel);
        assert_eq!(probe.cached(), Some(OsFamily::Rhel));
        assert_eq!(host.calls(), 2, "the retry is the second call, and the last");

        assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Rhel);
        assert_eq!(host.calls(), 2);
    }

    #[tokio::test]
    async fn a_known_family_is_never_probed() {
        let host = FakeHost::new(Behaviour::Unreachable);
        let probe = OsFamilyProbe::with_known(OsFamily::Debian);

        assert_eq!(probe.cached(), Some(OsFamily::Debian));
        assert_eq!(probe.detect(host.as_ref()).await.unwrap(), OsFamily::Debian);
        assert_eq!(host.calls(), 0);
    }

    /// The probe and the catalog have to agree end to end: detect, then
    /// resolve the built-in check that actually varies.
    #[tokio::test]
    async fn detection_feeds_the_catalog_variant_choice() {
        use filar_core::{CommandForOs, FleetCheckCatalog};

        let catalog = FleetCheckCatalog::builtin();
        let disk = catalog.get("disk-usage").expect("built-in check missing");

        let alpine = FakeHost::new(Behaviour::Prints(ALPINE));
        let probe = OsFamilyProbe::new();
        let family = probe.detect(alpine.as_ref()).await.unwrap();
        assert_eq!(disk.command_for(family), CommandForOs::Runnable("df"));

        let alma = FakeHost::new(Behaviour::Prints(ALMA));
        let probe = OsFamilyProbe::new();
        let family = probe.detect(alma.as_ref()).await.unwrap();
        assert_eq!(
            disk.command_for(family),
            CommandForOs::Runnable("df --output=source,size,used,avail,pcent,target")
        );
    }
}
