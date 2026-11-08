//! Private key files for `SshAuth::Key`, including passphrase-protected
//! ones (#480).
//!
//! The helpers here are **blocking** (file I/O and, for an encrypted key,
//! a deliberately slow KDF). Call them from startup code or through
//! `tokio::task::spawn_blocking`, never directly on the async runtime.
//!
//! A passphrase never appears in an error message or a log line produced
//! here: errors name the key file only.

use std::path::{Path, PathBuf};

use russh::keys::{load_secret_key, Error as KeyError, PrivateKey};

use filar_core::{CoreError, Result, SecretProvider, SshAuth, SshTarget};
use tracing::warn;

/// Whether a private key file can be used without a passphrase.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyProtection {
    /// The key loads as is.
    Plain,
    /// The key is encrypted: a passphrase is needed to use it.
    Encrypted,
}

/// The key file of a `SshAuth::Key` target: `path` if set, else
/// `~/.ssh/id_ed25519`, falling back to `~/.ssh/id_rsa`.
pub fn resolve_key_path(path: Option<&Path>) -> PathBuf {
    path.map(Path::to_path_buf)
        .unwrap_or_else(crate::ssh::dirs_or_default)
}

/// Whether the key at `path` needs a passphrase (blocking).
///
/// Errors when the file cannot be read or is not a usable private key.
pub fn key_protection(path: &Path) -> Result<KeyProtection> {
    match load_secret_key(path, None) {
        Ok(_) => Ok(KeyProtection::Plain),
        Err(KeyError::KeyIsEncrypted) => Ok(KeyProtection::Encrypted),
        Err(e) => Err(unreadable(path, &e)),
    }
}

/// Whether `passphrase` decrypts the key at `path` (blocking).
///
/// Lets a caller check a passphrase locally — and ask again — before any
/// connection is attempted.
pub fn key_passphrase_matches(path: &Path, passphrase: &str) -> bool {
    load_secret_key(path, Some(passphrase)).is_ok()
}

/// What [`fill_key_passphrase`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPassphrase {
    /// Not a key target, or the key is not encrypted.
    NotNeeded,
    /// The target now carries a passphrase that decrypts its key.
    Found,
    /// The key is encrypted and no stored passphrase decrypts it: ask the
    /// user, or give up. Carries the key file for the prompt.
    Missing(PathBuf),
}

/// Fill the passphrase of `target`'s encrypted key from what is stored,
/// without asking anyone (blocking).
///
/// Tried in order, each checked by decrypting the key: a passphrase already
/// on the target, the OS credential store entry
/// [`ssh_key_passphrase:<name>`](filar_core::ssh_key_passphrase_name) in
/// `store`, then `SSH_KEY_PASSPHRASE` in `env`. The fleet passes no `env`:
/// one shared secret for every host is what per-host credentials avoid.
/// A stored passphrase that does not decrypt the key is skipped with a
/// warning that names its source, never its value.
///
/// On [`KeyPassphrase::Missing`] the target carries **no** passphrase: one
/// it came with was tried first and did not decrypt the key, so it is
/// dropped rather than left to fail the login.
pub fn fill_key_passphrase(
    target: &mut SshTarget,
    store: &dyn SecretProvider,
    env: Option<&dyn SecretProvider>,
) -> Result<KeyPassphrase> {
    let SshAuth::Key { path, passphrase } = &mut target.auth else {
        return Ok(KeyPassphrase::NotNeeded);
    };
    let key_path = resolve_key_path(path.as_deref());
    if key_protection(&key_path)? == KeyProtection::Plain {
        return Ok(KeyPassphrase::NotNeeded);
    }
    let candidates = [
        ("target", passphrase.take()),
        (
            "OS credential store",
            store.get(&filar_core::ssh_key_passphrase_name(&target.name)).ok(),
        ),
        (
            "SSH_KEY_PASSPHRASE",
            env.and_then(|e| e.get(filar_core::secrets::env_vars::SSH_KEY_PASSPHRASE).ok()),
        ),
    ];
    for (source, candidate) in candidates {
        let Some(candidate) = candidate.filter(|p| !p.is_empty()) else {
            continue;
        };
        if key_passphrase_matches(&key_path, &candidate) {
            *passphrase = Some(candidate);
            return Ok(KeyPassphrase::Found);
        }
        warn!(target = %target.name, %source, "stored passphrase does not decrypt the SSH key");
    }
    Ok(KeyPassphrase::Missing(key_path))
}

/// Load the key at `path`, decrypting it with `passphrase` when it is
/// encrypted (blocking). `target_name` only feeds the hint of a "passphrase
/// required" error.
pub(crate) fn load_key(
    path: &Path,
    passphrase: Option<&str>,
    target_name: &str,
) -> Result<PrivateKey> {
    match load_secret_key(path, None) {
        Ok(key) => Ok(key),
        Err(KeyError::KeyIsEncrypted) => {
            let Some(passphrase) = passphrase else {
                return Err(passphrase_required(path, target_name));
            };
            load_secret_key(path, Some(passphrase)).map_err(|_| wrong_passphrase(path))
        }
        Err(e) => Err(unreadable(path, &e)),
    }
}

fn unreadable(path: &Path, err: &KeyError) -> CoreError {
    CoreError::Other(format!("failed to load SSH key {}: {err}", path.display()))
}

fn passphrase_required(path: &Path, target_name: &str) -> CoreError {
    CoreError::Other(format!(
        "SSH key {} is encrypted and no passphrase was given; save it in the OS credential \
         store as {}, set SSH_KEY_PASSPHRASE, or load the key into ssh-agent and use \
         type = \"agent\"",
        path.display(),
        filar_core::ssh_key_passphrase_name(target_name),
    ))
}

