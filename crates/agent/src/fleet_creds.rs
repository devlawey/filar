//! Credentials of fleet hosts (#436).
//!
//! A secret belongs to **one** host. There is no group-wide password in a
//! fleet: whatever lets `web-1` in is never offered to `web-2`. That rules
//! out the shared `SSH_PASSWORD` environment variable a single tab falls
//! back to — in a fleet it would be one secret sent to every member — and
//! it rules out the `$FILAR_SECRET_N` variables typed with Ctrl+P, which
//! are entered for the host of one tab.
//!
//! Asking for the missing secrets when the fleet opens would mean twelve
//! masked prompts in a row, which nobody can answer sensibly. So a host
//! whose credentials are not ready at that moment **drops out**: it gets no
//! task, the summary marks it [`Skipped`](crate::fleet_result::HostState::Skipped),
//! and the fleet runs on the rest. To bring it in, store its password in
//! the OS credential store (or open it on its own with Ctrl+O, where
//! Ctrl+P works as in any tab) and re-enter the fleet.
//!
//! What counts as ready, host by host:
//!
//! | Auth in config | Ready when |
//! |---|---|
//! | `key` | always — a key that fails to load is "no contact", not "skipped" |
//! | `password` with a value | always (the config warns about plain text elsewhere) |
//! | `password` without a value | the OS credential store has `ssh_target:<name>` |
//! | `agent` | never — SSH agent auth is not implemented by the transport |
//!
//! The resolution is made once, when the fleet opens, like the composition
//! itself (#426): nothing a host prints can change who is asked, and a
//! secret stored mid-fleet is picked up on the next entry, not silently.

use std::fmt;

use filar_core::config::{SshAuth, SshTarget};
use filar_core::fleet_op::{FleetOperation, HostHandle, OperationId};
use filar_core::secrets::SecretProvider;

/// Name of a target's password in the OS credential store.
///
/// The same entry a single tab reads for a password-auth target, so one
/// stored secret serves the host in a tab and in a fleet alike.
pub fn target_secret_name(target: &str) -> String {
    format!("ssh_target:{target}")
}

/// Why a fleet host has no usable credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissingCredentials {
    /// Password auth, and neither the config nor the OS credential store
    /// holds a password for this host.
    NoPassword,
    /// SSH agent auth, which the transport does not implement.
    AgentUnsupported,
}

impl fmt::Display for MissingCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoPassword => "no password in the OS credential store",
            Self::AgentUnsupported => "SSH agent auth is not supported",
        })
    }
}

/// Per-member credentials of one fleet, resolved when it opened.
///
/// `Debug` never prints a password: [`SshAuth`]'s own `Debug` replaces it.
#[derive(Debug, Clone)]
pub struct FleetCredentials {
    operation: OperationId,
    /// In composition order: the target to connect with (a stored password
    /// already filled in), or why the member drops out.
    hosts: Vec<Result<SshTarget, MissingCredentials>>,
}

impl FleetCredentials {
    /// Resolve every member of `op` against `store`, the OS credential
    /// store in the app. Only the member's own entry is consulted — never
    /// a shared one.
    pub fn resolve(op: &FleetOperation, store: &dyn SecretProvider) -> Self {
        let hosts = op
            .members()
            .iter()
            .map(|m| resolve_one(m.target(), store))
            .collect();
        Self {
            operation: op.id(),
            hosts,
        }
    }

    /// The operation these credentials were resolved for.
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    /// The target to connect member `index` with, if it has credentials.
    pub(crate) fn ready(&self, index: usize) -> Option<&SshTarget> {
        self.hosts.get(index).and_then(|h| h.as_ref().ok())
    }

    /// Why member `handle` drops out, or `None` if it takes part.
    pub fn missing(&self, op: &FleetOperation, handle: HostHandle) -> Option<MissingCredentials> {
        let index = op.handles().position(|h| h == handle)?;
        self.hosts.get(index).and_then(|h| h.as_ref().err().copied())
    }

