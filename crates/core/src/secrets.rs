//! Reading secrets from the environment or other providers.
//!
//! Secrets are **never** hardcoded. They are read at runtime from a
//! [`SecretProvider`] — the default implementation ([`EnvSecretProvider`])
//! reads from environment variables, while [`StaticSecretProvider`] allows
//! programmatic injection (for bots, FFI, tests, and GUI input).
//!
//! The [`secrets`] module centralises this logic so that no other part of the
//! codebase needs to know variable names or storage details.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use zeroize::Zeroize;

use crate::error::{CoreError, Result};

/// Environment variable names for secrets.
pub mod env_vars {
    /// Default name of the LLM API key: the `key_env` of the `[llm]` section
    /// and of a profile that sets none (#490).
    pub const LLM_API_KEY: &str = "FILAR_LLM_API_KEY";
    /// Former default name of the LLM API key. Still read when nothing is
    /// stored under [`LLM_API_KEY`], so existing setups keep working (#490).
    pub const LEGACY_LLM_API_KEY: &str = "GLM_API_KEY";
    /// Passphrase of an encrypted SSH key (`SshAuth::Key`), for a single
    /// tab or `--target` launch. Never used by the fleet: a shared secret
    /// for every host is what per-host credentials exist to avoid (#480).
    pub const SSH_KEY_PASSPHRASE: &str = "SSH_KEY_PASSPHRASE";
    /// Default name of the command safety scorer's API key — the
    /// `key_env` of `[risk_scorer]` when it sets none (#549).
    pub const RISK_API_KEY: &str = "FILAR_RISK_API_KEY";
}

// ---------------------------------------------------------------------------
// SecretProvider trait
// ---------------------------------------------------------------------------

/// Trait for retrieving secrets by logical name.
///
/// Implementations:
/// - [`EnvSecretProvider`] — reads from `std::env` (default for TUI/desktop).
/// - [`StaticSecretProvider`] — in-memory `HashMap`, mutable at runtime
///   (for FFI, bots, tests, and GUI-provided credentials).
///
/// All engine code that needs a secret (API keys, `$FILAR_SECRET_N` variables)
/// must go through this trait — direct `std::env::var` calls for secrets are
/// only permitted inside `EnvSecretProvider`.
pub trait SecretProvider: Send + Sync {
    /// Retrieve a secret by its logical name (e.g. `"FILAR_LLM_API_KEY"`,
    /// `"$FILAR_SECRET_1"`).
    ///
    /// Returns [`CoreError::Secret`] if the secret is not available.
    fn get(&self, name: &str) -> Result<String>;

    /// List all known secret variable names (e.g. `["$FILAR_SECRET_1", ...]`).
    ///
    /// Used by `SecretSubstitutingExecutor` to enumerate placeholders for
    /// substitution and output sanitisation.
    fn secret_names(&self) -> Vec<String>;
}

// ---------------------------------------------------------------------------
// EnvSecretProvider
// ---------------------------------------------------------------------------

/// Default [`SecretProvider`] — reads secrets from environment variables.
///
/// This is the default for TUI/desktop: LLM API keys and `FILAR_SECRET_*`
/// are read from the process environment.
#[derive(Debug, Default, Clone, Copy)]
pub struct EnvSecretProvider;

impl EnvSecretProvider {
    /// Create a new `EnvSecretProvider`.
    pub fn new() -> Self {
        Self
    }
}