fn wrong_passphrase(path: &Path) -> CoreError {
    CoreError::Other(format!("wrong passphrase for SSH key {}", path.display()))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use russh::keys::ssh_key::private::Ed25519Keypair;
    use russh::keys::ssh_key::{Cipher, Kdf, LineEnding};

    /// A throwaway ed25519 key written to a fresh temp file, encrypted with
    /// `passphrase` when one is given. Deterministic seed, one bcrypt round:
    /// fast, and nothing secret is checked into the repository.
    pub(crate) fn write_test_key(passphrase: Option<&str>) -> PathBuf {
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[7u8; 32]));
        let key = match passphrase {
            Some(p) => key
                .encrypt_with(
                    Cipher::Aes256Ctr,
                    Kdf::Bcrypt { salt: [3u8; 16].to_vec(), rounds: 1 },
                    0x1234_5678,
                    p,
                )
                .expect("encrypt test key"),
            None => key,
        };
        let pem = key.to_openssh(LineEnding::LF).expect("encode test key");
        let path = std::env::temp_dir().join(format!("filar-key-{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, pem.as_bytes()).expect("write test key");
        path
    }

    #[test]
    fn a_plain_key_needs_no_passphrase() {
        let path = write_test_key(None);
        assert_eq!(key_protection(&path).expect("readable"), KeyProtection::Plain);
        assert!(load_key(&path, None, "t").is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_encrypted_key_is_reported_and_loads_with_its_passphrase() {
        let path = write_test_key(Some("correct horse"));
        assert_eq!(key_protection(&path).expect("readable"), KeyProtection::Encrypted);
        assert!(key_passphrase_matches(&path, "correct horse"));
        assert!(!key_passphrase_matches(&path, "wrong"));
        assert!(load_key(&path, Some("correct horse"), "t").is_ok());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_missing_passphrase_names_the_key_and_where_to_put_it() {
        let path = write_test_key(Some("correct horse"));
        let msg = load_key(&path, None, "prod-web").expect_err("needs passphrase").to_string();
        assert!(msg.contains("is encrypted"), "{msg}");
        assert!(msg.contains("ssh_key_passphrase:prod-web"), "{msg}");
        assert!(msg.contains("SSH_KEY_PASSPHRASE"), "{msg}");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_wrong_passphrase_is_a_clear_error_that_does_not_echo_it() {
        let path = write_test_key(Some("correct horse"));
        let msg = load_key(&path, Some("battery staple"), "t").expect_err("wrong").to_string();
        assert!(msg.starts_with("wrong passphrase for SSH key"), "{msg}");
        assert!(!msg.contains("battery staple"), "{msg}");
        assert!(!msg.contains("correct horse"), "{msg}");
        let _ = std::fs::remove_file(path);
    }

    fn key_target(path: &Path) -> SshTarget {
        SshTarget {
            name: "prod-web".into(),
            host: "h".into(),
            port: 22,
            user: "u".into(),
            auth: SshAuth::Key { path: Some(path.to_path_buf()), passphrase: None },
            host_key_policy: Default::default(),
            tags: Vec::new(),
        }
    }

    fn passphrase_of(t: &SshTarget) -> Option<&str> {
        match &t.auth {
            SshAuth::Key { passphrase, .. } => passphrase.as_deref(),
            _ => None,
        }
    }

    #[test]
    fn the_store_wins_over_the_environment() {
        use filar_core::StaticSecretProvider;
        let path = write_test_key(Some("correct horse"));
        let store = StaticSecretProvider::new();
        store.insert("ssh_key_passphrase:prod-web", "correct horse");
        let env = StaticSecretProvider::new();
        env.insert("SSH_KEY_PASSPHRASE", "stale");
        let mut t = key_target(&path);
        assert_eq!(fill_key_passphrase(&mut t, &store, Some(&env)).unwrap(), KeyPassphrase::Found);
        assert_eq!(passphrase_of(&t), Some("correct horse"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_wrong_stored_passphrase_is_skipped_for_the_next_source() {
        use filar_core::StaticSecretProvider;
        let path = write_test_key(Some("correct horse"));
        let store = StaticSecretProvider::new();
        store.insert("ssh_key_passphrase:prod-web", "outdated");
        let env = StaticSecretProvider::new();
        env.insert("SSH_KEY_PASSPHRASE", "correct horse");
        let mut t = key_target(&path);
        assert_eq!(fill_key_passphrase(&mut t, &store, Some(&env)).unwrap(), KeyPassphrase::Found);
        assert_eq!(passphrase_of(&t), Some("correct horse"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn without_env_the_shared_variable_is_not_consulted() {
        use filar_core::StaticSecretProvider;
        let path = write_test_key(Some("correct horse"));
        let env = StaticSecretProvider::new();
        env.insert("SSH_KEY_PASSPHRASE", "correct horse");
        let mut t = key_target(&path);
        let got = fill_key_passphrase(&mut t, &StaticSecretProvider::new(), None).unwrap();
        assert_eq!(got, KeyPassphrase::Missing(path.clone()));
        assert_eq!(passphrase_of(&t), None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn a_plain_key_or_another_auth_needs_nothing() {
        use filar_core::StaticSecretProvider;
        let path = write_test_key(None);
        let store = StaticSecretProvider::new();
        let mut t = key_target(&path);
        assert_eq!(fill_key_passphrase(&mut t, &store, None).unwrap(), KeyPassphrase::NotNeeded);
        t.auth = SshAuth::Agent;
        assert_eq!(fill_key_passphrase(&mut t, &store, None).unwrap(), KeyPassphrase::NotNeeded);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn an_unreadable_key_is_an_error() {
        let path = std::env::temp_dir().join(format!("filar-no-key-{}", uuid::Uuid::new_v4()));
        assert!(key_protection(&path).is_err());
    }
}
