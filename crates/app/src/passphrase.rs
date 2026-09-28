//! Passphrase of an encrypted SSH key for a `--target` launch (#480).
//!
//! The connection is made before the TUI starts, so the passphrase is
//! resolved here, in the plain terminal: OS credential store first, then
//! `SSH_KEY_PASSPHRASE` (through a [`SecretProvider`], not a direct env
//! read), then a prompt without echo. Every candidate is checked locally by
//! decrypting the key, so a wrong passphrase is caught — and asked again —
//! before any connection is attempted.
//!
//! The passphrase is never logged or printed; messages name the key file
//! and the target only.

use std::io::{IsTerminal, Write};
use std::path::Path;

use filar_core::{ssh_key_passphrase_name, SecretProvider, SshAuth, SshTarget};
use filar_transport::{fill_key_passphrase, key_passphrase_matches, KeyPassphrase};

/// How many times a wrong passphrase may be typed before giving up.
const PROMPT_ATTEMPTS: usize = 3;

/// Where the passphrase may come from, besides the target itself.
pub struct PassphraseSources<'a> {
    /// The OS credential store (`ssh_key_passphrase:<target>`).
    pub keyring: &'a dyn SecretProvider,
    /// The process environment (`SSH_KEY_PASSPHRASE`).
    pub env: &'a dyn SecretProvider,
}

/// Talks to the user. `ask` returns `None` when no prompt is possible (no
/// terminal) or the user cancelled.
pub trait PassphrasePrompt {
    /// Ask for the passphrase without echo; `retry` is set after a wrong one.
    fn ask(&mut self, key: &Path, target: &str, retry: bool) -> Option<String>;
    /// Ask whether to keep the typed passphrase in the OS credential store.
    fn offer_to_save(&mut self, target: &str) -> bool;
    /// Store the passphrase under `name` in the OS credential store.
    fn save(&mut self, name: &str, passphrase: &str);
}

/// Fill `target`'s key passphrase when its key is encrypted.
///
/// No-op for a non-key target or an unencrypted key. Errors when the key
/// cannot be read, or no working passphrase was found or typed.
pub fn resolve_key_passphrase(
    target: &mut SshTarget,
    sources: &PassphraseSources<'_>,
    prompt: &mut dyn PassphrasePrompt,
) -> anyhow::Result<()> {
    let key_path = match fill_key_passphrase(target, sources.keyring, Some(sources.env))? {
        KeyPassphrase::NotNeeded | KeyPassphrase::Found => return Ok(()),
        KeyPassphrase::Missing(path) => path,
    };
    let entry = ssh_key_passphrase_name(&target.name);
    for attempt in 0..PROMPT_ATTEMPTS {
        let Some(typed) = prompt.ask(&key_path, &target.name, attempt > 0) else {
            break;
        };
        if typed.is_empty() || !key_passphrase_matches(&key_path, &typed) {
            continue;
        }
        if prompt.offer_to_save(&target.name) {
            prompt.save(&entry, &typed);
        }
        if let SshAuth::Key { passphrase, .. } = &mut target.auth {
            *passphrase = Some(typed);
        }
        return Ok(());
    }
    anyhow::bail!(
        "SSH key {} is encrypted and no working passphrase was given; save it in the OS \
         credential store as {entry}, set SSH_KEY_PASSPHRASE, or load the key into ssh-agent \
         and use type = \"agent\"",
        key_path.display()
    )
}

/// The real terminal: prompts on stderr, reads without echo.
pub struct TerminalPrompt;

impl PassphrasePrompt for TerminalPrompt {
    fn ask(&mut self, key: &Path, target: &str, retry: bool) -> Option<String> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        if retry {
            eprintln!("Wrong passphrase, try again.");
        }
        read_hidden(&format!("Passphrase for SSH key {} ({target}): ", key.display()))
    }

    fn offer_to_save(&mut self, _target: &str) -> bool {
        if !std::io::stdin().is_terminal() {
            return false;
        }
        eprint!("Save the passphrase in the OS credential store? [y/N] ");
        let _ = std::io::stderr().flush();
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err() {
            return false;
        }
        matches!(answer.trim(), "y" | "Y" | "yes" | "Yes")
    }

    fn save(&mut self, name: &str, passphrase: &str) {
        filar_core::secrets::save_secret_to_keyring(name, passphrase);
    }
}

