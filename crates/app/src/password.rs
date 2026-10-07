//! Password of a password-login SSH target for a launch (#495).
//!
//! The launcher and the session are separate processes, and the password
//! crosses only through the OS credential store: `pending_launch.json` never
//! holds it. Without "Save password" there is no stored entry, so the session
//! resolves the password here, in the plain terminal, before the TUI starts:
//! OS credential store (`ssh_target:<name>`) first, then `SSH_PASSWORD`
//! (through a [`SecretProvider`], not a direct env read), then a prompt
//! without echo. A typed password lives only in memory and in the
//! [`SshTarget`]; storing it is offered once the login has succeeded.
//!
//! The password is never logged or printed; messages name the target only.

use std::io::IsTerminal;

use filar_core::{SecretProvider, SshAuth, SshTarget};

/// How many times an empty answer is asked again before giving up.
const PROMPT_ATTEMPTS: usize = 3;

/// Where the password may come from, besides the target itself.
pub struct PasswordSources<'a> {
    /// The OS credential store (`ssh_target:<target>`).
    pub keyring: &'a dyn SecretProvider,
    /// The process environment (`SSH_PASSWORD`).
    pub env: &'a dyn SecretProvider,
}

/// Talks to the user. `ask` returns `None` when no prompt is possible (no
/// terminal) or the user cancelled.
pub trait PasswordPrompt {
    /// Ask for the SSH password of `target` without echo.
    fn ask(&mut self, target: &SshTarget) -> Option<String>;
    /// Ask whether to keep the typed password in the OS credential store.
    fn offer_to_save(&mut self, target: &str) -> bool;
    /// Store the password under `name` in the OS credential store.
    fn save(&mut self, name: &str, password: &str);
}

/// Where [`resolve_ssh_password`] got the password from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PasswordOrigin {
    /// Not a password target, or the target already carried one.
    NotNeeded,
    /// The OS credential store or `SSH_PASSWORD`.
    Stored,
    /// Typed at the prompt: may be offered for saving after the login.
    Typed,
}

/// OS credential-store entry of a target's SSH password: `ssh_target:<name>`,
/// the same entry the launcher writes and the TUI reconnect reads.
pub fn password_entry(target_name: &str) -> String {
    format!("ssh_target:{target_name}")
}

/// Fill `target`'s password when it is a password target without one.
///
/// Errors — without the password, which is unknown anyway — when nothing was
/// stored and no terminal can ask (or the user cancelled).
pub fn resolve_ssh_password(
    target: &mut SshTarget,
    sources: &PasswordSources<'_>,
    prompt: &mut dyn PasswordPrompt,
) -> anyhow::Result<PasswordOrigin> {
    if !matches!(target.auth, SshAuth::Password { password: None }) {
        return Ok(PasswordOrigin::NotNeeded);
    }
    let stored = [
        sources.keyring.get(&password_entry(&target.name)).ok(),
        sources.env.get("SSH_PASSWORD").ok(),
    ]
    .into_iter()
    .flatten()
    .find(|p| !p.is_empty());
    if let Some(password) = stored {
        set_password(target, password);
        return Ok(PasswordOrigin::Stored);
    }
    for _ in 0..PROMPT_ATTEMPTS {
        match prompt.ask(target) {
            Some(typed) if !typed.is_empty() => {
                set_password(target, typed);
                return Ok(PasswordOrigin::Typed);
            }
            Some(_) => continue,
            None => break,
        }
    }
    anyhow::bail!(
        "no SSH password for {} ({}@{}): tick \"Save password\" for this host in the launcher, \
         set the SSH_PASSWORD environment variable, or start filar in a terminal and type the \
         password when asked",
        target.name,
        target.user,
        target.host
    )
}

/// After a successful login with a typed password, offer to store it under
/// [`password_entry`] — asked, never done silently.
pub fn offer_to_save_password(target: &SshTarget, prompt: &mut dyn PasswordPrompt) {
    let SshAuth::Password { password: Some(password) } = &target.auth else {
        return;
    };
    if prompt.offer_to_save(&target.name) {
        prompt.save(&password_entry(&target.name), password);
    }
}

fn set_password(target: &mut SshTarget, value: String) {
    if let SshAuth::Password { password } = &mut target.auth {
        *password = Some(value);
    }
}

/// The real terminal: prompts on stderr, reads without echo.
pub struct TerminalPrompt;

impl PasswordPrompt for TerminalPrompt {
    fn ask(&mut self, target: &SshTarget) -> Option<String> {
        if !std::io::stdin().is_terminal() {
            return None;
        }
        crate::passphrase::read_hidden(&format!(
            "SSH password for {}@{} ({}): ",
            target.user, target.host, target.name
        ))
    }

    fn offer_to_save(&mut self, _target: &str) -> bool {
        crate::passphrase::ask_yes_no("Save the password in the OS credential store? [y/N] ")
    }