impl SecretProvider for EnvSecretProvider {
    fn get(&self, name: &str) -> Result<String> {
        // Secret variable names may start with `$` (e.g. `$FILAR_SECRET_1`),
        // but environment variables never have a `$` prefix.
        let env_name = name.strip_prefix('$').unwrap_or(name);
        std::env::var(env_name)
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| CoreError::Secret(format!("{} not set or empty", redact(name))))
    }

    fn secret_names(&self) -> Vec<String> {
        // Return all FILAR_SECRET_* env vars.
        std::env::vars()
            .filter(|(k, v)| k.starts_with("FILAR_SECRET_") && !v.is_empty())
            .map(|(k, _)| format!("${k}"))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// StaticSecretProvider
// ---------------------------------------------------------------------------

/// In-memory [`SecretProvider`] backed by a `HashMap`.
///
/// Designed for FFI, bots, tests, and GUI-launched sessions where secrets
/// are injected programmatically rather than read from env. Values are
/// zeroized when the last clone is dropped.
///
/// The internal `HashMap` is shared via `Arc<RwLock<…>>`, so clones see
/// mutations — `insert()` on one clone is visible to all.
#[derive(Debug, Clone)]
pub struct StaticSecretProvider {
    secrets: Arc<RwLock<HashMap<String, String>>>,
}

impl StaticSecretProvider {
    /// Create an empty provider.
    pub fn new() -> Self {
        Self {
            secrets: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Create a provider pre-loaded with the given secrets.
    pub fn with_secrets(secrets: HashMap<String, String>) -> Self {
        Self {
            secrets: Arc::new(RwLock::new(secrets)),
        }
    }

    /// Insert or replace a secret. Visible to all clones.
    ///
    /// If a secret with the same name already exists, the old value is
    /// zeroized before being dropped.
    pub fn insert(&self, name: impl Into<String>, value: impl Into<String>) {
        if let Ok(mut secrets) = self.secrets.write() {
            if let Some(mut old) = secrets.insert(name.into(), value.into()) {
                old.zeroize();
            }
        }
    }

    /// Remove a secret. Returns `true` if the secret existed.
    ///
    /// The removed value is zeroized internally — it is never returned to
    /// the caller in plaintext.
    pub fn remove(&self, name: &str) -> bool {
        if let Some(mut value) = self
            .secrets
            .write()
            .ok()
            .and_then(|mut s| s.remove(name))
        {
            value.zeroize();
            true
        } else {
            false
        }
    }
}

impl Default for StaticSecretProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretProvider for StaticSecretProvider {
    fn get(&self, name: &str) -> Result<String> {
        let secrets = self
            .secrets
            .read()
            .map_err(|_| CoreError::Other("secrets lock poisoned".into()))?;
        secrets
            .get(name)
            .filter(|v| !v.is_empty())
            .cloned()
            .ok_or_else(|| CoreError::Secret(format!("{} not set or empty", redact(name))))
    }

    fn secret_names(&self) -> Vec<String> {
        self.secrets
            .read()
            .map(|secrets| secrets.keys().cloned().collect())
            .unwrap_or_default()
    }
}

/// Zeroize all secret values when the last clone is dropped.
impl Drop for StaticSecretProvider {
    fn drop(&mut self) {
        // Only zeroize if this is the last clone (Arc count == 1).
        if let Some(rwlock) = Arc::get_mut(&mut self.secrets) {
            if let Ok(map) = rwlock.get_mut() {
                for value in map.values_mut() {
                    value.zeroize();
                }
                map.clear();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Redaction helper
// ---------------------------------------------------------------------------

/// Redact a potentially sensitive string for use in error messages.
///
/// Shows the first 4 characters and the total length, e.g. `"FILA… (len 17)"`.
/// Preserves diagnostics (you can tell which secret is missing) while ensuring
/// that a full secret value cannot appear in user-facing text even if passed by
/// mistake in place of a secret name.
///
/// # Examples
///
/// ```
/// use filar_core::secrets::redact;
/// assert_eq!(redact("FILAR_LLM_API_KEY"), "FILA… (len 17)".to_string());
/// assert_eq!(redact("sk-or-v1-abc123def456"), "sk-o… (len 21)".to_string());
/// ```
pub fn redact(s: &str) -> String {
    let prefix: String = s.chars().take(4).collect();
    let len = s.chars().count();
    format!("{prefix}… (len {len})")
}

/// Replace every known secret value in `text` with its placeholder.
///
/// The complement of [`redact`]: where that hides a name in an error message,
/// this removes *values* from free-form text that is about to leave the
/// machine — a transcript handed to an LLM so it can write a runbook (#401).
/// Command output is already sanitised by the executor, but a value the user
/// typed into the chat directly never passed through it.
///
/// A `$`-prefixed slot (`$FILAR_SECRET_N`) is restored to its own name — the
/// same form the operator sees in a command line, and the form the executor
/// substitutes. A plain name (an API key) is internal configuration with no
/// place in a document meant to be shared: both its value and occurrences of
/// the bare name itself become `<secret>` (#404 review).
///
/// Plain names and slot placeholders can overlap (`SECRET` inside
/// `$FILAR_SECRET_1`), so slot occurrences leave the text as control-character
/// markers first and come back once every pattern has run — a blind pass
/// would rewrite the placeholder itself (#404 review). Markers are built from
/// `\u{0}`/`\u{1}` precisely because patterns are printable strings: even a
/// marker like `__SECRET_SLOT_0__` would be eaten by the example above.
///
/// Patterns are replaced longest first, so one that contains another (`abc`
/// inside `abcdef`) cannot be rewritten before its own match is tried. Empty
/// values are skipped — replacing the empty string would mangle the text
/// outright.
pub fn redact_secrets(text: &str, provider: &dyn SecretProvider) -> String {
    let mut patterns: Vec<(usize, String, String)> = Vec::new();
    let mut slots: Vec<(String, String)> = Vec::new();
    for name in provider.secret_names() {
        let Ok(value) = provider.get(&name) else {
            continue;
        };
        if value.is_empty() {
            continue;
        }
        if name.starts_with('$') {
            slots.push((name, value));
        } else {
            // A plain name: neither the value nor the bare name may reach a
            // shared document — both are masked the same way. The name takes
            // part in the longest-first pass, so a value that overlaps it
            // cannot leave a mangled hybrid behind.
            patterns.push((value.len(), value, "<secret>".to_string()));
            patterns.push((name.len(), name, "<secret>".to_string()));
        }
    }

    // Mask slot occurrences longest-name-first — `$FILAR_SECRET_1` is a
    // prefix of `$FILAR_SECRET_11`, and the shorter name must not eat into
    // the longer one.
    slots.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
    let mut markers: Vec<(String, String)> = Vec::new();
    let mut out = text.to_string();
    for (name, value) in slots {
        // The `\u{0}` runs break containment, so no marker is a substring of
        // another and each restore is exact.
        let marker = format!(
            "\u{0}\u{0}\u{0}{}\u{0}\u{0}\u{0}",
            "\u{1}".repeat(markers.len() + 1)
        );
        out = out.replace(&name, &marker);
        patterns.push((value.len(), value, marker.clone()));
        markers.push((marker, name));
    }

    patterns.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, pattern, placeholder) in patterns {
        out = out.replace(&pattern, &placeholder);
    }
    for (marker, name) in markers {
        out = out.replace(&marker, &name);
    }
    out
}

// ---------------------------------------------------------------------------
// Legacy convenience functions (delegates to EnvSecretProvider)
// ---------------------------------------------------------------------------

/// The names an LLM API key stored for `key_env` is looked up under, in
/// order: `key_env` itself and, for the default name
/// [`env_vars::LLM_API_KEY`], the legacy [`env_vars::LEGACY_LLM_API_KEY`]
/// (#490).
pub fn llm_key_lookup_names(key_env: &str) -> Vec<&str> {
    if key_env == env_vars::LLM_API_KEY {
        vec![key_env, env_vars::LEGACY_LLM_API_KEY]
    } else {
        vec![key_env]
    }
}

/// Retrieve the LLM API key stored for `key_env` from `provider`, falling
/// back to the legacy name for the default one (see [`llm_key_lookup_names`]).
///
/// A hit under the legacy name logs a warning naming both variables, never
/// the value. Returns the error for `key_env` itself when nothing is found.
pub fn llm_api_key(provider: &dyn SecretProvider, key_env: &str) -> Result<String> {
    let names = llm_key_lookup_names(key_env);
    let mut first_err = None;
    for name in names {
        match provider.get(name) {
            Ok(key) => {
                if name != key_env {
                    tracing::warn!(
                        legacy = %name,
                        current = %key_env,
                        "LLM API key read from its legacy name; rename it"
                    );
                }
                return Ok(key);
            }
            Err(e) => {
                first_err.get_or_insert(e);
            }
        }
    }
    Err(first_err.unwrap_or_else(|| CoreError::Secret(format!("{key_env} is not set"))))
}

/// Retrieve the default LLM API key ([`env_vars::LLM_API_KEY`], or its
/// legacy name) from the environment.
///
/// Returns [`CoreError::Secret`] if neither variable is set.
pub fn default_llm_api_key() -> Result<String> {
    llm_api_key(&EnvSecretProvider::new(), env_vars::LLM_API_KEY)
}

/// Retrieve an API key from a named environment variable.
///
/// **Deprecated:** prefer [`SecretProvider::get`] with [`EnvSecretProvider`].
pub fn api_key(env_var: &str) -> Result<String> {
    EnvSecretProvider::new().get(env_var)
}

// ---------------------------------------------------------------------------
// SSH keyring key naming (GUI ↔ TUI contract)
// ---------------------------------------------------------------------------

/// Display / config name for an SSH launcher slot.
///
/// Empty `alias` → `SSH{slot+1}` (e.g. slot 0 → `"SSH1"`). Non-empty alias is
/// returned as-is. Must stay in sync with the GUI launcher and TUI runner.
pub fn ssh_target_display_name(slot: usize, alias: &str) -> String {
    if alias.is_empty() {
        format!("SSH{}", slot + 1)
    } else {
        alias.to_string()
    }
}

/// OS credential-store username for an SSH launcher slot password.
///
/// Format: `ssh_target:{name}` where `name` is [`ssh_target_display_name`].
/// Used by the GUI when saving, `main` on GUI→TUI handoff, and the TUI runner
/// on reconnect (`format!("ssh_target:{}", target.name)`).
pub fn ssh_cred_name(slot: usize, alias: &str) -> String {
    format!("ssh_target:{}", ssh_target_display_name(slot, alias))
}

/// OS credential-store username for the passphrase of a target's encrypted
/// key (#480): `ssh_key_passphrase:{name}`, `name` being the target name.
///
/// Separate from [`ssh_cred_name`] so a host's password and its key
/// passphrase never overwrite each other.
pub fn ssh_key_passphrase_name(target_name: &str) -> String {
    format!("ssh_key_passphrase:{target_name}")
}

// ---------------------------------------------------------------------------
// KeyringSecretProvider
// ---------------------------------------------------------------------------

/// [`SecretProvider`] backed by the OS credential store (via `keyring`).
///
/// Secret names are used directly as the credential entry username; the service
/// name is always `"filar"`.
///
/// `get()` returns [`CoreError::Secret`] when the store has no such entry,
/// and [`CoreError::Other`] when the store itself cannot be used (no
/// credential store on the platform, Secret Service unreachable, keychain
/// locked) — so a caller can tell "not stored" from "could not look" (#436).
/// Neither error carries the secret. `secret_names()` returns an empty list.
/// The provider never panics on keyring errors.
///
/// # Examples
///
/// ```no_run
/// use filar_core::secrets::{KeyringSecretProvider, SecretProvider};
/// let keyring = KeyringSecretProvider::new();
/// // This reads the credential stored under "api_key" in the "filar" service.
/// if let Ok(api_key) = keyring.get("api_key") {
///     println!("API key loaded from OS credential store");
/// }
/// ```
#[derive(Debug, Clone)]
pub struct KeyringSecretProvider;

impl KeyringSecretProvider {
    /// Create a new `KeyringSecretProvider`.
    pub fn new() -> Self {
        Self
    }
}

impl Default for KeyringSecretProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl SecretProvider for KeyringSecretProvider {
    fn get(&self, name: &str) -> Result<String> {
        // Failing to even build the entry (an invalid name, no backend) says
        // nothing about whether the secret is stored: not `Secret`.
        let entry = keyring::Entry::new("filar", name).map_err(|e| {
            CoreError::Other(format!("OS credential store entry error for {}: {e}", redact(name)))
        })?;
        entry.get_password().map_err(|e| match e {
            keyring::Error::NoEntry => {
                CoreError::Secret(format!("{} not in the OS credential store", redact(name)))
            }
            other => CoreError::Other(format!(
                "OS credential store unavailable for {}: {other}",
                redact(name)
            )),
        })
    }

    fn secret_names(&self) -> Vec<String> {
        // Keyring doesn't support listing entries — return empty.
        Vec::new()
    }
}

/// Save a secret to the OS credential store.
///
/// `username` is the logical name (e.g. `"api_key"`, `"ssh0"`). An empty
/// secret deletes the entry rather than storing an empty string.
pub fn save_secret_to_keyring(username: &str, secret: &str) {
    if secret.is_empty() {
        delete_secret_from_keyring(username);
        return;
    }
    match keyring::Entry::new("filar", username) {
        Ok(entry) => {
            if let Err(e) = entry.set_password(secret) {
                tracing::warn!(error = %e, "failed to save secret to credential store");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to create credential entry"),
    }
}

/// Delete a secret from the OS credential store.
pub fn delete_secret_from_keyring(username: &str) {
    if let Ok(entry) = keyring::Entry::new("filar", username) {
        let _ = entry.delete_credential();
    }
}

// ---------------------------------------------------------------------------
// ChainSecretProvider
// ---------------------------------------------------------------------------

/// A cached secret value together with the moment it expires.
struct CachedSecret {
    value: String,
    expires_at: Instant,
}

/// [`SecretProvider`] that consults an ordered chain of providers (#690).
///
/// The first provider returning a non-empty value wins; an empty answer is
/// treated as a miss and the next provider is tried. Resolved values are
/// cached for a configurable TTL so a slow provider (a password-manager CLI)
/// is not spawned on every lookup; cached values are zeroized when evicted,
/// replaced or dropped. A zero TTL disables caching.
///
/// `secret_names()` is the union of every provider's names plus the names
/// of unexpired cached values — `SecretSubstitutingExecutor` needs it to
/// mask command output even after a source stops reporting a name it once
/// resolved. Neither errors nor the hand-written [`Debug`] ever carry a
/// secret value.
///
/// Build one from `[secret_provider]` with [`ChainSecretProvider::from_config`],
/// or wrap an explicit list with [`ChainSecretProvider::new`].
pub struct ChainSecretProvider {
    providers: Vec<Arc<dyn SecretProvider>>,
    ttl: Duration,
    cache: Mutex<HashMap<String, CachedSecret>>,
}

impl ChainSecretProvider {
    /// Wrap `providers` into a chain with the given cache TTL. An empty
    /// chain is rejected: there must be no silent fallback to env.
    pub fn new(providers: Vec<Arc<dyn SecretProvider>>, ttl: Duration) -> Result<Self> {
        if providers.is_empty() {
            return Err(CoreError::Config(
                "secret provider chain must contain at least one provider".into(),
            ));
        }
        Ok(Self {
            providers,
            ttl,
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// Build the chain described by `[secret_provider]` (#690).
    ///
    /// `keyring`, `env`, `onepassword` and `bitwarden` map to
    /// [`KeyringSecretProvider`], [`EnvSecretProvider`],
    /// [`OnePasswordProvider`] and [`BitwardenProvider`]; the remaining
    /// password-manager kind is recognised by validation but its provider
    /// lands with the follow-up issues of milestone 3.1.0 — until then
    /// naming one fails with a clear error instead of silently skipping it.
    pub fn from_config(cfg: &crate::config::SecretProviderConfig) -> Result<Self> {
        cfg.validate()?;
        let mut providers: Vec<Arc<dyn SecretProvider>> = Vec::new();
        for kind in &cfg.order {
            let parsed = crate::config::SecretProviderKind::parse(kind).ok_or_else(|| {
                CoreError::Config(format!("secret_provider.order: unknown kind '{kind}'"))
            })?;
            let provider: Arc<dyn SecretProvider> = match parsed {
                crate::config::SecretProviderKind::Keyring => Arc::new(KeyringSecretProvider::new()),
                crate::config::SecretProviderKind::Env => Arc::new(EnvSecretProvider::new()),
                crate::config::SecretProviderKind::OnePassword => {
                    Arc::new(OnePasswordProvider::new(cfg.onepassword.items.clone()))
                }
                crate::config::SecretProviderKind::Bitwarden => {
                    Arc::new(BitwardenProvider::new(cfg.bitwarden.items.clone()))
                }
                crate::config::SecretProviderKind::KeePassXc => {
                    let database = cfg.keepassxc.database.clone().ok_or_else(|| {
                        CoreError::Config(
                            "secret_provider.keepassxc.database must be set when 'keepassxc' is in secret_provider.order"
                                .into(),
                        )
                    })?;
                    // The database password is itself a secret: resolve it
                    // through keyring → env under a fixed logical name, with
                    // no caching (TTL zero). A masked startup prompt can be
                    // injected instead via `with_password_provider` (#694).
                    let password_source = Arc::new(ChainSecretProvider::new(
                        vec![
                            Arc::new(KeyringSecretProvider::new()),
                            Arc::new(EnvSecretProvider::new()),
                        ],
                        Duration::ZERO,
                    )?);
                    Arc::new(
                        KeePassXcProvider::new(database, cfg.keepassxc.entries.clone())
                            .with_attribute(cfg.keepassxc.attribute.clone())
                            .with_password_provider(password_source, "FILAR_KEEPASSXC_DB_PASSWORD"),
                    )
                }
            };
            providers.push(provider);
        }
        Self::new(providers, Duration::from_secs(cfg.cache_ttl_secs))
    }

    /// Lock the cache, recovering from a poisoned lock: the cache holds
    /// only copies of secrets, so a panic elsewhere must not wedge lookups.
    fn cache(&self) -> std::sync::MutexGuard<'_, HashMap<String, CachedSecret>> {
        self.cache.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl SecretProvider for ChainSecretProvider {
    fn get(&self, name: &str) -> Result<String> {
        if !self.ttl.is_zero() {
            let mut cache = self.cache();
            if let Some(entry) = cache.get(name) {
                if entry.expires_at > Instant::now() {
                    return Ok(entry.value.clone());
                }
            }
            // Expired (or about to be replaced): zeroize before dropping.
            if let Some(mut old) = cache.remove(name) {
                old.value.zeroize();
            }
        }

        // Resolve without holding the lock: a slow provider must not block
        // other lookups, and a provider may itself consult the chain.
        for provider in &self.providers {
            if let Ok(value) = provider.get(name) {
                if !value.is_empty() {
                    if !self.ttl.is_zero() {
                        // An unrepresentable deadline (e.g. `Duration::MAX`)
                        // must not panic under the lock: skip caching and
                        // still return the provider's result.
                        if let Some(expires_at) = Instant::now().checked_add(self.ttl) {
                            let mut cache = self.cache();
                            if let Some(mut old) = cache.insert(
                                name.to_string(),
                                CachedSecret {
                                    value: value.clone(),
                                    expires_at,
                                },
                            ) {
                                old.value.zeroize();
                            }
                        }
                    }
                    return Ok(value);
                }
            }
        }
        Err(CoreError::Secret(format!(
            "{} not resolved by any of the {} secret providers",
            redact(name),
            self.providers.len()
        )))
    }

    fn secret_names(&self) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        let mut names = Vec::new();
        for provider in &self.providers {
            for name in provider.secret_names() {
                if seen.insert(name.clone()) {
                    names.push(name);
                }
            }
        }
        // Cached values stay returnable until they expire, so their names
        // must stay visible to output masking even when the source no
        // longer reports them.
        let now = Instant::now();
        for (name, entry) in self.cache().iter() {
            if entry.expires_at > now && seen.insert(name.clone()) {
                names.push(name.clone());
            }
        }
        names
    }
}

/// Hand-written like [`SshAuth`](crate::config::SshAuth): counts only, so a
/// `{chain:?}` in a log line can never print a cached value.
impl std::fmt::Debug for ChainSecretProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ChainSecretProvider")
            .field("providers", &self.providers.len())
            .field("cache_ttl_secs", &self.ttl.as_secs())
            .field("cached", &self.cache().len())
            .finish()
    }
}

/// Zeroize every cached value when the chain is dropped.
impl Drop for ChainSecretProvider {
    fn drop(&mut self) {
        if let Ok(cache) = self.cache.get_mut() {
            for entry in cache.values_mut() {
                entry.value.zeroize();
            }
            cache.clear();
        }
    }
}

// ---------------------------------------------------------------------------
// Password-manager CLI helpers
// ---------------------------------------------------------------------------

/// Default timeout for a password-manager CLI invocation, seconds (#691).
pub const DEFAULT_SECRET_CLI_TIMEOUT_SECS: u64 = 30;

/// Upper bound on password-manager CLI output, per stream. Secret values
/// are small; the fixed pre-reserved capacity guarantees the buffer never
/// reallocates, so no freed copy of a secret is left unwiped (#733 review).
/// Output beyond the limit is an error, never a truncated secret.
const MAX_SECRET_CLI_OUTPUT_BYTES: usize = 1024 * 1024;

/// Partial pipe output shared with a reader thread. The abort path wipes
/// `data` under the same lock the reader copies each chunk into, so a
/// partial secret is zeroized immediately on timeout — even while the
/// reader stays blocked in `read()` on a pipe an orphaned grandchild keeps
/// open. The reader keeps no secret bytes of its own between reads.
/// The capacity is reserved once in `new()` and never grows.
struct PipeBuffer {
    data: Vec<u8>,
    done: bool,
    overflow: bool,
    error: bool,
}

impl PipeBuffer {
    fn new() -> Self {
        Self {
            data: Vec::with_capacity(MAX_SECRET_CLI_OUTPUT_BYTES),
            done: false,
            overflow: false,
            error: false,
        }
    }
}

/// Wipe the secret bytes even if the buffer is dropped without an explicit
/// zeroize (panic, future refactor).
impl Drop for PipeBuffer {
    fn drop(&mut self) {
        self.data.zeroize();
    }
}

/// Set the abort flag and wipe whatever the pipe readers already copied.
/// A reader that returns from `read()` later sees the flag under the same
/// lock and wipes its chunk, so no partial output survives unzeroized.
/// `done` is set too: an aborted buffer must never be awaited or read as
/// a valid result.
fn abort_pipe_readers(
    aborted: &std::sync::atomic::AtomicBool,
    buffers: &[&Mutex<PipeBuffer>],
) {
    aborted.store(true, std::sync::atomic::Ordering::SeqCst);
    for buf in buffers {
        let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
        b.data.zeroize();
        b.done = true;
    }
}

/// Read a pipe chunk by chunk into the shared buffer, checking the abort
/// flag under the buffer lock after every chunk. While blocked in `read()`
/// the thread holds no secret data: the previous chunk was wiped right
/// after being copied. The thread may linger until EOF, but never with a
/// secret in its own memory. `Interrupted` reads are retried; any other
/// read error is recorded so a truncated value is never returned as the
/// secret. Thread creation is fallible: the caller must not leave an
/// already-spawned child unmanaged on failure.
fn spawn_pipe_reader<R: std::io::Read + Send + 'static>(
    pipe: Option<R>,
    buf: Arc<Mutex<PipeBuffer>>,
    aborted: Arc<std::sync::atomic::AtomicBool>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("filar-secret-cli-reader".to_string())
        .spawn(move || {
            if let Some(mut pipe) = pipe {
                let mut chunk = [0u8; 8192];
                loop {
                    let n = match pipe.read(&mut chunk) {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => {
                            buf.lock().unwrap_or_else(|e| e.into_inner()).error = true;
                            break;
                        }
                    };
                    let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                    if aborted.load(std::sync::atomic::Ordering::SeqCst) {
                        chunk[..n].zeroize();
                        break;
                    }
                    if b.data.len() + n > MAX_SECRET_CLI_OUTPUT_BYTES {
                        b.overflow = true;
                        chunk[..n].zeroize();
                        break;
                    }
                    // Within the capacity reserved in PipeBuffer::new():
                    // no reallocation, no freed unwiped copy of the secret.
                    b.data.extend_from_slice(&chunk[..n]);
                    chunk[..n].zeroize();
                }
            }
            buf.lock().unwrap_or_else(|e| e.into_inner()).done = true;
        })?;
    Ok(())
}

/// Run a password-manager CLI and return its stdout verbatim (the caller
/// passes flags like `--no-newline` so nothing is added; a stored secret
/// may itself end with a newline). The child is killed when `timeout`
/// elapses; both output pipes are drained while it runs, so a value larger
/// than the pipe capacity cannot deadlock the child into a false timeout.
///
/// Errors name the provider and the (non-secret) reference, never a value
/// and never the raw stderr: the stderr is only classified into a short
/// hint by `classify`. All intermediate buffers are zeroized.
fn run_secret_cli(
    cli: &str,
    args: &[&str],
    timeout: Duration,
    provider: &str,
    reference: &str,
    classify: impl Fn(&str) -> Option<&'static str>,
) -> Result<String> {
    run_secret_cli_with_env(cli, args, timeout, provider, reference, classify, &[])
}

/// [`run_secret_cli`] with extra environment for the child only (e.g. the
/// Bitwarden session key `BW_SESSION`, #692). The values come from env or
/// the OS credential store at the call site; they never enter the config,
/// the logs or the error messages.
fn run_secret_cli_with_env(
    cli: &str,
    args: &[&str],
    timeout: Duration,
    provider: &str,
    reference: &str,
    classify: impl Fn(&str) -> Option<&'static str>,
    env: &[(&str, &str)],
) -> Result<String> {
    run_secret_cli_inner(cli, args, timeout, provider, reference, classify, env, None)
}

/// [`run_secret_cli`] that feeds one secret line to the child's stdin and
/// closes the pipe (KeePassXC reads the database password this way, #693).
/// No temporary file is created; the writer's copy of the secret is
/// zeroized on every path.
fn run_secret_cli_with_stdin(
    cli: &str,
    args: &[&str],
    timeout: Duration,
    provider: &str,
    reference: &str,
    classify: impl Fn(&str) -> Option<&'static str>,
    stdin_secret: &str,
) -> Result<String> {
    run_secret_cli_inner(
        cli,
        args,
        timeout,
        provider,
        reference,
        classify,
        &[],
        Some(stdin_secret),
    )
}

/// Shared body of the CLI runners: spawn, drain both output pipes, enforce
/// the deadline, optionally feed one secret line to stdin. `classify` turns
/// lowercased stderr into a short hint; raw stderr never reaches the error.
fn run_secret_cli_inner(
    cli: &str,
    args: &[&str],
    timeout: Duration,
    provider: &str,
    reference: &str,
    classify: impl Fn(&str) -> Option<&'static str>,
    env: &[(&str, &str)],
    stdin_secret: Option<&str>,
) -> Result<String> {
    use std::process::{Command, Stdio};

    // An unrepresentable deadline must fail before the child starts:
    // dropping a `Child` does not terminate the subprocess.
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        CoreError::Config(format!("{provider}: timeout is too large to represent"))
    })?;

    let mut child = Command::new(cli)
        .args(args)
        .envs(env.iter().copied())
        .stdin(if stdin_secret.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                CoreError::Secret(format!(
                    "{provider}: CLI '{cli}' not found — install it or remove the provider from secret_provider.order"
                ))
            } else {
                CoreError::Other(format!("{provider}: failed to start CLI '{cli}': {e}"))
            }
        })?;

    // Drain both pipes concurrently: a child writing more than the pipe
    // capacity would block forever with nobody reading (#733 review).
    // Readers copy chunks into shared buffers; on abort the flag is set
    // and the buffers wiped under the same lock, so partial output is
    // zeroized immediately — no EOF wait, no secret left in a blocked
    // reader's own memory.
    let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let out_buf: Arc<Mutex<PipeBuffer>> = Arc::new(Mutex::new(PipeBuffer::new()));
    let err_buf: Arc<Mutex<PipeBuffer>> = Arc::new(Mutex::new(PipeBuffer::new()));
    spawn_pipe_reader(child.stdout.take(), Arc::clone(&out_buf), Arc::clone(&aborted))
        .and_then(|_| {
            spawn_pipe_reader(child.stderr.take(), Arc::clone(&err_buf), Arc::clone(&aborted))
        })
        .map_err(|e| {
            // A reader thread could not start: stop whatever did start,
            // terminate and reap the child, never leave it unmanaged.
            abort_pipe_readers(&aborted, &[&out_buf, &err_buf]);
            let _ = child.kill();
            let _ = child.wait();
            CoreError::Other(format!(
                "{provider}: failed to start output reader for CLI '{cli}': {e}"
            ))
        })?;

    // Feed the secret line on a dedicated thread: a password larger than
    // the pipe capacity must not block the deadline loop. Closing the pipe
    // afterwards gives the CLI EOF. The thread owns a `Zeroizing` copy, so
    // the secret is wiped on every path — including a failed spawn, where
    // the closure (and its payload) is dropped without running.
    if let Some(secret) = stdin_secret {
        let mut stdin = child.stdin.take();
        let mut payload = zeroize::Zeroizing::new(Vec::with_capacity(secret.len() + 1));
        payload.extend_from_slice(secret.as_bytes());
        payload.push(b'\n');
        let writer = std::thread::Builder::new()
            .name("filar-secret-cli-writer".to_string())
            .spawn(move || {
                if let Some(pipe) = stdin.as_mut() {
                    use std::io::Write;
                    // A broken pipe only means the child exited early; its
                    // exit status and stderr govern the outcome.
                    let _ = pipe.write_all(&payload);
                    let _ = pipe.flush();
                }
            });
        if let Err(e) = writer {
            abort_pipe_readers(&aborted, &[&out_buf, &err_buf]);
            let _ = child.kill();
            let _ = child.wait();
            return Err(CoreError::Other(format!(
                "{provider}: failed to start stdin writer for CLI '{cli}': {e}"
            )));
        }
    }

    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    abort_pipe_readers(&aborted, &[&out_buf, &err_buf]);
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(CoreError::Secret(format!(
                        "{provider}: timed out after {}s reading {reference}",
                        timeout.as_secs()
                    )));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                abort_pipe_readers(&aborted, &[&out_buf, &err_buf]);
                let _ = child.kill();
                let _ = child.wait();
                return Err(CoreError::Other(format!(
                    "{provider}: failed to wait for CLI '{cli}': {e}"
                )));
            }
        }
    };

    // The child exited, but an orphaned grandchild may still hold a pipe
    // open (Windows .cmd stubs): wait for the readers no longer than the
    // same deadline, never an unbounded join.
    loop {
        let out_done = out_buf.lock().unwrap_or_else(|e| e.into_inner()).done;
        let err_done = err_buf.lock().unwrap_or_else(|e| e.into_inner()).done;
        if out_done && err_done {
            break;
        }
        if Instant::now() >= deadline {
            abort_pipe_readers(&aborted, &[&out_buf, &err_buf]);
            return Err(CoreError::Secret(format!(
                "{provider}: timed out after {}s reading {reference}",
                timeout.as_secs()
            )));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let overflow = out_buf.lock().unwrap_or_else(|e| e.into_inner()).overflow
        || err_buf.lock().unwrap_or_else(|e| e.into_inner()).overflow;
    if overflow {
        abort_pipe_readers(&aborted, &[&out_buf, &err_buf]);
        return Err(CoreError::Secret(format!(
            "{provider}: CLI output exceeds the {MAX_SECRET_CLI_OUTPUT_BYTES}-byte limit reading {reference}"
        )));
    }
    // A failed read must never pass a truncated value off as the secret.
    let read_error = out_buf.lock().unwrap_or_else(|e| e.into_inner()).error
        || err_buf.lock().unwrap_or_else(|e| e.into_inner()).error;
    if read_error {
        abort_pipe_readers(&aborted, &[&out_buf, &err_buf]);
        return Err(CoreError::Other(format!(
            "{provider}: failed to read CLI output reading {reference}"
        )));
    }
    let mut stdout_bytes = std::mem::take(&mut out_buf.lock().unwrap_or_else(|e| e.into_inner()).data);
    let mut stderr_bytes = std::mem::take(&mut err_buf.lock().unwrap_or_else(|e| e.into_inner()).data);
    let mut stdout = String::from_utf8_lossy(&stdout_bytes).into_owned();
    let mut stderr = String::from_utf8_lossy(&stderr_bytes).into_owned();
    stdout_bytes.zeroize();
    stderr_bytes.zeroize();

    if status.success() {
        stderr.zeroize();
        Ok(stdout)
    } else {
        let mut normalized = stderr.to_lowercase();
        let hint = classify(&normalized).unwrap_or("CLI read failed");
        normalized.zeroize();
        stderr.zeroize();
        stdout.zeroize();
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".to_string());
        Err(CoreError::Secret(format!(
            "{provider}: {hint} (item {reference}, exit {code})"
        )))
    }
}

// ---------------------------------------------------------------------------
// OnePasswordProvider
// ---------------------------------------------------------------------------

/// [`SecretProvider`] backed by the 1Password CLI (`op read`, #691).
///
/// Logical names (`FILAR_LLM_API_KEY`, `ssh_target:<name>`,
/// `ssh_key_passphrase:<name>`) map to `op://<vault>/<item>/<field>`
/// references through `[secret_provider.onepassword.items]`. The CLI runs
/// on the user's machine, so zero-install on the remote (invariant 1) is
/// not affected. The trait is synchronous: async callers must wrap `get()`
/// in `tokio::task::spawn_blocking`, never call it on a runtime worker.
///
/// A locked vault or a missing session (`OP_SESSION`, service account) is
/// reported as a clear error naming the provider and the item reference —
/// never a value and never the raw stderr. Value buffers are zeroized;
/// caching lives in [`ChainSecretProvider`], not here.
pub struct OnePasswordProvider {
    items: HashMap<String, String>,
    cli: String,
    timeout: Duration,
}

impl OnePasswordProvider {
    /// Create a provider from the logical-name → `op://` mapping. Uses the
    /// `op` binary from `PATH` and [`DEFAULT_SECRET_CLI_TIMEOUT_SECS`].
    pub fn new(items: HashMap<String, String>) -> Self {
        Self {
            items,
            cli: "op".to_string(),
            timeout: Duration::from_secs(DEFAULT_SECRET_CLI_TIMEOUT_SECS),
        }
    }

    /// Override the CLI path (tests use a stub script).
    pub fn with_cli_path(mut self, path: impl Into<String>) -> Self {
        self.cli = path.into();
        self
    }

    /// Override the per-invocation timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

impl SecretProvider for OnePasswordProvider {
    fn get(&self, name: &str) -> Result<String> {
        let reference = self.items.get(name).ok_or_else(|| {
            CoreError::Secret(format!("onepassword: no op:// mapping for {}", redact(name)))
        })?;
        run_secret_cli(
            &self.cli,
            &["read", reference, "--no-newline"],
            self.timeout,
            "onepassword",
            reference,
            |stderr| {
                if stderr.contains("not signed in")
                    || stderr.contains("session expired")
                    || stderr.contains("session has expired")
                    || stderr.contains("sign in")
                    || stderr.contains("authentication")
                    || stderr.contains("locked")
                {
                    Some(
                        "vault is locked or there is no active session — sign in with 'op signin' or provide OP_SESSION / a service account token",
                    )
                } else if stderr.contains("isn't an item")
                    || stderr.contains("not found")
                    || stderr.contains("could not be found")
                    || stderr.contains("no item")
                    || stderr.contains("no vault")
                {
                    Some("item or vault not found")
                } else {
                    None
                }
            },
        )
    }

    fn secret_names(&self) -> Vec<String> {
        self.items.keys().cloned().collect()
    }
}

/// Counts only — the provider holds no values, but keep the log line lean.
impl std::fmt::Debug for OnePasswordProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnePasswordProvider")
            .field("items", &self.items.len())
            .field("timeout_secs", &self.timeout.as_secs())
            .finish()
    }
}

// ---------------------------------------------------------------------------
// BitwardenProvider
// ---------------------------------------------------------------------------

/// [`SecretProvider`] backed by the local Bitwarden CLI (`bw`, #692).
///
/// Logical names map to Bitwarden item ids via
/// `[secret_provider.bitwarden.items]`; a value is read with
/// `bw get password <id>`. The vault state is checked with `bw status`
/// first: a locked vault fails with «vault locked: выполните bw unlock»,
/// an unauthenticated one with a 'bw login' hint — never an interactive
/// prompt inside the TUI loop.
///
/// The session key `BW_SESSION` is taken from the process environment or
/// the OS credential store (service `filar`, name `BW_SESSION`) — never
/// from `config.toml` — and passed to the child process only.
///
/// `bw get password` writes the value verbatim when stdout is piped (the
/// CLI's `writeLn` skips its own newline for a final line on a non-TTY),
/// so the captured stdout is returned unchanged — a stored password that
/// ends with a line break keeps it.
///
/// Errors name the provider and the item id, never a value and never the
/// raw stderr. Value buffers are zeroized; caching lives in
/// [`ChainSecretProvider`]. The trait is synchronous: async callers must
/// use `tokio::task::spawn_blocking`.
pub struct BitwardenProvider {
    items: HashMap<String, String>,
    cli: String,
    timeout: Duration,
    #[cfg(test)]
    session_override: Option<String>,
}

impl BitwardenProvider {
    /// Create a provider from the logical-name → item-id mapping. Uses the
    /// `bw` binary from `PATH` and [`DEFAULT_SECRET_CLI_TIMEOUT_SECS`].
    pub fn new(items: HashMap<String, String>) -> Self {
        Self {
            items,
            cli: "bw".to_string(),
            timeout: Duration::from_secs(DEFAULT_SECRET_CLI_TIMEOUT_SECS),
            #[cfg(test)]
            session_override: None,
        }
    }

    /// Override the CLI path (tests use a stub script).
    pub fn with_cli_path(mut self, path: impl Into<String>) -> Self {
        self.cli = path.into();
        self
    }

    /// Override the per-invocation timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Test-only: inject the session key directly so a test can verify the
    /// session reaches the child without mutating the process environment
    /// (parallel tests share it, and concurrent env access is unsound).
    #[cfg(test)]
    fn with_session(mut self, session: impl Into<String>) -> Self {
        self.session_override = Some(session.into());
        self
    }

    /// Check the vault state, then read the item. Split out of `get` so the
    /// session key is zeroized on every exit path.
    fn read_with_status(&self, reference: &str, env: &[(&str, &str)]) -> Result<String> {
        let mut status_out = run_secret_cli_with_env(
            &self.cli,
            &["status"],
            self.timeout,
            "bitwarden",
            reference,
            classify_bw_stderr,
            env,
        )?;
        let state = serde_json::from_str::<serde_json::Value>(&status_out)
            .ok()
            .filter(|v| v.is_object())
            .and_then(|v| v.get("status").and_then(|s| s.as_str()).map(str::to_string));
        status_out.zeroize();
        match state.as_deref() {
            Some("unlocked") => {}
            Some("locked") => {
                return Err(CoreError::Secret(format!(
                    "bitwarden: vault locked: выполните bw unlock (item {reference})"
                )));
            }
            Some("unauthenticated") => {
                return Err(CoreError::Secret(format!(
                    "bitwarden: not logged in — run 'bw login' (item {reference})"
                )));
            }
            _ => {
                return Err(CoreError::Other(format!(
                    "bitwarden: unexpected 'bw status' output — expected a JSON object with a 'status' field (item {reference})"
                )));
            }
        }
        let value = run_secret_cli_with_env(
            &self.cli,
            &["get", "password", reference],
            self.timeout,
            "bitwarden",
            reference,
            classify_bw_stderr,
            env,
        )?;
        // Piped `bw get password` adds no newline of its own: return the
        // value verbatim, including any stored terminal line break.
        Ok(value)
    }
}

impl SecretProvider for BitwardenProvider {
    fn get(&self, name: &str) -> Result<String> {
        let reference = self.items.get(name).ok_or_else(|| {
            CoreError::Secret(format!("bitwarden: no bw item mapping for {}", redact(name)))
        })?;
        #[cfg(test)]
        let mut session = self.session_override.clone().or_else(resolve_bw_session);
        #[cfg(not(test))]
        let mut session = resolve_bw_session();
        let env: Vec<(&str, &str)> = session
            .as_deref()
            .map(|s| vec![("BW_SESSION", s)])
            .unwrap_or_default();
        let result = self.read_with_status(reference, &env);
        if let Some(s) = session.as_mut() {
            s.zeroize();
        }
        result
    }

    fn secret_names(&self) -> Vec<String> {
        self.items.keys().cloned().collect()
    }
}

/// Counts only — the provider holds no values, but keep the log line lean.
impl std::fmt::Debug for BitwardenProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BitwardenProvider")
            .field("items", &self.items.len())
            .field("timeout_secs", &self.timeout.as_secs())
            .finish()
    }
}