/// Read one line from the terminal without echo. `None` on Esc / Ctrl+C or
/// a terminal error.
fn read_hidden(prompt: &str) -> Option<String> {
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal;

    eprint!("{prompt}");
    let _ = std::io::stderr().flush();
    terminal::enable_raw_mode().ok()?;
    let mut typed = String::new();
    let result = loop {
        let key = match event::read() {
            Ok(Event::Key(key)) => key,
            Ok(_) => continue,
            // A terminal that cannot be read will not recover: stop asking.
            Err(_) => break None,
        };
        // Windows reports key releases too; only presses carry input.
        if key.kind != KeyEventKind::Press {
            continue;
        }
        match key.code {
            KeyCode::Enter => break Some(typed),
            KeyCode::Esc => break None,
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => break None,
            KeyCode::Backspace => {
                typed.pop();
            }
            KeyCode::Char(c) => typed.push(c),
            _ => {}
        }
    };
    let _ = terminal::disable_raw_mode();
    eprintln!();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_core::StaticSecretProvider;
    use std::path::PathBuf;

    /// Scripted user: answers from `typed` in order, records saves.
    struct Scripted {
        typed: Vec<&'static str>,
        asked: usize,
        save: bool,
        saved: Vec<(String, String)>,
    }

    impl Scripted {
        fn new(typed: &[&'static str], save: bool) -> Self {
            Self { typed: typed.to_vec(), asked: 0, save, saved: Vec::new() }
        }
    }

    impl PassphrasePrompt for Scripted {
        fn ask(&mut self, _: &Path, _: &str, _: bool) -> Option<String> {
            let answer = self.typed.get(self.asked).map(|s| s.to_string());
            self.asked += 1;
            answer
        }
        fn offer_to_save(&mut self, _: &str) -> bool {
            self.save
        }
        fn save(&mut self, name: &str, passphrase: &str) {
            self.saved.push((name.into(), passphrase.into()));
        }
    }

    const GOOD: &str = "correct horse";

    /// A temp key file, removed when dropped — even when the test panics.
    struct TempKey(PathBuf);

    impl std::ops::Deref for TempKey {
        type Target = Path;
        fn deref(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempKey {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A throwaway encrypted key with a name unique within the process: a
    /// counter, not the clock — macOS's clock is coarse enough for two
    /// parallel tests to draw the same timestamp and delete each other's file.
    fn encrypted_key() -> TempKey {
        use russh::keys::ssh_key::private::Ed25519Keypair;
        use russh::keys::ssh_key::{Cipher, Kdf, LineEnding, PrivateKey};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[9u8; 32]))
            .encrypt_with(
                Cipher::Aes256Ctr,
                Kdf::Bcrypt { salt: [5u8; 16].to_vec(), rounds: 1 },
                7,
                GOOD,
            )
            .expect("encrypt");
        let pem = key.to_openssh(LineEnding::LF).expect("encode");
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir()
            .join(format!("filar-app-key-{}-{n}", std::process::id()));
        std::fs::write(&path, pem.as_bytes()).expect("write");
        TempKey(path)
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

    fn resolve(
        path: &Path,
        keyring: &StaticSecretProvider,
        env: &StaticSecretProvider,
        prompt: &mut Scripted,
    ) -> (SshTarget, anyhow::Result<()>) {
        let mut t = key_target(path);
        let res = resolve_key_passphrase(
            &mut t,
            &PassphraseSources { keyring, env },
            prompt,
        );
        (t, res)
    }

    #[test]
    fn the_credential_store_wins_and_nothing_is_asked() {
        let path = encrypted_key();
        let keyring = StaticSecretProvider::new();
        keyring.insert("ssh_key_passphrase:prod-web", GOOD);
        let env = StaticSecretProvider::new();
        env.insert("SSH_KEY_PASSPHRASE", "stale");
        let mut prompt = Scripted::new(&[], false);
        let (t, res) = resolve(&path, &keyring, &env, &mut prompt);
        res.expect("resolved");
        assert_eq!(passphrase_of(&t), Some(GOOD));
        assert_eq!(prompt.asked, 0);
    }

    #[test]
    fn the_environment_is_used_when_the_store_has_nothing() {
        let path = encrypted_key();
        let env = StaticSecretProvider::new();
        env.insert("SSH_KEY_PASSPHRASE", GOOD);
        let mut prompt = Scripted::new(&[], false);
        let (t, res) = resolve(&path, &StaticSecretProvider::new(), &env, &mut prompt);
        res.expect("resolved");
        assert_eq!(passphrase_of(&t), Some(GOOD));
        assert_eq!(prompt.asked, 0);
    }

    #[test]
    fn a_wrong_stored_passphrase_falls_through_to_the_prompt() {
        let path = encrypted_key();
        let keyring = StaticSecretProvider::new();
        keyring.insert("ssh_key_passphrase:prod-web", "outdated");
        let mut prompt = Scripted::new(&["typo", GOOD], true);
        let (t, res) = resolve(&path, &keyring, &StaticSecretProvider::new(), &mut prompt);
        res.expect("resolved");
        assert_eq!(passphrase_of(&t), Some(GOOD));
        assert_eq!(prompt.asked, 2, "asked again after the wrong one");
        assert_eq!(
            prompt.saved,
            [("ssh_key_passphrase:prod-web".to_string(), GOOD.to_string())]
        );
    }

    #[test]
    fn three_wrong_answers_give_an_error_without_the_passphrase() {
        let path = encrypted_key();
        let mut prompt = Scripted::new(&["a", "b", "c", GOOD], false);
        let empty = StaticSecretProvider::new();
        let (t, res) = resolve(&path, &empty, &empty, &mut prompt);
        let msg = res.expect_err("gave up").to_string();
        assert_eq!(prompt.asked, 3);
        assert_eq!(passphrase_of(&t), None);
        assert!(msg.contains("ssh_key_passphrase:prod-web"), "{msg}");
        for typed in ["a", "b", "c", GOOD] {
            assert!(!msg.contains(&format!(" {typed} ")), "{msg}");
        }
    }

    #[test]
    fn no_terminal_means_an_error_not_a_hang() {
        let path = encrypted_key();
        let mut prompt = Scripted::new(&[], false);
        let empty = StaticSecretProvider::new();
        let (_, res) = resolve(&path, &empty, &empty, &mut prompt);
        assert!(res.is_err());
    }

    #[test]
    fn a_non_key_target_is_left_alone() {
        let mut t = key_target(Path::new("/nonexistent"));
        t.auth = SshAuth::Agent;
        let empty = StaticSecretProvider::new();
        let mut prompt = Scripted::new(&[], false);
        resolve_key_passphrase(&mut t, &PassphraseSources { keyring: &empty, env: &empty }, &mut prompt)
            .expect("no-op");
        assert_eq!(prompt.asked, 0);
    }
}