    fn save(&mut self, name: &str, password: &str) {
        filar_core::secrets::save_secret_to_keyring(name, password);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_core::StaticSecretProvider;

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

    impl PasswordPrompt for Scripted {
        fn ask(&mut self, _: &SshTarget) -> Option<String> {
            let answer = self.typed.get(self.asked).map(|s| s.to_string());
            self.asked += 1;
            answer
        }
        fn offer_to_save(&mut self, _: &str) -> bool {
            self.save
        }
        fn save(&mut self, name: &str, password: &str) {
            self.saved.push((name.into(), password.into()));
        }
    }

    const SECRET: &str = "hunter2-Пароль";

    fn password_target(password: Option<&str>) -> SshTarget {
        SshTarget {
            name: "prod-web".into(),
            host: "10.0.0.5".into(),
            port: 22,
            user: "deploy".into(),
            auth: SshAuth::Password { password: password.map(str::to_string) },
            host_key_policy: Default::default(),
            tags: Vec::new(),
        }
    }

    fn password_of(t: &SshTarget) -> Option<&str> {
        match &t.auth {
            SshAuth::Password { password } => password.as_deref(),
            _ => None,
        }
    }

    fn resolve(
        keyring: &StaticSecretProvider,
        env: &StaticSecretProvider,
        prompt: &mut Scripted,
    ) -> (SshTarget, anyhow::Result<PasswordOrigin>) {
        let mut t = password_target(None);
        let res = resolve_ssh_password(&mut t, &PasswordSources { keyring, env }, prompt);
        (t, res)
    }

    #[test]
    fn the_credential_store_wins_and_nothing_is_asked() {
        let keyring = StaticSecretProvider::new();
        keyring.insert("ssh_target:prod-web", SECRET);
        let env = StaticSecretProvider::new();
        env.insert("SSH_PASSWORD", "from-env");
        let mut prompt = Scripted::new(&["typed"], true);
        let (t, res) = resolve(&keyring, &env, &mut prompt);
        assert_eq!(res.unwrap(), PasswordOrigin::Stored);
        assert_eq!(password_of(&t), Some(SECRET));
        assert_eq!(prompt.asked, 0);
    }

    #[test]
    fn the_environment_comes_second() {
        let env = StaticSecretProvider::new();
        env.insert("SSH_PASSWORD", SECRET);
        let mut prompt = Scripted::new(&["typed"], true);
        let (t, res) = resolve(&StaticSecretProvider::new(), &env, &mut prompt);
        assert_eq!(res.unwrap(), PasswordOrigin::Stored);
        assert_eq!(password_of(&t), Some(SECRET));
        assert_eq!(prompt.asked, 0);
    }

    #[test]
    fn the_prompt_comes_last_and_an_empty_answer_is_asked_again() {
        let empty = StaticSecretProvider::new();
        let mut prompt = Scripted::new(&["", SECRET], true);
        let (t, res) = resolve(&empty, &empty, &mut prompt);
        assert_eq!(res.unwrap(), PasswordOrigin::Typed);
        assert_eq!(password_of(&t), Some(SECRET));
        assert_eq!(prompt.asked, 2);
        assert!(prompt.saved.is_empty(), "nothing is saved before the login");
    }

    #[test]
    fn no_terminal_means_an_error_that_says_what_to_do() {
        let empty = StaticSecretProvider::new();
        let mut prompt = Scripted::new(&[], true);
        let (t, res) = resolve(&empty, &empty, &mut prompt);
        let msg = res.unwrap_err().to_string();
        assert_eq!(password_of(&t), None);
        assert!(msg.contains("Save password") && msg.contains("SSH_PASSWORD"), "{msg}");
        assert!(msg.contains("prod-web"), "{msg}");
        assert!(!msg.contains("SshAuth"), "no internal names: {msg}");
    }

    #[test]
    fn three_empty_answers_give_up() {
        let empty = StaticSecretProvider::new();
        let mut prompt = Scripted::new(&["", "", "", SECRET], true);
        let (_, res) = resolve(&empty, &empty, &mut prompt);
        assert!(res.is_err());
        assert_eq!(prompt.asked, 3);
    }

    #[test]
    fn other_targets_and_explicit_passwords_are_left_alone() {
        let empty = StaticSecretProvider::new();
        let mut prompt = Scripted::new(&[SECRET], true);
        let sources = PasswordSources { keyring: &empty, env: &empty };
        let mut agent = password_target(None);
        agent.auth = SshAuth::Agent;
        assert_eq!(
            resolve_ssh_password(&mut agent, &sources, &mut prompt).unwrap(),
            PasswordOrigin::NotNeeded
        );
        let mut set = password_target(Some("given"));
        assert_eq!(
            resolve_ssh_password(&mut set, &sources, &mut prompt).unwrap(),
            PasswordOrigin::NotNeeded
        );
        assert_eq!(password_of(&set), Some("given"));
        assert_eq!(prompt.asked, 0);
    }

    #[test]
    fn saving_is_offered_and_goes_to_the_launchers_entry() {
        let target = password_target(Some(SECRET));
        let mut yes = Scripted::new(&[], true);
        offer_to_save_password(&target, &mut yes);
        assert_eq!(yes.saved, [("ssh_target:prod-web".to_string(), SECRET.to_string())]);
        let mut no = Scripted::new(&[], false);
        offer_to_save_password(&target, &mut no);
        assert!(no.saved.is_empty());
    }

    #[test]
    fn the_password_stays_out_of_debug_output() {
        let target = password_target(Some(SECRET));
        let debug = format!("{target:?}");
        assert!(!debug.contains(SECRET), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");
    }
}