/// Resolve `BW_SESSION` from the environment or the OS credential store —
/// never from the config (#692). A keyring failure means "no session", not
/// an error: `bw status` reports the vault state either way. The caller
/// zeroizes the returned value; the OS environment block itself cannot be
/// wiped, which is inherent to env-based secrets.
fn resolve_bw_session() -> Option<String> {
    if let Ok(v) = std::env::var("BW_SESSION") {
        if !v.is_empty() {
            return Some(v);
        }
    }
    let entry = match keyring::Entry::new("filar", "BW_SESSION") {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("bitwarden: OS credential store unavailable for BW_SESSION: {e}");
            return None;
        }
    };
    match entry.get_password() {
        Ok(v) if !v.is_empty() => Some(v),
        Ok(_) => None,
        Err(keyring::Error::NoEntry) => None,
        Err(e) => {
            tracing::debug!(
                "bitwarden: could not read BW_SESSION from the OS credential store: {e}"
            );
            None
        }
    }
}

/// Classify `bw` stderr into a short hint; the raw text never reaches the
/// error message. Matches the lowercased stderr.
fn classify_bw_stderr(stderr: &str) -> Option<&'static str> {
    if stderr.contains("locked") {
        Some("vault locked: выполните bw unlock")
    } else if stderr.contains("not logged in") || stderr.contains("unauthenticated") {
        Some("not logged in — run 'bw login'")
    } else if stderr.contains("not found") {
        Some("item not found")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// KeePassXcProvider
// ---------------------------------------------------------------------------

/// [`SecretProvider`] backed by the local KeePassXC CLI
/// (`keepassxc-cli show -a <attribute> <db> <entry>`, #693).
///
/// The database path and the attribute come from
/// `[secret_provider.keepassxc]`; logical names map to entry paths through
/// `entries`. The database password is a secret and never enters the
/// config: it is resolved through an injected [`SecretProvider`] —
/// `from_config` wires keyring → env under the logical name
/// `FILAR_KEEPASSXC_DB_PASSWORD`; a masked startup prompt can be injected
/// as a [`StaticSecretProvider`] instead. The password is fed to the CLI's
/// stdin (its prompt goes to stderr) — no temporary file is created — and
/// zeroized after the call.
///
/// `show -a` appends exactly one newline of its own (`Qt::endl`); precisely
/// that trailing line break (`\n` or `\r\n`) is stripped, a newline stored
/// in the value itself survives.
///
/// Errors name the provider and the entry path, never a password value and
/// never the raw stderr. Value buffers are zeroized; caching lives in
/// [`ChainSecretProvider`]. The trait is synchronous: async callers must
/// use `tokio::task::spawn_blocking`.
pub struct KeePassXcProvider {
    database: std::path::PathBuf,
    attribute: String,
    entries: HashMap<String, String>,
    cli: String,
    timeout: Duration,
    password_provider: Option<Arc<dyn SecretProvider>>,
    password_name: String,
}

impl KeePassXcProvider {
    /// Create a provider for a database and a logical-name → entry-path
    /// mapping. Reads the `password` attribute via `keepassxc-cli` from
    /// `PATH` with [`DEFAULT_SECRET_CLI_TIMEOUT_SECS`]; no password source
    /// is configured yet — add one with [`Self::with_password_provider`].
    pub fn new(database: impl Into<std::path::PathBuf>, entries: HashMap<String, String>) -> Self {
        Self {
            database: database.into(),
            attribute: "password".to_string(),
            entries,
            cli: "keepassxc-cli".to_string(),
            timeout: Duration::from_secs(DEFAULT_SECRET_CLI_TIMEOUT_SECS),
            password_provider: None,
            password_name: "FILAR_KEEPASSXC_DB_PASSWORD".to_string(),
        }
    }

    /// Override the entry attribute to read (`show -a`).
    pub fn with_attribute(mut self, attribute: impl Into<String>) -> Self {
        self.attribute = attribute.into();
        self
    }

    /// Override the CLI path (tests use a stub script).
    pub fn with_cli_path(mut self, path: impl Into<String>) -> Self {
        self.cli = path.into();
        self
    }

    /// Override the per-invocation timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Set where the database password comes from: a [`SecretProvider`]
    /// (keyring/env, or a [`StaticSecretProvider`] holding a masked startup
    /// input) queried under `name`. The password never enters the config.
    pub fn with_password_provider(
        mut self,
        provider: Arc<dyn SecretProvider>,
        name: impl Into<String>,
    ) -> Self {
        self.password_provider = Some(provider);
        self.password_name = name.into();
        self
    }

    /// Resolve the database password through the configured source. The
    /// error names the logical name only — never a value; the underlying
    /// provider error goes to the debug log (provider errors carry names,
    /// not values, by contract).
    fn resolve_db_password(&self) -> Result<String> {
        let provider = self.password_provider.as_ref().ok_or_else(|| {
            CoreError::Secret(
                "keepassxc: no database password source configured — store the password in the OS credential store/env or enter it at startup"
                    .into(),
            )
        })?;
        provider.get(&self.password_name).map_err(|e| {
            tracing::debug!(
                "keepassxc: database password '{}' unavailable: {e}",
                self.password_name
            );
            CoreError::Secret(format!(
                "keepassxc: database password '{}' is not available — store it in the OS credential store/env or enter it at startup",
                self.password_name
            ))
        })
    }

    /// Run `show -a` for one entry. Split out of `get` so the password is
    /// zeroized on every exit path.
    fn read_entry(&self, db: &str, entry: &str, password: &str) -> Result<String> {
        let mut value = run_secret_cli_with_stdin(
            &self.cli,
            &["show", "-a", &self.attribute, db, entry],
            self.timeout,
            "keepassxc",
            entry,
            classify_keepassxc_stderr,
            password,
        )?;
        // Strip exactly the one line break `show -a` appends (Qt::endl);
        // a newline that is part of the stored value itself survives.
        if value.ends_with('\n') {
            value.pop();
            if value.ends_with('\r') {
                value.pop();
            }
        }
        Ok(value)
    }
}

impl SecretProvider for KeePassXcProvider {
    fn get(&self, name: &str) -> Result<String> {
        let entry = self.entries.get(name).ok_or_else(|| {
            CoreError::Secret(format!("keepassxc: no entry mapping for {}", redact(name)))
        })?;
        let db = self.database.to_str().ok_or_else(|| {
            CoreError::Config("keepassxc: database path is not valid UTF-8".into())
        })?;
        let mut password = self.resolve_db_password()?;
        let result = self.read_entry(db, entry, &password);
        password.zeroize();
        result
    }

    fn secret_names(&self) -> Vec<String> {
        self.entries.keys().cloned().collect()
    }
}

/// Counts and non-secret parameters only — never the password, never a
/// value; the database path is not secret but keeps the log line lean.
impl std::fmt::Debug for KeePassXcProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KeePassXcProvider")
            .field("entries", &self.entries.len())
            .field("attribute", &self.attribute)
            .field("timeout_secs", &self.timeout.as_secs())
            .field("password_source", &self.password_provider.is_some())
            .finish()
    }
}

