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
//! | `key`, not encrypted | always — a key that fails to load is "no contact", not "skipped" |
//! | `key`, encrypted | the OS credential store has `ssh_key_passphrase:<name>` (#480) |
//! | `password` with a value | always (the config warns about plain text elsewhere) |
//! | `password` without a value | the OS credential store has `ssh_target:<name>` |
//! | `agent` | always — an agent that is down or holds no accepted key is "no contact" |
//!
//! An encrypted key is only recognised here, not decrypted — that is slow on
//! purpose and would stall the fleet's opening for every such host; a
//! stored passphrase that turns out wrong fails that host's login ("no
//! contact"). The shared `SSH_KEY_PASSPHRASE` is not used, for the same
//! reason as `SSH_PASSWORD`.
//!
//! A store that cannot be consulted at all (no Secret Service on a headless
//! Linux, a locked keychain) is its own reason, not "no password": the
//! password may well be stored, and telling the user to save it again would
//! send them the wrong way.
//!
//! The resolution is made once, when the fleet opens, like the composition
//! itself (#426): nothing a host prints can change who is asked, and a
//! secret stored mid-fleet is picked up on the next entry, not silently.

use std::fmt;

use filar_core::config::{SshAuth, SshTarget};
use filar_core::error::CoreError;
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
    /// Password auth, and the OS credential store could not be consulted.
    /// The store's error is not kept: nothing about a lookup reaches the
    /// transcript but the fact that it failed.
    StoreUnavailable,
    /// Key auth with an encrypted key, and the OS credential store holds no
    /// passphrase for this host (#480).
    NoPassphrase,
}

impl fmt::Display for MissingCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NoPassword => "no password in the OS credential store",
            Self::StoreUnavailable => "OS credential store unavailable",
            Self::NoPassphrase => "encrypted SSH key, no passphrase in the OS credential store",
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
        SshAuth::Key { path, passphrase } => {
            let key = filar_transport::resolve_key_path(path.as_deref());
            // Unreadable → let the login fail as "no contact", like before.
            let encrypted = matches!(
                filar_transport::key_protection(&key),
                Ok(filar_transport::KeyProtection::Encrypted)
            );
            if !encrypted || passphrase.is_some() {
                return Ok(target.clone());
            }
            let passphrase = stored(store, &filar_core::ssh_key_passphrase_name(&target.name))
                .map_err(|missing| match missing {
                    MissingCredentials::NoPassword => MissingCredentials::NoPassphrase,
                    other => other,
                })?;
            let mut ready = target.clone();
            ready.auth = SshAuth::Key { path: path.clone(), passphrase: Some(passphrase) };
            Ok(ready)
        }
        // The SSH agent needs nothing resolved up front: each member's
        // connection opens its own agent session when it logs in.
        SshAuth::Agent => Ok(target.clone()),
        SshAuth::Password { password: Some(_) } => Ok(target.clone()),
        SshAuth::Password { password: None } => {
            let password = stored(store, &target_secret_name(&target.name))?;
            let mut ready = target.clone();
            ready.auth = SshAuth::Password {
                password: Some(password),
            };
            Ok(ready)
        }
    }
}