    /// Names of the members that drop out, with the reason, in composition
    /// order — for the fleet's opening lines.
    pub fn skipped<'a>(&'a self, op: &'a FleetOperation) -> Vec<(&'a str, MissingCredentials)> {
        op.members()
            .iter()
            .zip(&self.hosts)
            .filter_map(|(m, h)| h.as_ref().err().map(|why| (m.name(), *why)))
            .collect()
    }

    /// Names of the members that take part, in composition order — the
    /// radius a command actually goes to.
    pub fn participants<'a>(&'a self, op: &'a FleetOperation) -> Vec<&'a str> {
        op.members()
            .iter()
            .zip(&self.hosts)
            .filter(|(_, h)| h.is_ok())
            .map(|(m, _)| m.name())
            .collect()
    }
}

fn resolve_one(target: &SshTarget, store: &dyn SecretProvider) -> Result<SshTarget, MissingCredentials> {
    match &target.auth {
        SshAuth::Key { .. } => Ok(target.clone()),
        SshAuth::Password { password: Some(_) } => Ok(target.clone()),
        SshAuth::Password { password: None } => {
            let password = store
                .get(&target_secret_name(&target.name))
                .ok()
                .filter(|p| !p.is_empty())
                .ok_or(MissingCredentials::NoPassword)?;
            let mut ready = target.clone();
            ready.auth = SshAuth::Password {
                password: Some(password),
            };
            Ok(ready)
        }
        SshAuth::Agent => Err(MissingCredentials::AgentUnsupported),
    }
}

#[cfg(test)]
mod tests {
    use filar_core::config::HostGroup;
    use filar_core::secrets::StaticSecretProvider;

    use super::*;

    fn target(name: &str, auth: SshAuth) -> SshTarget {
        SshTarget {
            name: name.into(),
            host: format!("{name}.example"),
            port: 22,
            user: "admin".into(),
            auth,
            host_key_policy: Default::default(),
            tags: vec!["web".into()],
        }
    }

    fn fleet(targets: &[SshTarget]) -> FleetOperation {
        let group = HostGroup {
            name: "web".into(),
            match_tags: vec!["web".into()],
            ..HostGroup::default()
        };
        FleetOperation::open(&group, targets)
    }

    #[test]
    fn a_host_without_credentials_drops_out_and_the_rest_take_part() {
        let op = fleet(&[
            target("web-1", SshAuth::Key { path: None }),
            target("web-2", SshAuth::Password { password: None }),
            target("web-3", SshAuth::Agent),
            target("web-4", SshAuth::Password { password: None }),
        ]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_target:web-4", "pw-of-web-4");
        let creds = FleetCredentials::resolve(&op, &store);

        assert_eq!(creds.participants(&op), ["web-1", "web-4"]);
        assert_eq!(
            creds.skipped(&op),
            [
                ("web-2", MissingCredentials::NoPassword),
                ("web-3", MissingCredentials::AgentUnsupported)
            ]
        );
        let handles: Vec<_> = op.handles().collect();
        assert_eq!(creds.missing(&op, handles[1]), Some(MissingCredentials::NoPassword));
        assert_eq!(creds.missing(&op, handles[0]), None);
    }

    #[test]
    fn a_stored_secret_serves_only_its_own_host() {
        let op = fleet(&[
            target("web-1", SshAuth::Password { password: None }),
            target("web-2", SshAuth::Password { password: None }),
        ]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_target:web-1", "only-web-1");
        // A shared password under the tab's fallback name is not a fleet
        // credential: it would be one secret for every host.
        store.insert("SSH_PASSWORD", "shared-for-everyone");
        let creds = FleetCredentials::resolve(&op, &store);

        match &creds.ready(0).expect("web-1 ready").auth {
            SshAuth::Password { password } => assert_eq!(password.as_deref(), Some("only-web-1")),
            other => panic!("unexpected auth {other:?}"),
        }
        assert!(creds.ready(1).is_none(), "web-2 has no secret of its own");
    }

    #[test]
    fn an_empty_stored_password_is_not_a_credential() {
        let op = fleet(&[target("web-1", SshAuth::Password { password: None })]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_target:web-1", "");
        let creds = FleetCredentials::resolve(&op, &store);
        assert!(creds.ready(0).is_none());
    }

    #[test]
    fn debug_never_prints_a_password() {
        let op = fleet(&[target("web-1", SshAuth::Password { password: None })]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_target:web-1", "hunter2-password");
        let creds = FleetCredentials::resolve(&op, &store);
        let debug = format!("{creds:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
    }
}