/// Classify `keepassxc-cli` stderr into a short hint; the raw text never
/// reaches the error message. Matches the lowercased stderr.
fn classify_keepassxc_stderr(stderr: &str) -> Option<&'static str> {
    if stderr.contains("invalid credentials") {
        Some("wrong database password")
    } else if stderr.contains("could not find entry") {
        Some("entry not found")
    } else if stderr.contains("unknown attribute") {
        Some("unknown attribute")
    } else if stderr.contains("is ambiguous") {
        Some("attribute name is ambiguous")
    } else if stderr.contains("not readable") {
        Some("database file not readable")
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_provider_get_returns_env_var() {
        // SAFETY: env var manipulation is not thread-safe, but this test
        // uses a unique name unlikely to collide with other tests.
        let name = "FILAR_TEST_SECRET_ENV_42";
        std::env::set_var(name, "test-value");
        let provider = EnvSecretProvider::new();
        assert_eq!(provider.get(name).unwrap(), "test-value");
        std::env::remove_var(name);
    }

    #[test]
    fn env_provider_get_missing_returns_error() {
        let provider = EnvSecretProvider::new();
        assert!(provider.get("FILAR_NONEXISTENT_SECRET_99999").is_err());
    }

    #[test]
    fn env_provider_get_strips_dollar_prefix() {
        let name = "FILAR_TEST_SECRET_DOLLAR";
        std::env::set_var(name, "dollar-value");
        let provider = EnvSecretProvider::new();
        // `$FILAR_TEST_SECRET_DOLLAR` should resolve to env var `FILAR_TEST_SECRET_DOLLAR`.
        assert_eq!(provider.get(&format!("${name}")).unwrap(), "dollar-value");
        std::env::remove_var(name);
    }

    #[test]
    fn static_provider_insert_and_get() {
        let provider = StaticSecretProvider::new();
        provider.insert("API_KEY", "abc123");
        assert_eq!(provider.get("API_KEY").unwrap(), "abc123");
    }

    #[test]
    fn static_provider_get_missing_returns_error() {
        let provider = StaticSecretProvider::new();
        assert!(provider.get("MISSING").is_err());
    }

    #[test]
    fn static_provider_empty_value_returns_error() {
        let provider = StaticSecretProvider::new();
        provider.insert("EMPTY", "");
        assert!(provider.get("EMPTY").is_err());
    }

    #[test]
    fn static_provider_secret_names() {
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "pass1");
        provider.insert("$FILAR_SECRET_2", "pass2");
        provider.insert("GLM_API_KEY", "key");

        let mut names = provider.secret_names();
        names.sort();
        assert_eq!(
            names,
            vec!["$FILAR_SECRET_1", "$FILAR_SECRET_2", "GLM_API_KEY"]
        );
    }

    #[test]
    fn static_provider_remove() {
        let provider = StaticSecretProvider::new();
        provider.insert("SECRET", "value");
        assert!(provider.remove("SECRET"));
        assert!(provider.get("SECRET").is_err());
        // Removing again returns false.
        assert!(!provider.remove("SECRET"));
    }

    #[test]
    fn static_provider_clone_shares_state() {
        let provider = StaticSecretProvider::new();
        let clone = provider.clone();
        provider.insert("KEY", "val");
        // Clone sees the mutation.
        assert_eq!(clone.get("KEY").unwrap(), "val");
    }

    #[test]
    fn static_provider_overwrite() {
        let provider = StaticSecretProvider::new();
        provider.insert("KEY", "old");
        provider.insert("KEY", "new");
        assert_eq!(provider.get("KEY").unwrap(), "new");
    }

    #[test]
    fn static_provider_with_secrets() {
        let mut map = HashMap::new();
        map.insert("KEY1".into(), "val1".into());
        map.insert("KEY2".into(), "val2".into());
        let provider = StaticSecretProvider::with_secrets(map);
        assert_eq!(provider.get("KEY1").unwrap(), "val1");
        assert_eq!(provider.get("KEY2").unwrap(), "val2");
    }

    #[test]
    fn env_provider_secret_names_scans_env() {
        // SAFETY: uses unique names unlikely to collide.
        std::env::set_var("FILAR_SECRET_TEST_A", "alpha");
        std::env::set_var("FILAR_SECRET_TEST_B", "beta");
        let provider = EnvSecretProvider::new();
        let names = provider.secret_names();
        assert!(names.contains(&"$FILAR_SECRET_TEST_A".to_string()));
        assert!(names.contains(&"$FILAR_SECRET_TEST_B".to_string()));
        std::env::remove_var("FILAR_SECRET_TEST_A");
        std::env::remove_var("FILAR_SECRET_TEST_B");
    }

    // ── Redaction tests (#184) ─────────────────────────────────────────

    #[test]
    fn redact_normal_name_keeps_prefix_and_shows_length() {
        let result = redact("GLM_API_KEY");
        assert_eq!(result, "GLM_… (len 11)");
    }

    #[test]
    fn redact_short_name_shows_all_chars() {
        let result = redact("ab");
        assert_eq!(result, "ab… (len 2)");
    }

    #[test]
    fn error_message_does_not_contain_secret_value() {
        let key_value = "sk-or-v1-1234567890abcdef1234567890abcdef";
        let provider = StaticSecretProvider::new();
        // Mistakenly pass the key value as if it were the name.
        let err = provider.get(key_value).unwrap_err();
        let msg = format!("{err}");
        assert!(
            !msg.contains(key_value),
            "error must NOT contain the full key value '{key_value}', got: {msg}"
        );
        // The redacted form (prefix + length) should be present.
        assert!(
            msg.contains("sk-o"),
            "error must contain the redacted prefix, got: {msg}"
        );
    }

    #[test]
    fn env_provider_redacted_on_missing_secret() {
        let name = "sk-or-v1-leaked-key-12345678";
        let provider = EnvSecretProvider::new();
        let err = provider.get(name).unwrap_err();
        let msg = format!("{err}");
        let expected_prefix = redact(name);
        assert!(!msg.contains(name));
        assert!(msg.contains(&expected_prefix));
    }

    #[test]
    fn static_provider_redacted_on_missing_secret() {
        let name = "another-secret-key-that-could-leak";
        let provider = StaticSecretProvider::new();
        let err = provider.get(name).unwrap_err();
        let msg = format!("{err}");
        let expected_prefix = redact(name);
        assert!(!msg.contains(name));
        assert!(msg.contains(&expected_prefix));
    }

    // ── redact_secrets (value redaction for outgoing text, #401) ──────

    #[test]
    fn redact_secrets_replaces_values_with_their_placeholders() {
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "hunter2");
        let text = "Step 1: ssh with password hunter2 and check disks.";
        let out = redact_secrets(text, &provider);
        assert!(!out.contains("hunter2"), "value must be gone: {out}");
        assert!(
            out.contains("$FILAR_SECRET_1"),
            "placeholder must stand in for the value: {out}"
        );
        assert!(out.contains("Step 1:"), "the rest of the text survives: {out}");
    }

    #[test]
    fn redact_secrets_masks_plain_names_and_values_as_secret() {
        // An API key is stored without a `$` prefix; neither its value nor
        // the bare name may leak into a shared document (#404 review).
        let provider = StaticSecretProvider::new();
        provider.insert("GLM_API_KEY", "sk-or-v1-abcdef");
        let out = redact_secrets("export GLM_API_KEY=sk-or-v1-abcdef", &provider);
        assert!(!out.contains("sk-or-v1-abcdef"), "value must be gone: {out}");
        assert!(!out.contains("GLM_API_KEY"), "the name must not leak: {out}");
        assert_eq!(out, "export <secret>=<secret>");
    }

    #[test]
    fn redact_secrets_masks_a_plain_name_referenced_as_a_variable() {
        // `$GLM_API_KEY` names the variable, not the value — the name itself
        // must not survive into the shared document either.
        let provider = StaticSecretProvider::new();
        provider.insert("GLM_API_KEY", "sk-or-v1-abcdef");
        let out = redact_secrets("echo $GLM_API_KEY", &provider);
        assert_eq!(out, "echo $<secret>");
    }

    #[test]
    fn redact_secrets_keeps_placeholders_out_of_the_plain_name_pass() {
        // A plain name can be a substring of a slot name (`SECRET` inside
        // `$FILAR_SECRET_1`): the placeholder must survive intact while the
        // standalone occurrences of the name are still masked (#404 review).
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "hunter2");
        provider.insert("SECRET", "zzz");
        let out = redact_secrets("SECRET=zzz and ssh with hunter2, keep $FILAR_SECRET_1", &provider);
        assert_eq!(
            out,
            "<secret>=<secret> and ssh with $FILAR_SECRET_1, keep $FILAR_SECRET_1"
        );
    }

    #[test]
    fn redact_secrets_masks_slot_names_longest_first() {
        // `$FILAR_SECRET_1` is a prefix of `$FILAR_SECRET_11`; the shorter
        // name must not eat into the longer one's placeholder.
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "first");
        provider.insert("$FILAR_SECRET_11", "eleventh");
        let out = redact_secrets("use $FILAR_SECRET_11 then $FILAR_SECRET_1", &provider);
        assert_eq!(out, "use $FILAR_SECRET_11 then $FILAR_SECRET_1");
    }

    #[test]
    fn redact_secrets_prefers_the_longer_overlapping_value() {
        // Replacing `abc` first would also rewrite the inside of `abcdef`
        // and leave a mangled hybrid behind.
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "abc");
        provider.insert("$FILAR_SECRET_2", "abcdef");
        let out = redact_secrets("value abcdef", &provider);
        assert_eq!(out, "value $FILAR_SECRET_2");
    }

    #[test]
    fn redact_secrets_leaves_text_without_secrets_alone() {
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "hunter2");
        let text = "User: check disk usage on the replica";
        assert_eq!(redact_secrets(text, &provider), text);
    }

    #[test]
    fn redact_secrets_keeps_existing_placeholders() {
        // The transcript normally contains the placeholder, not the value:
        // the executor substitutes at run time. Redaction must not touch it.
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "hunter2");
        let text = "mysql -u root -p$FILAR_SECRET_1";
        assert_eq!(redact_secrets(text, &provider), text);
    }

    // ── SSH keyring naming (#290) ─────────────────────────────────────

    #[test]
    fn ssh_cred_name_empty_alias_uses_slot() {
        assert_eq!(ssh_cred_name(0, ""), "ssh_target:SSH1");
        assert_eq!(ssh_cred_name(4, ""), "ssh_target:SSH5");
        assert_eq!(ssh_target_display_name(0, ""), "SSH1");
    }

    #[test]
    fn ssh_cred_name_nonempty_alias_uses_alias() {
        assert_eq!(ssh_cred_name(0, "VPS DE"), "ssh_target:VPS DE");
        assert_eq!(ssh_cred_name(2, "prod-web"), "ssh_target:prod-web");
        assert_eq!(ssh_target_display_name(2, "prod-web"), "prod-web");
    }

    #[test]
    fn ssh_cred_name_special_chars_preserved() {
        assert_eq!(ssh_cred_name(0, "my server!"), "ssh_target:my server!");
        assert_eq!(ssh_cred_name(1, "сервер"), "ssh_target:сервер");
    }

    #[test]
    fn key_passphrase_entry_is_separate_from_the_password_entry() {
        assert_eq!(ssh_key_passphrase_name("prod-web"), "ssh_key_passphrase:prod-web");
        assert_ne!(ssh_key_passphrase_name("prod-web"), ssh_cred_name(0, "prod-web"));
    }

    #[test]
    fn the_default_llm_key_falls_back_to_its_legacy_name() {
        let provider = StaticSecretProvider::new();
        provider.insert("GLM_API_KEY", "legacy-key");
        assert_eq!(llm_api_key(&provider, env_vars::LLM_API_KEY).unwrap(), "legacy-key");
        provider.insert("FILAR_LLM_API_KEY", "new-key");
        assert_eq!(llm_api_key(&provider, env_vars::LLM_API_KEY).unwrap(), "new-key");
    }

    #[test]
    fn a_named_llm_key_has_no_fallback() {
        let provider = StaticSecretProvider::new();
        provider.insert("GLM_API_KEY", "legacy-key");
        assert!(llm_api_key(&provider, "OPENROUTER_API_KEY").is_err());
        assert_eq!(llm_key_lookup_names("OPENROUTER_API_KEY"), vec!["OPENROUTER_API_KEY"]);
    }

    // ── ChainSecretProvider (#690) ────────────────────────────────────

    /// A provider that counts `get` calls, to observe the cache.
    struct CountingProvider {
        inner: StaticSecretProvider,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl CountingProvider {
        fn new() -> Self {
            Self {
                inner: StaticSecretProvider::new(),
                calls: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    impl SecretProvider for CountingProvider {
        fn get(&self, name: &str) -> Result<String> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.get(name)
        }

        fn secret_names(&self) -> Vec<String> {
            self.inner.secret_names()
        }
    }

    /// A provider that answers `Ok("")` — an empty answer must be a miss.
    struct EmptyOkProvider;

    impl SecretProvider for EmptyOkProvider {
        fn get(&self, _name: &str) -> Result<String> {
            Ok(String::new())
        }

        fn secret_names(&self) -> Vec<String> {
            Vec::new()
        }
    }

    fn chain(providers: Vec<Arc<dyn SecretProvider>>, ttl_secs: u64) -> ChainSecretProvider {
        ChainSecretProvider::new(providers, Duration::from_secs(ttl_secs)).unwrap()
    }

    #[test]
    fn chain_first_non_empty_answer_wins() {
        let first = StaticSecretProvider::new();
        first.insert("KEY", "from-first");
        let second = StaticSecretProvider::new();
        second.insert("KEY", "from-second");
        let c = chain(vec![Arc::new(first), Arc::new(second)], 0);
        assert_eq!(c.get("KEY").unwrap(), "from-first");
    }

    #[test]
    fn chain_falls_through_to_the_next_provider() {
        let first = StaticSecretProvider::new();
        let second = StaticSecretProvider::new();
        second.insert("KEY", "from-second");
        let c = chain(vec![Arc::new(first), Arc::new(second)], 0);
        assert_eq!(c.get("KEY").unwrap(), "from-second");
    }

    #[test]
    fn chain_treats_an_empty_answer_as_a_miss() {
        let second = StaticSecretProvider::new();
        second.insert("KEY", "real");
        let c = chain(vec![Arc::new(EmptyOkProvider), Arc::new(second)], 0);
        assert_eq!(c.get("KEY").unwrap(), "real");
    }

    #[test]
    fn chain_empty_providers_rejected() {
        assert!(ChainSecretProvider::new(Vec::new(), Duration::from_secs(60)).is_err());
    }

    #[test]
    fn chain_caches_within_ttl() {
        let counting = Arc::new(CountingProvider::new());
        counting.inner.insert("KEY", "cached-value");
        let calls = counting.calls.clone();
        let c = chain(vec![counting], 3600);
        assert_eq!(c.get("KEY").unwrap(), "cached-value");
        assert_eq!(c.get("KEY").unwrap(), "cached-value");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn chain_zero_ttl_disables_caching() {
        let counting = Arc::new(CountingProvider::new());
        counting.inner.insert("KEY", "v");
        let calls = counting.calls.clone();
        let c = chain(vec![counting], 0);
        let _ = c.get("KEY");
        let _ = c.get("KEY");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn chain_expired_entry_is_resolved_again() {
        let counting = Arc::new(CountingProvider::new());
        counting.inner.insert("KEY", "old");
        let calls = counting.calls.clone();
        let c = ChainSecretProvider::new(vec![counting.clone()], Duration::from_millis(20)).unwrap();
        assert_eq!(c.get("KEY").unwrap(), "old");
        std::thread::sleep(Duration::from_millis(60));
        counting.inner.insert("KEY", "new");
        assert_eq!(c.get("KEY").unwrap(), "new");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[test]
    fn chain_secret_names_is_the_deduplicated_union() {
        let first = StaticSecretProvider::new();
        first.insert("$FILAR_SECRET_1", "a");
        first.insert("SHARED", "b");
        let second = StaticSecretProvider::new();
        second.insert("SHARED", "c");
        second.insert("$FILAR_SECRET_2", "d");
        let c = chain(vec![Arc::new(first), Arc::new(second)], 0);
        let mut names = c.secret_names();
        names.sort();
        assert_eq!(names, vec!["$FILAR_SECRET_1", "$FILAR_SECRET_2", "SHARED"]);
    }

    #[test]
    fn chain_miss_error_carries_no_value() {
        let provider = StaticSecretProvider::new();
        provider.insert("OTHER", "sk-or-v1-super-secret-value");
        let c = chain(vec![Arc::new(provider)], 0);
        let err = c.get("sk-or-v1-super-secret-value").unwrap_err();
        let msg = format!("{err}");
        assert!(!msg.contains("sk-or-v1-super-secret-value"), "leak: {msg}");
        assert!(msg.contains("sk-o"), "redacted prefix expected: {msg}");
    }

    #[test]
    fn chain_debug_carries_no_values() {
        let provider = StaticSecretProvider::new();
        provider.insert("KEY", "sk-or-v1-debug-leak-check");
        let c = chain(vec![Arc::new(provider)], 3600);
        let _ = c.get("KEY"); // populate the cache
        let dbg = format!("{c:?}");
        assert!(!dbg.contains("sk-or-v1-debug-leak-check"), "leak: {dbg}");
        assert!(dbg.contains("ChainSecretProvider"));
    }

    #[test]
    fn chain_from_config_builds_keyring_and_env() {
        let cfg = crate::config::SecretProviderConfig {
            order: vec!["keyring".into(), "env".into()],
            ..Default::default()
        };
        let c = ChainSecretProvider::from_config(&cfg).unwrap();
        // env is the second link: a unique env var resolves through it.
        std::env::set_var("FILAR_TEST_CHAIN_690", "chain-env-value");
        assert_eq!(c.get("FILAR_TEST_CHAIN_690").unwrap(), "chain-env-value");
        std::env::remove_var("FILAR_TEST_CHAIN_690");
    }

    #[test]
    fn chain_from_config_rejects_empty_order() {
        let cfg = crate::config::SecretProviderConfig::default();
        let err = ChainSecretProvider::from_config(&cfg).unwrap_err();
        assert!(format!("{err}").contains("secret_provider.order"));
    }

    #[test]
    fn chain_from_config_rejects_unknown_kind() {
        let cfg = crate::config::SecretProviderConfig {
            order: vec!["lastpass".into()],
            ..Default::default()
        };
        let err = ChainSecretProvider::from_config(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("secret_provider.order"), "{msg}");
        assert!(msg.contains("lastpass"), "{msg}");
    }

    #[test]
    fn chain_from_config_keepassxc_requires_database() {
        // All five kinds now build; keepassxc without a database path is a
        // config error naming the field, not a silent skip.
        let cfg = crate::config::SecretProviderConfig {
            order: vec!["keepassxc".into()],
            ..Default::default()
        };
        let err = ChainSecretProvider::from_config(&cfg).unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("keepassxc.database"), "{msg}");
    }

    #[test]
    fn chain_max_ttl_does_not_panic_on_an_unrepresentable_deadline() {
        let provider = StaticSecretProvider::new();
        provider.insert("KEY", "v");
        let c = ChainSecretProvider::new(vec![Arc::new(provider)], Duration::MAX).unwrap();
        // Instant + Duration::MAX overflows: the value is returned uncached.
        assert_eq!(c.get("KEY").unwrap(), "v");
        assert_eq!(c.get("KEY").unwrap(), "v");
    }

    #[test]
    fn chain_secret_names_keeps_unexpired_cached_names() {
        // A cached value stays returnable after its source removes the
        // name; output masking must still see it (#732 review).
        let provider = StaticSecretProvider::new();
        provider.insert("$FILAR_SECRET_1", "hunter2");
        let c = chain(vec![Arc::new(provider.clone())], 3600);
        assert_eq!(c.get("$FILAR_SECRET_1").unwrap(), "hunter2");
        assert!(provider.remove("$FILAR_SECRET_1"));
        assert!(c.secret_names().contains(&"$FILAR_SECRET_1".to_string()));
        assert_eq!(c.get("$FILAR_SECRET_1").unwrap(), "hunter2");
        let out = redact_secrets("ssh with hunter2", &c);
        assert!(!out.contains("hunter2"), "cached value must be masked: {out}");
        assert!(out.contains("$FILAR_SECRET_1"), "{out}");
    }

    // -----------------------------------------------------------------------
    // OnePasswordProvider (#691) — CLI stub tests
    // -----------------------------------------------------------------------

    fn op_stub_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("filar-op-stub-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_stub(dir: &std::path::Path, name: &str, body: &str) -> std::path::PathBuf {
        #[cfg(windows)]
        let path = dir.join(format!("{name}.cmd"));
        #[cfg(not(windows))]
        let path = dir.join(name);
        std::fs::write(&path, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        path
    }

    /// Stub `op`: the reference (second argument) selects the behaviour —
    /// "locked" → sign-in error, "missing" → not-found error, "newline" → a
    /// value whose stored form ends with a line break, anything else → a
    /// value printed verbatim (the stub honours `--no-newline`).
    fn op_stub(dir: &std::path::Path) -> std::path::PathBuf {
        #[cfg(windows)]
        let body = concat!(
            "@echo off\r\n",
            "echo %2 | find \"locked\" >nul\r\n",
            "if not errorlevel 1 (\r\n",
            "  echo [ERROR] You are not signed in. Your session has expired. 1>&2\r\n",
            "  exit /b 1\r\n",
            ")\r\n",
            "echo %2 | find \"missing\" >nul\r\n",
            "if not errorlevel 1 (\r\n",
            "  echo [ERROR] item isn't an item in the vault 1>&2\r\n",
            "  exit /b 1\r\n",
            ")\r\n",
            "echo %2 | find \"newline\" >nul\r\n",
            "if not errorlevel 1 (\r\n",
            "  echo op-secret-value\r\n",
            "  exit /b 0\r\n",
            ")\r\n",
            "<nul set /p=op-secret-value\r\n",
            "exit /b 0\r\n",
        );
        #[cfg(not(windows))]
        let body = concat!(
            "#!/bin/sh\n",
            "case \"$2\" in\n",
            "*locked*) printf '%s\\n' \"[ERROR] You are not signed in. Your session has expired.\" >&2; exit 1 ;;\n",
            "*missing*) printf '%s\\n' \"[ERROR] item isn't an item in the vault\" >&2; exit 1 ;;\n",
            "*newline*) printf 'op-secret-value\\n' ;;\n",
            "*) printf 'op-secret-value' ;;\n",
            "esac\n",
        );
        write_stub(dir, "op-stub", body)
    }

    /// Stub `op` writing more than the pipe capacity (≈200 KB) before it
    /// exits — the provider must drain the pipes, not deadlock (#733 review).
    fn op_big_stub(dir: &std::path::Path) -> std::path::PathBuf {
        #[cfg(windows)]
        let body = concat!(
            "@echo off\r\n",
            "setlocal enabledelayedexpansion\r\n",
            "set LINE=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n",
            "for /L %%i in (1,1,2000) do echo !LINE!\r\n",
            "exit /b 0\r\n",
        );
        #[cfg(not(windows))]
        let body = "#!/bin/sh\nawk 'BEGIN{for(i=0;i<200000;i++)printf \"a\"}'\n";
        write_stub(dir, "op-big", body)
    }

    fn op_slow_stub(dir: &std::path::Path) -> std::path::PathBuf {
        #[cfg(windows)]
        let body = "@echo off\r\nping -n 30 127.0.0.1 >nul\r\nexit /b 0\r\n";
        #[cfg(not(windows))]
        let body = "#!/bin/sh\nsleep 30\n";
        write_stub(dir, "op-slow", body)
    }

    fn op_items(reference: &str) -> HashMap<String, String> {
        let mut items = HashMap::new();
        items.insert("FILAR_LLM_API_KEY".to_string(), reference.to_string());
        items
    }

    #[test]
    fn op_provider_reads_value_via_cli_stub() {
        let dir = op_stub_dir("read");
        let stub = op_stub(&dir);
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path(stub.to_str().unwrap());
        // The stub honours --no-newline; stdout is returned verbatim.
        assert_eq!(p.get("FILAR_LLM_API_KEY").unwrap(), "op-secret-value");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_preserves_a_terminal_newline_of_the_stored_secret() {
        // A stored password may itself end with a line break; --no-newline
        // only suppresses the newline the CLI would add (#733 review).
        let dir = op_stub_dir("newline");
        let stub = op_stub(&dir);
        let p = OnePasswordProvider::new(op_items("op://vault/newline-item/credential"))
            .with_cli_path(stub.to_str().unwrap());
        #[cfg(windows)]
        let expected = "op-secret-value\r\n";
        #[cfg(not(windows))]
        let expected = "op-secret-value\n";
        assert_eq!(p.get("FILAR_LLM_API_KEY").unwrap(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_large_output_does_not_deadline() {
        // More than the pipe capacity: the pipes must be drained while the
        // CLI runs, or the child blocks and gets a false timeout.
        let dir = op_stub_dir("big");
        let stub = op_big_stub(&dir);
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path(stub.to_str().unwrap())
            .with_timeout(Duration::from_secs(20));
        let value = p.get("FILAR_LLM_API_KEY").unwrap();
        assert!(value.len() > 100_000, "expected >100KB, got {}", value.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_unrepresentable_timeout_fails_before_spawning() {
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path("filar-nonexistent-op-cli-991")
            .with_timeout(Duration::MAX);
        // The deadline check runs before spawn: the error is about the
        // timeout, not about the missing CLI.
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("too large"), "{msg}");
    }

    #[test]
    fn op_provider_unmapped_name_is_a_miss() {
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"));
        let err = p.get("ssh_target:other").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("onepassword"), "{msg}");
        assert!(msg.contains("no op:// mapping"), "{msg}");
    }

    #[test]
    fn op_provider_locked_vault_error_names_provider_and_item() {
        let dir = op_stub_dir("locked");
        let stub = op_stub(&dir);
        let p = OnePasswordProvider::new(op_items("op://vault/locked-item/credential"))
            .with_cli_path(stub.to_str().unwrap());
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("onepassword"), "{msg}");
        assert!(msg.contains("op://vault/locked-item/credential"), "{msg}");
        assert!(msg.contains("session"), "{msg}");
        // Neither the raw stderr nor a value may leak into the error.
        assert!(!msg.contains("[ERROR]"), "raw stderr leak: {msg}");
        assert!(!msg.contains("op-secret-value"), "value leak: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_missing_item_error() {
        let dir = op_stub_dir("missing");
        let stub = op_stub(&dir);
        let p = OnePasswordProvider::new(op_items("op://vault/missing-item/credential"))
            .with_cli_path(stub.to_str().unwrap());
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "{msg}");
        assert!(msg.contains("op://vault/missing-item/credential"), "{msg}");
        assert!(!msg.contains("[ERROR]"), "raw stderr leak: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_timeout_kills_the_cli() {
        let dir = op_stub_dir("slow");
        let stub = op_slow_stub(&dir);
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path(stub.to_str().unwrap())
            .with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "timeout not enforced");
        let msg = format!("{err}");
        assert!(msg.contains("timed out"), "{msg}");
        assert!(msg.contains("onepassword"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_grandchild_holding_pipe_is_bounded_by_deadline() {
        // The direct child exits at once, but a background grandchild keeps
        // the stderr pipe open: the post-exit wait for the readers must
        // obey the deadline instead of blocking on an unbounded join.
        let dir = op_stub_dir("grandchild");
        #[cfg(windows)]
        let body = "@echo off\r\nstart /b ping -n 30 127.0.0.1 >nul\r\nexit /b 0\r\n";
        #[cfg(not(windows))]
        let body = "#!/bin/sh\nsleep 30 &\nexit 0\n";
        let stub = write_stub(&dir, "op-grandchild", body);
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path(stub.to_str().unwrap())
            .with_timeout(Duration::from_millis(500));
        let started = Instant::now();
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "post-exit wait not bounded");
        assert!(format!("{err}").contains("timed out"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn op_provider_timeout_wipes_output_stored_before_abort() {
        // The stdout reader finishes (the stub closes its stdout) while the
        // CLI keeps running: at timeout the already-stored buffer must be
        // wiped by the abort path, not dropped unzeroized (#733 review).
        let dir = op_stub_dir("preabort");
        // cat in a pipeline guarantees the partial value reaches the pipe
        // before stdout is closed (a bare printf could stay buffered).
        let body = "#!/bin/sh\nprintf 'partial-secret' | cat\nexec 1>&-\nsleep 30\n";
        let stub = write_stub(&dir, "op-preabort", body);
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path(stub.to_str().unwrap())
            .with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "timeout not enforced");
        let msg = format!("{err}");
        assert!(msg.contains("timed out"), "{msg}");
        assert!(!msg.contains("partial-secret"), "value leak: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn abort_pipe_readers_wipes_partial_data_without_eof() {
        // The reader is still blocked in read() (done = false): the abort
        // path must wipe the shared partial data right away, without
        // waiting for the pipe to close (#733 review, ai-review blocker).
        let aborted = std::sync::atomic::AtomicBool::new(false);
        let buf = Mutex::new(PipeBuffer {
            data: b"partial-secret".to_vec(),
            done: false,
            overflow: false,
            error: false,
        });
        abort_pipe_readers(&aborted, &[&buf]);
        assert!(aborted.load(std::sync::atomic::Ordering::SeqCst));
        let b = buf.lock().unwrap();
        // zeroize wipes the bytes and truncates the Vec.
        assert!(b.data.is_empty(), "partial data not zeroized");
        // An aborted buffer is marked done: it must never be awaited.
        assert!(b.done);
    }

    #[test]
    fn pipe_reader_caps_output_without_realloc() {
        // The capacity is reserved once: filling past the limit sets the
        // overflow flag instead of growing (and freeing) the allocation,
        // so no unwiped copy of the secret is left behind (#733 review).
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let buf = Arc::new(Mutex::new(PipeBuffer::new()));
        spawn_pipe_reader(
            Some(std::io::repeat(b'x')),
            Arc::clone(&buf),
            Arc::clone(&aborted),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if buf.lock().unwrap().done {
                break;
            }
            assert!(Instant::now() < deadline, "reader did not stop at the cap");
            std::thread::sleep(Duration::from_millis(5));
        }
        let b = buf.lock().unwrap();
        assert!(b.overflow, "overflow flag not set");
        assert_eq!(b.data.len(), MAX_SECRET_CLI_OUTPUT_BYTES);
        assert_eq!(b.data.capacity(), MAX_SECRET_CLI_OUTPUT_BYTES, "buffer reallocated");
    }

    #[test]
    fn pipe_reader_retries_interrupted_and_records_fatal_errors() {
        // A read error must not pass for EOF: Interrupted is retried, any
        // other error is recorded so run_secret_cli rejects the invocation
        // instead of returning a truncated secret (#733 review).
        struct FlakyReader {
            calls: usize,
        }
        impl std::io::Read for FlakyReader {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                self.calls += 1;
                match self.calls {
                    1 => Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "retry me")),
                    2 => {
                        buf[..4].copy_from_slice(b"tail");
                        Ok(4)
                    }
                    3 => Err(std::io::Error::new(std::io::ErrorKind::Other, "pipe broke")),
                    _ => Ok(0),
                }
            }
        }
        let aborted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let buf = Arc::new(Mutex::new(PipeBuffer::new()));
        spawn_pipe_reader(
            Some(FlakyReader { calls: 0 }),
            Arc::clone(&buf),
            Arc::clone(&aborted),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if buf.lock().unwrap().done {
                break;
            }
            assert!(Instant::now() < deadline, "reader did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        let b = buf.lock().unwrap();
        assert!(b.error, "fatal read error not recorded");
        // The Interrupted read was retried: the tail chunk was collected.
        assert_eq!(&b.data[..], b"tail");
    }

    #[test]
    fn op_provider_timeout_with_partial_output_and_held_pipe() {
        // The CLI writes part of the value, then keeps the pipe open past
        // the deadline: the call must return promptly and the error must
        // not carry the partial value.
        let dir = op_stub_dir("partial");
        #[cfg(windows)]
        let body = "@echo off\r\necho partial-secret\r\nping -n 30 127.0.0.1 >nul\r\nexit /b 0\r\n";
        #[cfg(not(windows))]
        // The pipeline subshell flushes on exit and cat writes to the pipe
        // immediately: a bare builtin printf could stay buffered and the
        // reader would never see the partial value (#733 review).
        let body = "#!/bin/sh\nprintf 'partial-secret' | cat\nsleep 30\n";
        let stub = write_stub(&dir, "op-partial", body);
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path(stub.to_str().unwrap())
            .with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "timeout not enforced");
        let msg = format!("{err}");
        assert!(msg.contains("timed out"), "{msg}");
        assert!(!msg.contains("partial-secret"), "value leak: {msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_missing_cli_is_a_clear_error() {
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path("filar-nonexistent-op-cli-991");
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("onepassword"), "{msg}");
        assert!(msg.contains("not found"), "{msg}");
    }

    #[test]
    fn op_provider_debug_carries_no_values_or_references() {
        let dir = op_stub_dir("debug");
        let stub = op_stub(&dir);
        let p = OnePasswordProvider::new(op_items("op://vault/api-key/credential"))
            .with_cli_path(stub.to_str().unwrap());
        let _ = p.get("FILAR_LLM_API_KEY").unwrap();
        let dbg = format!("{p:?}");
        assert!(dbg.contains("OnePasswordProvider"), "{dbg}");
        assert!(!dbg.contains("op-secret-value"), "value leak: {dbg}");
        assert!(!dbg.contains("credential"), "reference leak: {dbg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_provider_secret_names_are_the_mapping_keys() {
        let mut items = op_items("op://vault/api-key/credential");
        items.insert("ssh_target:prod".into(), "op://vault/prod/password".into());
        let p = OnePasswordProvider::new(items);
        let mut names = p.secret_names();
        names.sort();
        assert_eq!(names, vec!["FILAR_LLM_API_KEY", "ssh_target:prod"]);
    }

    #[test]
    fn chain_from_config_builds_onepassword() {
        let mut cfg = crate::config::SecretProviderConfig {
            order: vec!["onepassword".into()],
            ..Default::default()
        };
        cfg.onepassword.items = op_items("op://vault/api-key/credential");
        // Building the chain must not spawn the CLI.
        let c = ChainSecretProvider::from_config(&cfg).unwrap();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("providers: 1"), "{dbg}");
        assert!(c.secret_names().contains(&"FILAR_LLM_API_KEY".to_string()));
    }

    #[test]
    #[ignore = "needs 1Password CLI (op) with an active session; set FILAR_TEST_OP_REFERENCE"]
    fn op_provider_real_cli_integration() {
        let reference = std::env::var("FILAR_TEST_OP_REFERENCE")
            .expect("set FILAR_TEST_OP_REFERENCE to an op:// reference");
        let p = OnePasswordProvider::new(op_items(&reference));
        let value = p.get("FILAR_LLM_API_KEY").unwrap();
        assert!(!value.is_empty());
    }

    // -----------------------------------------------------------------------
    // BitwardenProvider (#692)
    // -----------------------------------------------------------------------

    fn bw_items(id: &str) -> HashMap<String, String> {
        let mut items = HashMap::new();
        items.insert("FILAR_LLM_API_KEY".to_string(), id.to_string());
        items
    }

    /// Stub `bw`: `status` reports an unlocked vault; `get password <id>`
    /// prints the value verbatim, like the real CLI on a pipe. The id
    /// "missing-item" selects the not-found branch, "newline-item" a stored
    /// value that itself ends with a line break.
    fn bw_stub(dir: &std::path::Path) -> std::path::PathBuf {
        #[cfg(windows)]
        let body = concat!(
            "@echo off\r\n",
            "if \"%1\"==\"status\" (\r\n",
            "  <nul set /p={\"status\":\"unlocked\"}\r\n",
            "  exit /b 0\r\n",
            ")\r\n",
            "if \"%3\"==\"missing-item\" (\r\n",
            "  echo Not found. 1>&2\r\n",
            "  exit /b 1\r\n",
            ")\r\n",
            "<nul set /p=bw-secret-value\r\n",
            "if \"%3\"==\"newline-item\" echo.\r\n",
            "exit /b 0\r\n",
        );
        #[cfg(not(windows))]
        let body = concat!(
            "#!/bin/sh\n",
            "if [ \"$1\" = \"status\" ]; then\n",
            "  printf '{\"status\":\"unlocked\"}'\n",
            "  exit 0\n",
            "fi\n",
            "if [ \"$3\" = \"missing-item\" ]; then\n",
            "  echo \"Not found.\" >&2\n",
            "  exit 1\n",
            "fi\n",
            "printf 'bw-secret-value'\n",
            "if [ \"$3\" = \"newline-item\" ]; then printf '\\n'; fi\n",
        );
        write_stub(dir, "bw", body)
    }

    /// Stub `bw` with a vault in the given state ("locked" or
    /// "unauthenticated"); `get` fails on stderr like the real CLI.
    fn bw_state_stub(dir: &std::path::Path, state: &str) -> std::path::PathBuf {
        #[cfg(windows)]
        let body = format!(
            "@echo off\r\nif \"%1\"==\"status\" (\r\n  <nul set /p={{\"status\":\"{state}\"}}\r\n  exit /b 0\r\n)\r\necho Vault is locked. marker-stderr-xyz 1>&2\r\nexit /b 1\r\n"
        );
        #[cfg(not(windows))]
        let body = format!(
            "#!/bin/sh\nif [ \"$1\" = \"status\" ]; then\n  printf '{{\"status\":\"{state}\"}}'\n  exit 0\nfi\necho \"Vault is locked. marker-stderr-xyz\" >&2\nexit 1\n"
        );
        write_stub(dir, "bw-state", &body)
    }

    #[test]
    fn bw_provider_reads_value_via_cli_stub() {
        let dir = op_stub_dir("bw-read");
        let stub = bw_stub(&dir);
        let p = BitwardenProvider::new(bw_items("api-key-item"))
            .with_cli_path(stub.to_str().unwrap());
        // Piped bw adds no newline: stdout is returned verbatim.
        assert_eq!(p.get("FILAR_LLM_API_KEY").unwrap(), "bw-secret-value");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bw_provider_preserves_a_stored_terminal_newline() {
        let dir = op_stub_dir("bw-newline");
        let stub = bw_stub(&dir);
        let p = BitwardenProvider::new(bw_items("newline-item"))
            .with_cli_path(stub.to_str().unwrap());
        // The stored terminal line break is part of the value and survives.
        #[cfg(windows)]
        let expected = "bw-secret-value\r\n";
        #[cfg(not(windows))]
        let expected = "bw-secret-value\n";
        assert_eq!(p.get("FILAR_LLM_API_KEY").unwrap(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bw_provider_locked_vault_error_names_provider_and_item() {
        let dir = op_stub_dir("bw-locked");
        let stub = bw_state_stub(&dir, "locked");
        let p = BitwardenProvider::new(bw_items("api-key-item"))
            .with_cli_path(stub.to_str().unwrap());
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bitwarden"), "{msg}");
        assert!(msg.contains("api-key-item"), "{msg}");
        assert!(msg.contains("vault locked"), "{msg}");
        assert!(msg.contains("bw unlock"), "{msg}");
        // The raw stderr never reaches the error.
        assert!(!msg.contains("marker-stderr-xyz"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bw_provider_unauthenticated_error_points_to_bw_login() {
        let dir = op_stub_dir("bw-unauth");
        let stub = bw_state_stub(&dir, "unauthenticated");
        let p = BitwardenProvider::new(bw_items("api-key-item"))
            .with_cli_path(stub.to_str().unwrap());
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bitwarden"), "{msg}");
        assert!(msg.contains("bw login"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bw_provider_missing_item_error() {
        let dir = op_stub_dir("bw-missing");
        let stub = bw_stub(&dir);
        let p = BitwardenProvider::new(bw_items("missing-item"))
            .with_cli_path(stub.to_str().unwrap());
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bitwarden"), "{msg}");
        assert!(msg.contains("missing-item"), "{msg}");
        assert!(msg.contains("not found"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bw_provider_timeout_kills_the_cli() {
        let dir = op_stub_dir("bw-slow");
        #[cfg(windows)]
        let body = "@echo off\r\nping -n 30 127.0.0.1 >nul\r\nexit /b 0\r\n";
        #[cfg(not(windows))]
        let body = "#!/bin/sh\nsleep 30\n";
        let stub = write_stub(&dir, "bw-slow", body);
        let p = BitwardenProvider::new(bw_items("api-key-item"))
            .with_cli_path(stub.to_str().unwrap())
            .with_timeout(Duration::from_millis(300));
        let started = Instant::now();
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "timeout not enforced");
        let msg = format!("{err}");
        assert!(msg.contains("timed out"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bw_provider_passes_session_to_the_cli_child() {
        // The stub echoes BW_SESSION back: the session must reach the child
        // through its environment, not from any config field. The key is
        // injected test-only — the process environment is never mutated,
        // so parallel tests cannot race on it (#734 review).
        let dir = op_stub_dir("bw-session");
        #[cfg(windows)]
        let body = concat!(
            "@echo off\r\n",
            "if \"%1\"==\"status\" (\r\n",
            "  <nul set /p={\"status\":\"unlocked\"}\r\n",
            "  exit /b 0\r\n",
            ")\r\n",
            "<nul set /p=%BW_SESSION%\r\n",
            "exit /b 0\r\n",
        );
        #[cfg(not(windows))]
        let body = concat!(
            "#!/bin/sh\n",
            "if [ \"$1\" = \"status\" ]; then\n",
            "  printf '{\"status\":\"unlocked\"}'\n",
            "  exit 0\n",
            "fi\n",
            "printf '%s' \"$BW_SESSION\"\n",
        );
        let stub = write_stub(&dir, "bw-session", body);
        let p = BitwardenProvider::new(bw_items("api-key-item"))
            .with_cli_path(stub.to_str().unwrap())
            .with_session("test-bw-session-42");
        let value = p.get("FILAR_LLM_API_KEY").unwrap();
        assert_eq!(value, "test-bw-session-42");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn bw_provider_unmapped_name_is_a_miss() {
        let p = BitwardenProvider::new(bw_items("api-key-item"));
        let err = p.get("ssh_target:other").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bitwarden"), "{msg}");
        assert!(msg.contains("no bw item mapping"), "{msg}");
    }

    #[test]
    fn bw_provider_missing_cli_is_a_clear_error() {
        let p = BitwardenProvider::new(bw_items("api-key-item"))
            .with_cli_path("filar-nonexistent-bw-cli-992");
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("bitwarden"), "{msg}");
        assert!(msg.contains("not found"), "{msg}");
    }

    #[test]
    fn bw_provider_debug_carries_no_values_or_references() {
        let p = BitwardenProvider::new(bw_items("api-key-item"));
        let dbg = format!("{p:?}");
        assert!(dbg.contains("BitwardenProvider"), "{dbg}");
        assert!(dbg.contains("items: 1"), "{dbg}");
        assert!(!dbg.contains("api-key-item"), "{dbg}");
    }

    #[test]
    fn bw_provider_secret_names_lists_the_mapping() {
        let p = BitwardenProvider::new(bw_items("api-key-item"));
        assert_eq!(p.secret_names(), vec!["FILAR_LLM_API_KEY".to_string()]);
    }

    #[test]
    fn chain_from_config_builds_bitwarden() {
        let mut cfg = crate::config::SecretProviderConfig {
            order: vec!["bitwarden".into()],
            ..Default::default()
        };
        cfg.bitwarden.items = bw_items("api-key-item");
        // Building the chain must not spawn the CLI.
        let c = ChainSecretProvider::from_config(&cfg).unwrap();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("providers: 1"), "{dbg}");
        assert!(c.secret_names().contains(&"FILAR_LLM_API_KEY".to_string()));
    }

    #[test]
    #[ignore = "needs Bitwarden CLI (bw) with an unlocked vault; set FILAR_TEST_BW_ITEM"]
    fn bw_provider_real_cli_integration() {
        let id = std::env::var("FILAR_TEST_BW_ITEM")
            .expect("set FILAR_TEST_BW_ITEM to a Bitwarden item id");
        let p = BitwardenProvider::new(bw_items(&id));
        let value = p.get("FILAR_LLM_API_KEY").unwrap();
        assert!(!value.is_empty());
    }

    // ------------------------------------------------------------------
    // KeePassXcProvider (#693)
    // ------------------------------------------------------------------

    fn kx_items(entry: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("FILAR_LLM_API_KEY".to_string(), entry.to_string());
        m
    }

    /// A static password source holding the test database password.
    fn kx_password_source() -> Arc<dyn SecretProvider> {
        let s = StaticSecretProvider::new();
        s.insert("KX_DB_PASS", "kx-db-pass-42");
        Arc::new(s)
    }

    /// Stub `keepassxc-cli`: reads the password line from stdin like the
    /// real CLI; `show -a <attr> <db> <entry>` selects the branch by the
    /// entry (5th argument). The value is printed with the CLI's own
    /// trailing newline, "newline-entry" adds a stored one on top.
    fn kx_stub(dir: &std::path::Path) -> std::path::PathBuf {
        #[cfg(windows)]
        let body = concat!(
            "@echo off\r\n",
            "set /p PW=\r\n",
            "if \"%5\"==\"missing-entry\" (\r\n",
            "  echo Could not find entry with path %5. 1>&2\r\n",
            "  exit /b 1\r\n",
            ")\r\n",
            "if \"%5\"==\"wrong-pass\" (\r\n",
            "  echo Invalid credentials were provided, please try again. marker-stderr-xyz 1>&2\r\n",
            "  exit /b 1\r\n",
            ")\r\n",
            "echo kx-secret-value\r\n",
            "if \"%5\"==\"newline-entry\" echo.\r\n",
            "exit /b 0\r\n",
        );
        #[cfg(not(windows))]
        let body = concat!(
            "#!/bin/sh\n",
            "read PW\n",
            "case \"$5\" in\n",
            "*missing-entry*) printf '%s\\n' \"Could not find entry with path $5.\" >&2; exit 1 ;;\n",
            "*wrong-pass*) printf '%s\\n' \"Invalid credentials were provided, please try again. marker-stderr-xyz\" >&2; exit 1 ;;\n",
            "*newline-entry*) printf 'kx-secret-value\\n\\n' ;;\n",
            "*) printf 'kx-secret-value\\n' ;;\n",
            "esac\n",
        );
        write_stub(dir, "kx", body)
    }

    #[test]
    fn kx_provider_reads_value_via_cli_stub() {
        let dir = op_stub_dir("kx-read");
        let stub = kx_stub(&dir);
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"))
            .with_cli_path(stub.to_str().unwrap())
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        // The CLI's own trailing newline is stripped, the value is intact.
        assert_eq!(p.get("FILAR_LLM_API_KEY").unwrap(), "kx-secret-value");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kx_provider_preserves_a_stored_terminal_newline() {
        let dir = op_stub_dir("kx-newline");
        let stub = kx_stub(&dir);
        let p = KeePassXcProvider::new("test.kdbx", kx_items("newline-entry"))
            .with_cli_path(stub.to_str().unwrap())
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        // Exactly one CLI-added line break is stripped; the stored one stays.
        #[cfg(windows)]
        let expected = "kx-secret-value\r\n";
        #[cfg(not(windows))]
        let expected = "kx-secret-value\n";
        assert_eq!(p.get("FILAR_LLM_API_KEY").unwrap(), expected);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kx_provider_wrong_password_error_hides_password_and_stderr() {
        let dir = op_stub_dir("kx-wrongpass");
        let stub = kx_stub(&dir);
        let p = KeePassXcProvider::new("test.kdbx", kx_items("wrong-pass"))
            .with_cli_path(stub.to_str().unwrap())
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("keepassxc"), "{msg}");
        assert!(msg.contains("wrong database password"), "{msg}");
        assert!(!msg.contains("kx-db-pass-42"), "{msg}");
        assert!(!msg.contains("marker-stderr-xyz"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kx_provider_missing_entry_error() {
        let dir = op_stub_dir("kx-missing");
        let stub = kx_stub(&dir);
        let p = KeePassXcProvider::new("test.kdbx", kx_items("missing-entry"))
            .with_cli_path(stub.to_str().unwrap())
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("keepassxc"), "{msg}");
        assert!(msg.contains("entry not found"), "{msg}");
        assert!(!msg.contains("kx-db-pass-42"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kx_provider_timeout_kills_the_cli() {
        let dir = op_stub_dir("kx-slow");
        #[cfg(windows)]
        let body = "@echo off\r\nping -n 30 127.0.0.1 >nul\r\nexit /b 0\r\n";
        #[cfg(not(windows))]
        let body = "#!/bin/sh\nsleep 30\n";
        let stub = write_stub(&dir, "kx-slow", body);
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"))
            .with_cli_path(stub.to_str().unwrap())
            .with_timeout(Duration::from_millis(300))
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        let started = Instant::now();
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(10), "timeout not enforced");
        let msg = format!("{err}");
        assert!(msg.contains("timed out"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kx_provider_missing_cli_is_a_clear_error() {
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"))
            .with_cli_path("filar-nonexistent-kx-cli-993")
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("keepassxc"), "{msg}");
        assert!(msg.contains("not found"), "{msg}");
    }

    #[test]
    fn kx_provider_passes_password_via_stdin() {
        // The stub echoes the stdin line back: the database password must
        // reach the child through its stdin, not through args or a file.
        let dir = op_stub_dir("kx-stdin");
        #[cfg(windows)]
        let body = "@echo off\r\nset /p PW=\r\necho %PW%\r\nexit /b 0\r\n";
        #[cfg(not(windows))]
        let body = "#!/bin/sh\nread PW\nprintf '%s\\n' \"$PW\"\n";
        let stub = write_stub(&dir, "kx-stdin", body);
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"))
            .with_cli_path(stub.to_str().unwrap())
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        assert_eq!(p.get("FILAR_LLM_API_KEY").unwrap(), "kx-db-pass-42");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn kx_provider_unmapped_name_is_a_miss() {
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"));
        let err = p.get("ssh_target:other").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("keepassxc"), "{msg}");
        assert!(msg.contains("no entry mapping"), "{msg}");
    }

    #[test]
    fn kx_provider_no_password_source_is_a_clear_error() {
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"));
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("keepassxc"), "{msg}");
        assert!(msg.contains("no database password source"), "{msg}");
    }

    #[test]
    fn kx_provider_unavailable_password_names_the_logical_name_only() {
        let empty = StaticSecretProvider::new();
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"))
            .with_password_provider(Arc::new(empty), "KX_DB_PASS");
        let err = p.get("FILAR_LLM_API_KEY").unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("keepassxc"), "{msg}");
        assert!(msg.contains("KX_DB_PASS"), "{msg}");
        assert!(msg.contains("not available"), "{msg}");
    }

    #[test]
    fn kx_provider_debug_carries_no_values_or_entries() {
        let p = KeePassXcProvider::new("test.kdbx", kx_items("secret-entry-path"))
            .with_password_provider(kx_password_source(), "KX_DB_PASS");
        let dbg = format!("{p:?}");
        assert!(!dbg.contains("kx-db-pass-42"), "{dbg}");
        assert!(!dbg.contains("secret-entry-path"), "{dbg}");
        assert!(dbg.contains("entries: 1"), "{dbg}");
        assert!(dbg.contains("password_source: true"), "{dbg}");
    }

    #[test]
    fn kx_provider_secret_names_lists_the_mapping() {
        let p = KeePassXcProvider::new("test.kdbx", kx_items("api-key-entry"));
        assert_eq!(p.secret_names(), vec!["FILAR_LLM_API_KEY".to_string()]);
    }

    #[test]
    fn chain_from_config_builds_keepassxc() {
        let mut cfg = crate::config::SecretProviderConfig {
            order: vec!["keepassxc".into()],
            ..Default::default()
        };
        cfg.keepassxc.database = Some("test.kdbx".into());
        cfg.keepassxc.entries = kx_items("api-key-entry");
        // Building the chain must not spawn the CLI or touch the keyring.
        let c = ChainSecretProvider::from_config(&cfg).unwrap();
        let dbg = format!("{c:?}");
        assert!(dbg.contains("providers: 1"), "{dbg}");
        assert!(c.secret_names().contains(&"FILAR_LLM_API_KEY".to_string()));
    }

    #[test]
    #[ignore = "needs keepassxc-cli and a test database; set FILAR_TEST_KX_DB, FILAR_TEST_KX_ENTRY, FILAR_TEST_KX_PASSWORD"]
    fn kx_provider_real_cli_integration() {
        let db = std::env::var("FILAR_TEST_KX_DB").expect("set FILAR_TEST_KX_DB to a .kdbx path");
        let entry =
            std::env::var("FILAR_TEST_KX_ENTRY").expect("set FILAR_TEST_KX_ENTRY to an entry path");
        let password =
            std::env::var("FILAR_TEST_KX_PASSWORD").expect("set FILAR_TEST_KX_PASSWORD");
        let source = StaticSecretProvider::new();
        source.insert("KX_DB_PASS", password);
        let p = KeePassXcProvider::new(db, kx_items(&entry))
            .with_password_provider(Arc::new(source), "KX_DB_PASS");
        let value = p.get("FILAR_LLM_API_KEY").unwrap();
        assert!(!value.is_empty());
    }
}