/// The secret stored under `name`: `NoPassword` when it is not there,
/// `StoreUnavailable` when the store could not be consulted.
fn stored(store: &dyn SecretProvider, name: &str) -> Result<String, MissingCredentials> {
    // `Secret` is the provider's "not stored"; any other error means the
    // store itself could not be used.
    match store.get(name) {
        Ok(p) if !p.is_empty() => Ok(p),
        Ok(_) | Err(CoreError::Secret(_)) => Err(MissingCredentials::NoPassword),
        Err(_) => Err(MissingCredentials::StoreUnavailable),
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
            target("web-1", SshAuth::Key { path: None, passphrase: None }),
            target("web-2", SshAuth::Password { password: None }),
            target("web-3", SshAuth::Agent),
            target("web-4", SshAuth::Password { password: None }),
        ]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_target:web-4", "pw-of-web-4");
        let creds = FleetCredentials::resolve(&op, &store);

        assert_eq!(creds.participants(&op), ["web-1", "web-3", "web-4"]);
        assert_eq!(creds.skipped(&op), [("web-2", MissingCredentials::NoPassword)]);
        assert!(matches!(creds.ready(2).expect("web-3 ready").auth, SshAuth::Agent));
        let handles: Vec<_> = op.handles().collect();
        assert_eq!(creds.missing(&op, handles[1]), Some(MissingCredentials::NoPassword));
        assert_eq!(creds.missing(&op, handles[0]), None);
    }

    /// A throwaway ed25519 key file, encrypted with `passphrase`.
    fn encrypted_key(passphrase: &str) -> std::path::PathBuf {
        use russh::keys::ssh_key::private::Ed25519Keypair;
        use russh::keys::ssh_key::{Cipher, Kdf, LineEnding, PrivateKey};
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[4u8; 32]))
            .encrypt_with(
                Cipher::Aes256Ctr,
                Kdf::Bcrypt { salt: [2u8; 16].to_vec(), rounds: 1 },
                1,
                passphrase,
            )
            .expect("encrypt");
        let pem = key.to_openssh(LineEnding::LF).expect("encode");
        let path = std::env::temp_dir().join(format!(
            "filar-fleet-key-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or_default()
        ));
        std::fs::write(&path, pem.as_bytes()).expect("write");
        path
    }

    #[test]
    fn an_encrypted_key_takes_part_only_with_its_own_stored_passphrase() {
        let key = encrypted_key("per-host");
        let key_auth = || SshAuth::Key { path: Some(key.clone()), passphrase: None };
        let op = fleet(&[target("web-1", key_auth()), target("web-2", key_auth())]);
        let store = StaticSecretProvider::new();
        store.insert("ssh_key_passphrase:web-1", "per-host");
        // The tab's shared variable is not a fleet credential.
        store.insert("SSH_KEY_PASSPHRASE", "per-host");
        let creds = FleetCredentials::resolve(&op, &store);

        assert_eq!(creds.participants(&op), ["web-1"]);
        assert_eq!(creds.skipped(&op), [("web-2", MissingCredentials::NoPassphrase)]);
        match &creds.ready(0).expect("web-1 ready").auth {
            SshAuth::Key { passphrase, .. } => assert_eq!(passphrase.as_deref(), Some("per-host")),
            other => panic!("unexpected auth {other:?}"),
        }
        let _ = std::fs::remove_file(key);
    }

    #[test]
    fn an_encrypted_key_with_an_unusable_store_says_so() {
        let key = encrypted_key("x");
        let op = fleet(&[target("web-1", SshAuth::Key { path: Some(key.clone()), passphrase: None })]);
        let creds = FleetCredentials::resolve(&op, &BrokenStore);
        assert_eq!(creds.skipped(&op), [("web-1", MissingCredentials::StoreUnavailable)]);
        let _ = std::fs::remove_file(key);
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

    /// A store that cannot be consulted at all.
    struct BrokenStore;

    impl SecretProvider for BrokenStore {
        fn get(&self, _name: &str) -> filar_core::Result<String> {
            Err(CoreError::Other("Secret Service unreachable".into()))
        }
        fn secret_names(&self) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn an_unreachable_store_is_not_reported_as_a_missing_password() {
        let op = fleet(&[
            target("web-1", SshAuth::Password { password: None }),
            target("web-2", SshAuth::Key { path: None, passphrase: None }),
        ]);
        let creds = FleetCredentials::resolve(&op, &BrokenStore);
        assert_eq!(creds.skipped(&op), [("web-1", MissingCredentials::StoreUnavailable)]);
        assert_eq!(creds.participants(&op), ["web-2"]);
        let reason = MissingCredentials::StoreUnavailable.to_string();
        assert!(!reason.contains("Secret Service unreachable"), "no store error text: {reason}");
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
