//! Minimal GUI launcher for Warp.
//!
//! Shows a simple window where the user can:
//! - Pick a previous session (up to 10) to restore.
//! - Select a target: `Local` or a host from the saved SSH list (#411).
//! - Enter model, API URL, and API key.
//!
//! **Security:** Sensitive data (API keys, SSH passwords) is stored in the
//! OS credential manager (Windows Credential Manager, macOS Keychain, Linux
//! Secret Service) — NEVER in plain-text files. Non-sensitive data (host,
//! port, user, model, API URL) is saved in `settings.json`.
//!
//! The API key is always saved to the credential store after the first launch.
//! SSH passwords are saved only when the user checks "Save password" for that
//! SSH slot.
//!
//! On "Launch", returns a [`LaunchConfig`] that `main.rs` uses to start the TUI.

use eframe::egui;
use serde::{Deserialize, Serialize};

use filar_core::{Config, SessionMeta, SessionStore};
use filar_transport::ClusterApi as _;

/// Current SSH host-list format version (#411).
///
/// Version 0 (the field absent from `settings.json`) is the pre-#411 fixed
/// layout: exactly five slots, where a nameless host's display name — and its
/// keyring key — came from its position (`SSH{slot+1}`). Version 1 is the
/// open-ended host list where every real host carries its own alias.
const SSH_LIST_VERSION: u32 = 1;

/// Service name used for the OS credential store.
const CRED_SERVICE: &str = "filar";

// ---------------------------------------------------------------------------
// Credential store helpers (OS keyring / Credential Manager)
// ---------------------------------------------------------------------------

/// Normalize clipboard / pasteboard text for API keys and SSH passwords.
///
/// macOS Keychain and browsers typically copy a trailing newline (and sometimes
/// a UTF-8 BOM or zero-width chars). egui 0.29 single-line `TextEdit` then
/// replaces `\n`/`\r` with a **space**, so the secret becomes `"token "` and
/// LLM auth / SSH login fail — while typing the same value by hand works (#312).
fn sanitize_secret_clipboard(raw: &str) -> String {
    let s = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let first = s.split(['\n', '\r']).next().unwrap_or("");
    first
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{feff}' | '\u{200b}' | '\u{200c}' | '\u{200d}')
        })
        .collect()
}

/// If a secret field is focused, consume `Event::Paste` before `TextEdit` can
/// turn a trailing newline into a trailing space, and assign the sanitized value.
fn apply_focused_secret_paste(ui: &mut egui::Ui, id: egui::Id, value: &mut String) {
    if !ui.memory(|m| m.has_focus(id)) {
        return;
    }
    let mut paste = None;
    ui.input(|i| {
        for ev in &i.events {
            if let egui::Event::Paste(s) = ev {
                paste = Some(s.clone());
                break;
            }
        }
    });
    let Some(raw) = paste else {
        return;
    };
    ui.input_mut(|i| {
        i.events.retain(|e| !matches!(e, egui::Event::Paste(_)));
    });
    let clean = sanitize_secret_clipboard(&raw);
    if !clean.is_empty() {
        *value = clean;
    }
}

/// Masked (or plain) single-line secret editor with paste sanitization.
fn secret_text_edit(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    value: &mut String,
    show: bool,
) -> egui::Response {
    let id = ui.id().with(id_salt);
    apply_focused_secret_paste(ui, id, value);
    ui.add(egui::TextEdit::singleline(value).password(!show).id(id))
}

/// Save a secret to the OS credential store.
///
/// A no-op in unit tests, like [`load_secret`] and [`delete_secret`]: host
/// list tests use ordinary aliases, and must never touch the developer's
/// real credential store.
fn save_secret(username: &str, secret: &str) {
    if cfg!(test) {
        return;
    }
    let secret = sanitize_secret_clipboard(secret);
    if secret.is_empty() {
        delete_secret(username);
        return;
    }
    match keyring::Entry::new(CRED_SERVICE, username) {
        Ok(entry) => {
            if let Err(e) = entry.set_password(&secret) {
                tracing::warn!(error = %e, "failed to save secret to credential store");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to create credential entry"),
    }
}

/// Load a secret from the OS credential store. Returns empty string if not found.
fn load_secret(username: &str) -> String {
    if cfg!(test) {
        return String::new();
    }
    let raw = match keyring::Entry::new(CRED_SERVICE, username) {
        Ok(entry) => entry.get_password().unwrap_or_default(),
        Err(_) => String::new(),
    };
    sanitize_secret_clipboard(&raw)
}

/// Delete a secret from the OS credential store.
fn delete_secret(username: &str) {
    if cfg!(test) {
        return;
    }
    if let Ok(entry) = keyring::Entry::new(CRED_SERVICE, username) {
        let _ = entry.delete_credential();
    }
}

/// Credential key for the API key.
fn api_key_cred_name() -> &'static str {
    "api_key"
}

/// Try to load the API key for a profile from the OS credential store.
fn load_secret_for_profile(profile: &filar_core::LlmProfile) -> String {
    load_secret(&profile.key_env)
}

/// Parse the compaction threshold typed in the launcher.
///
/// An empty or unparseable field falls back to the built-in default rather
/// than to `0`, because `0` means "compaction disabled" and silently disabling
/// a feature because someone left a box blank would be the wrong default.
/// Validation rejects genuinely malformed input before Launch.
fn parse_compact_at_tokens(raw: &str) -> u64 {
    let text = raw.trim();
    if text.is_empty() {
        return filar_core::DEFAULT_COMPACT_AT_TOKENS;
    }
    text.parse().unwrap_or(filar_core::DEFAULT_COMPACT_AT_TOKENS)
}

/// Parse the reply-length cap typed in the launcher.
///
/// An empty or unparseable field falls back to the built-in default, as does
/// `0`: the API reads `max_tokens = 0` as "produce nothing", so it is never a
/// meaningful thing to save. Validation rejects malformed input before Launch.
fn parse_max_tokens(raw: &str) -> u32 {
    match raw.trim().parse::<u32>() {
        Ok(n) if n > 0 => n,
        _ => filar_core::DEFAULT_MAX_TOKENS,
    }
}

/// Validate the numeric and JSON fields of one profile as typed.
///
/// Returns the message to show, or `Ok(())` when every field either parses or
/// is empty (empty means "use the default" for each of them).
///
/// These are exactly the fields [`LlmProfileData::to_profile`] converts with a
/// fallback: an unparseable value there becomes `None` or a built-in default
/// with no error, which is how a typo in one profile used to be swallowed on
/// save (#385). Keep the two in step — a field that gains a fallback in
/// `to_profile` needs a check here, or it will silently reset again.
fn validate_profile_fields(p: &LlmProfileData) -> Result<(), String> {
    if !p.temperature.trim().is_empty()
        && !matches!(p.temperature.trim().parse::<f32>(), Ok(t) if t.is_finite() && (0.0..=2.0).contains(&t))
    {
        return Err(format!(
            "Invalid temperature: '{}'. Expected [0.0, 2.0].",
            p.temperature
        ));
    }
    if !p.max_tokens.trim().is_empty()
        && !matches!(p.max_tokens.trim().parse::<u32>(), Ok(n) if n > 0)
    {
        return Err(format!(
            "Invalid max tokens: '{}'. Expected a whole number above 0.",
            p.max_tokens
        ));
    }
    if !p.top_p.trim().is_empty()
        && !matches!(p.top_p.trim().parse::<f32>(), Ok(t) if t.is_finite() && t > 0.0 && t <= 1.0)
    {
        return Err(format!("Invalid top_p: '{}'. Expected (0.0, 1.0].", p.top_p));
    }
    if !p.compact_at_tokens.trim().is_empty() && p.compact_at_tokens.trim().parse::<u64>().is_err() {
        return Err(format!(
            "Invalid compaction threshold: '{}'. Expected a whole number of tokens (0 = off).",
            p.compact_at_tokens
        ));
    }
    if !p.extra_body.trim().is_empty()
        && serde_json::from_str::<serde_json::Value>(&p.extra_body).is_err()
    {
        return Err("Invalid extra body JSON.".to_string());
    }
    Ok(())
}

/// Validate every profile, not just the selected one, naming the offender.
///
/// Every persistence path converts the whole list through
/// [`LlmProfileData::to_profile`], so checking only the selected profile left
/// the others free to be rewritten with defaults (#385).
fn validate_all_profile_fields<'a>(
    profiles: impl IntoIterator<Item = &'a LlmProfileData>,
) -> Result<(), String> {
    for p in profiles {
        if let Err(msg) = validate_profile_fields(p) {
            return Err(format!("Profile \"{}\": {msg}", p.name));
        }
    }
    Ok(())
}

/// Generate a unique profile name with the given prefix.
/// Finds the first free number (profile-1, profile-2, ...) not already in use.
fn unique_profile_name(existing: &[LlmProfileData], prefix: &str) -> String {
    let mut n = 1;
    loop {
        let candidate = format!("{prefix}-{n}");
        if !existing.iter().any(|p| p.name == candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Credential key for SSH slot N. Uses the same naming convention as
/// the TUI runner: `ssh_target:{name}` where name is the alias (or
/// `SSH{slot+1}` if no alias is set).
///
/// **Contract:** the `name` produced here MUST match the `target.name`
/// field in `build_ssh_targets_from_profiles`, because the runner looks
/// up the password under `format!("ssh_target:{}", target.name)`.
/// Both functions use `SSH{slot+1}` as the fallback for empty alias.
fn ssh_cred_name(slot: usize, alias: &str) -> String {
    filar_core::ssh_cred_name(slot, alias)
}

/// Migration: fix duplicate profile names and key_env entries in a loaded list.
fn deduplicate_profiles(profiles: &mut Vec<LlmProfileData>) {
    let mut seen_names = std::collections::BTreeSet::new();
    for p in profiles.iter_mut() {
        let mut name = p.name.clone();
        while seen_names.contains(&name) {
            name = format!("{name}_dup");
        }
        if name != p.name {
            tracing::warn!(old = %p.name, new = %name, "deduplicated colliding profile name on load");
            p.name = name;
        }
        seen_names.insert(p.name.clone());
    }
    // Fix key_env collisions: ensure each *non-empty* key_env is unique.
    // Empty key_env is allowed on multiple profiles (keyless local servers).
    let mut seen_envs = std::collections::BTreeSet::new();
    for p in profiles.iter_mut() {
        if p.key_env.trim().is_empty() {
            p.key_env.clear();
            continue;
        }
        let mut env = p.key_env.clone();
        while seen_envs.contains(&env) {
            env = format!("api_key_{env}");
        }
        if env != p.key_env {
            tracing::warn!(old = %p.key_env, new = %env, profile = %p.name, "deduplicated colliding key_env on load");
            p.key_env = env;
        }
        seen_envs.insert(p.key_env.clone());
    }
}

/// Migrate the pre-#411 fixed-slot layout to the open-ended host list.
///
/// Unnamed hosts get their positional display name (`SSH{slot+1}`) as a real
/// alias: names do not change, so the keyring keys (`ssh_target:SSH3`) stay
/// valid and no saved password is orphaned (#411). Blank placeholder slots —
/// the old layout always carried exactly five — are dropped *after* the alias
/// pass, so the survivors keep the positions they were named from. Returns
/// `true` when anything changed.
fn migrate_ssh_profiles(profiles: &mut Vec<SshProfile>) -> bool {
    let mut changed = false;

    // Aliases first, while the positions are still those of the old list.
    for (i, p) in profiles.iter_mut().enumerate() {
        if p.host.trim().is_empty() || !p.alias.trim().is_empty() {
            continue;
        }
        p.alias = format!("SSH{}", i + 1);
        changed = true;
    }

    // A blank entry was a placeholder, not a host.
    let before = profiles.len();
    profiles.retain(|p| !p.host.trim().is_empty());
    changed |= profiles.len() != before;

    // The alias names the keyring entry, the Ctrl+O list and the per-target
    // export folder; a duplicate silently hides one of two hosts, so rename.
    let mut seen = std::collections::HashSet::new();
    for p in profiles.iter_mut() {
        let mut alias = p.alias.trim().to_string();
        while !seen.insert(alias.clone()) {
            alias = format!("{alias}_dup");
        }
        if alias != p.alias {
            tracing::warn!(old = %p.alias, new = %alias, "renamed colliding SSH host alias on load");
            p.alias = alias;
            changed = true;
        }
    }

    changed
}

/// Re-encode the pre-#411 `last_ssh` from a padded-list slot index into the
/// 1-based persisted position used by the open-ended list (#411).
///
/// The old encoding could not tell "Local" from the first slot (both `0`)
/// and knew nothing about the blank placeholders the migration drops. A
/// selected placeholder — or an index that names nothing — has no host to
/// restore: `0`.
fn migrate_last_ssh(last_ssh: usize, profiles: &[SshProfile]) -> usize {
    if last_ssh == 0 || last_ssh >= profiles.len() {
        return 0;
    }
    if profiles[last_ssh].host.trim().is_empty() {
        return 0;
    }
    profiles[..last_ssh]
        .iter()
        .filter(|p| !p.host.trim().is_empty())
        .count()
        + 1
}

// ---------------------------------------------------------------------------
// LaunchConfig — returned to main.rs
// ---------------------------------------------------------------------------

/// The user's choices from the launcher GUI.
///
/// Secrets (API key, SSH passwords) are NOT serialized — they go through
/// the OS credential store. See `filar_core::secrets::KeyringSecretProvider`.
#[derive(Serialize, Deserialize)]
pub struct LaunchConfig {
    /// Session label: `"local"` for local launches, otherwise the SSH
    /// target's display name — the alias, or `SSH{n}` when the alias is
    /// empty (#406). Never infer the transport from this string: use
    /// `ssh.is_some()` — an alias may even be `"local"`.
    pub target: String,
    /// SSH connection details; `Some` identifies an SSH launch and drives
    /// the executor choice in `main` (#406).
    pub ssh: Option<SshConnection>,
    /// Kubernetes tab to open. Transport selection is explicit, never inferred
    /// from the display label.
    #[serde(default)]
    pub kube: Option<filar_core::KubeSessionSnapshot>,
    /// Cluster definitions as edited in the launcher. `None` in an old handoff
    /// means "not said" and preserves the config-file fallback.
    #[serde(default)]
    pub kube_clusters: Option<Vec<filar_core::KubeCluster>>,
    /// Model name (e.g. `"deepseek-chat"`).
    pub model: String,
    /// API base URL.
    pub api_base_url: String,
    /// API key entered by the user — NEVER written to disk.
    #[serde(skip, default)]
    pub api_key: String,
    /// Session ID to restore, if the user picked a previous session.
    pub session_id: Option<String>,
    /// Temperature as text (empty = provider default).
    #[serde(default)]
    pub temperature: String,
    /// Extra body JSON as text (empty = none).
    #[serde(default)]
    pub extra_body: String,
    /// Selected profile name (if using the Models tab).
    #[serde(default)]
    pub selected_profile: Option<String>,
    /// Env var / credential name for the API key.
    #[serde(default = "filar_core::config::default_llm_key_env")]
    pub key_env: String,
    /// Directory for Ctrl+S session exports (`None` = CWD).
    #[serde(default)]
    pub save_dir: Option<std::path::PathBuf>,
    /// Full LLM profile list (for Ctrl+L cycling in the TUI).
    #[serde(default)]
    pub profiles: Vec<filar_core::LlmProfile>,
    /// Full SSH target list (for Ctrl+O cycling in the TUI).
    #[serde(default)]
    pub ssh_targets: Vec<filar_core::SshTarget>,
    /// Optional arbiter LLM profile name (`None` = same as session profile).
    /// Carried through pending_launch so the TUI does not depend on CWD config.toml (#360).
    #[serde(default)]
    pub arbiter_profile: Option<String>,
    /// Host groups as the launcher's Groups tab left them (#501): the
    /// session started by this launch uses them instead of the snapshot of
    /// `config.toml` read before the launcher opened. `Some(vec![])` means
    /// every group was removed; `None` (a file written before #501) means
    /// "not said" and the session keeps what `config.toml` holds.
    #[serde(default)]
    pub host_groups: Option<Vec<filar_core::HostGroup>>,
    /// The command safety scorer as the launcher left it (#552): `On` with
    /// its settings, or `Off`. `None` (a file written before #552) means
    /// "not said" and the session keeps `config.toml`'s `[risk_scorer]`.
    /// Never carries the key — only `key_env`, the name it is stored under.
    #[serde(default)]
    pub risk_scorer: Option<RiskScorerLaunch>,
    /// `[[log_sources]]` as the launcher left them (#584): `Some(vec![])`
    /// means every source was removed; `None` (a file from before #584)
    /// means "not said" and the session keeps `config.toml`'s. Never carries
    /// a key — only `key_env`, the name it is stored under.
    #[serde(default)]
    pub log_sources: Option<Vec<filar_core::LogSourceConfig>>,
}

/// The launcher's choice for the command safety scorer (#552).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum RiskScorerLaunch {
    /// No scorer for this session.
    Off,
    /// Rate commands with this scorer.
    On {
        /// The `[risk_scorer]` settings (no secret in them).
        config: filar_core::RiskScorerConfig,
    },
}

/// How the launcher logs in to an SSH host (#489).
///
/// Mirrors the three [`filar_core::SshAuth`] variants without their secrets:
/// a password or key passphrase lives only in the OS credential store.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SlotAuth {
    /// Password, typed or saved in the OS credential store.
    Password,
    /// A private key file, optionally passphrase-protected.
    Key,
    /// Keys of the running SSH agent — the default, like `SshAuth::default()`.
    #[default]
    Agent,
}

impl SlotAuth {
    /// The auth of a slot saved before #489, when the launcher could only
    /// log in by password: whatever `save_password` said, Launch always
    /// built password auth.
    fn legacy() -> Self {
        Self::Password
    }

    /// The `auth = { type = "…" }` name used by `config.toml` and the host
    /// list import/export.
    fn type_name(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::Key => "key",
            Self::Agent => "agent",
        }
    }
}

/// SSH connection details from the GUI.
///
/// The password is NEVER serialized — it goes through the OS credential store.
/// So does a key passphrase, which is not even carried here: the session
/// reads it from the store under [`filar_core::ssh_key_passphrase_name`].
#[derive(Clone, Serialize, Deserialize)]
pub struct SshConnection {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Password — NEVER written to disk.
    #[serde(skip, default)]
    pub password: String,
    /// Login method (#489). A handoff file written before it had none and
    /// was always password login.
    #[serde(default = "SlotAuth::legacy")]
    pub auth: SlotAuth,
    /// Key file for [`SlotAuth::Key`]; `None` = the default key
    /// (`~/.ssh/id_ed25519`, then `id_rsa`). Not a secret.
    #[serde(default)]
    pub key_path: Option<std::path::PathBuf>,
    /// Slot index for saving/loading the password from the OS credential store.
    /// Not a secret — must be persisted so resume picks the correct keyring entry.
    #[serde(default)]
    pub slot: usize,
    /// Optional alias for the slot (empty → `SSH{slot+1}`). Not a secret —
    /// persisted so GUI→TUI handoff can rebuild the keyring key via
    /// [`filar_core::ssh_cred_name`].
    #[serde(default)]
    pub alias: String,
    /// Tags of the target the launcher connected with (#413). Not a secret —
    /// persisted so the TUI can show them in the status bar.
    #[serde(default)]
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Pending launch — used when GUI runs as a subprocess
// ---------------------------------------------------------------------------

fn pending_launch_path() -> Option<std::path::PathBuf> {
    let base = filar_core::session::default_base_dir().ok()?;
    Some(base.join("filar").join("pending_launch.json"))
}

pub fn save_pending_launch(cfg: &LaunchConfig) {
    if let Some(p) = pending_launch_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string(cfg) {
            let _ = std::fs::write(p, data);
        }
    }
}

pub fn load_pending_launch() -> Option<LaunchConfig> {
    let p = pending_launch_path()?;
    if !p.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&p).ok()?;

    // Check for old-format files that still contain plaintext secrets.
    // Users upgrading from 0.6.x may have these on disk; delete them
    // and return None so the GUI re-launches with the new clean format.
    if data.contains("\"api_key\":") || data.contains("\"password\":") {
        let _ = std::fs::remove_file(&p);
        tracing::info!("deleted old-format pending_launch.json with plaintext secrets");
        return None;
    }

    let _ = std::fs::remove_file(&p);
    serde_json::from_str(&data).ok()
}

// ---------------------------------------------------------------------------
// Settings — saved between launches (NO secrets, only non-sensitive data)
// ---------------------------------------------------------------------------

/// A saved SSH profile (host/port/user only — NO password).
#[derive(Serialize, Deserialize, Default, Clone)]
struct SshProfile {
    host: String,
    port: String,
    user: String,
    /// Optional alias shown in the target selector instead of `SSHn`.
    #[serde(default)]
    alias: String,
    /// Comma-separated free-form tags (e.g. `"prod, web"`) — shown in the
    /// TUI status bar and usable for per-tag policies (#413).
    #[serde(default)]
    tags: String,
    /// Whether the user checked "Save password" for this slot.
    #[serde(default)]
    save_password: bool,
    /// Login method (#489). `None` in files saved before it: those slots
    /// migrate to [`SlotAuth::legacy`] on load.
    #[serde(default)]
    auth: Option<SlotAuth>,
    /// Key file for key login; empty = the default key. Not a secret.
    #[serde(default)]
    key_path: String,
    /// Whether the user checked "Save passphrase" for the key (#489).
    #[serde(default)]
    save_passphrase: bool,
}

/// Persistent settings saved between launches.
#[derive(Serialize, Deserialize, Default)]
struct Settings {
    model: String,
    api_base_url: String,
    #[serde(default)]
    ssh_profiles: Vec<SshProfile>,
    /// Kubernetes targets as last saved by the launcher. These contain only
    /// target metadata, never kubeconfig contents or credentials.
    #[serde(default)]
    kube_profiles: Vec<filar_core::KubeCluster>,
    /// Name of the last selected cluster; `None` restores `last_ssh` instead.
    #[serde(default)]
    last_kube: Option<String>,
    /// SSH host-list format version (#411). Absent in pre-#411 files, which
    /// is what triggers the one-time migration in [`Settings::load`].
    #[serde(default)]
    ssh_list_version: u32,
    /// Last selected SSH target: 1 + its position in `ssh_profiles`
    /// (0 = Local). Pre-#411 files store the raw slot index here; the
    /// one-time migration re-encodes it (#411).
    #[serde(default)]
    last_ssh: usize,
    #[serde(default)]
    temperature: String,
    #[serde(default)]
    extra_body: String,
    /// Named LLM profiles (for Models tab).
    #[serde(default)]
    profiles: Vec<filar_core::LlmProfile>,
    /// Index of the default (last-selected) profile.
    #[serde(default)]
    selected_profile: usize,
    /// Directory for Ctrl+S session exports (`None` = CWD).
    #[serde(default)]
    save_dir: Option<std::path::PathBuf>,
    /// Optional LLM profile name for the command arbiter (`None` = session profile).
    #[serde(default)]
    arbiter_profile: Option<String>,
}

impl Settings {
    fn path() -> Option<std::path::PathBuf> {
        let base = filar_core::session::default_base_dir().ok()?;
        Some(base.join("filar").join("settings.json"))
    }

    fn load() -> Self {
        Self::load_from(Self::path())
    }

    /// Load from an explicit path (tests pass a temp file; production uses
    /// [`Self::path`]).
    ///
    /// A corrupt or unreadable file falls back to `Default` *in memory only*
    /// and the migration save is skipped for it: a parse failure would
    /// otherwise read as "version 0, empty list" and overwrite the file —
    /// the only copy of the host list and LLM profiles — with defaults.
    fn load_from(path: Option<std::path::PathBuf>) -> Self {
        let parsed = path
            .as_ref()
            .filter(|p| p.exists())
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|data| serde_json::from_str::<Self>(&data).ok());
        let parsed_ok = parsed.is_some();
        let mut settings = parsed.unwrap_or_default();
        if parsed_ok && settings.apply_ssh_list_migration() {
            // Persist right away so the migration happens once, not on every
            // start (the same pattern as the profile-dedup repair in
            // `run_launcher`). Only a successfully parsed file is rewritten.
            if let Some(p) = path.as_ref() {
                settings.save_to(p);
            }
        }
        settings
    }

    /// Bring a loaded host list up to the current format (#411).
    ///
    /// Returns `true` when the settings file must be re-saved: the version
    /// marker itself is the durable part — a repeated alias pass is only ever
    /// wrong to run, never wrong to skip. Also re-encodes the pre-#411
    /// `last_ssh` slot index ([`migrate_last_ssh`]).
    fn apply_ssh_list_migration(&mut self) -> bool {
        if self.ssh_list_version >= SSH_LIST_VERSION {
            return false;
        }
        self.last_ssh = migrate_last_ssh(self.last_ssh, &self.ssh_profiles);
        let migrated = migrate_ssh_profiles(&mut self.ssh_profiles);
        self.ssh_list_version = SSH_LIST_VERSION;
        tracing::info!(migrated, "SSH host list migrated to the open-ended format");
        true
    }

    fn save(&self) {
        if let Some(p) = Self::path() {
            self.save_to(&p);
        }
    }

    /// Write to an explicit path; the parent directory is created first.
    fn save_to(&self, path: &std::path::Path) {
        // Ensure parent directory exists before writing.
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, data);
        }
    }
}

// ---------------------------------------------------------------------------
// SSH target merge
// ---------------------------------------------------------------------------

/// Clean one tag: strip control characters and surrounding whitespace (#413).
///
/// Control characters are filtered here and again at the TUI render sink:
/// the tags segment reaches the terminal verbatim, and a launcher value must
/// not be able to inject escape sequences.
fn clean_tag(raw: &str) -> String {
    raw.chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .to_string()
}

/// Split the launcher's comma-separated tag field into tags (#413).
///
/// Empty items are dropped; order is preserved.
fn parse_tags(raw: &str) -> Vec<String> {
    raw.split(',').map(clean_tag).filter(|t| !t.is_empty()).collect()
}

/// The key file typed in a slot: `None` for a blank field (the default key).
fn slot_key_path(key_path: &str) -> Option<std::path::PathBuf> {
    let trimmed = key_path.trim();
    (!trimmed.is_empty()).then(|| std::path::PathBuf::from(trimmed))
}

/// The secret-free [`filar_core::SshAuth`] of a launcher slot (#489). The
/// password and passphrase stay in the OS credential store; the session
/// resolves them by target name.
fn slot_ssh_auth(auth: SlotAuth, key_path: &str) -> filar_core::SshAuth {
    match auth {
        SlotAuth::Password => filar_core::SshAuth::Password { password: None },
        SlotAuth::Key => filar_core::SshAuth::Key {
            path: slot_key_path(key_path),
            passphrase: None,
        },
        SlotAuth::Agent => filar_core::SshAuth::Agent,
    }
}

/// Build the launcher-owned part of the SSH target list from its profiles.
///
/// A full rebuild of the launcher's own records: each non-empty profile
/// produces one `SshTarget`, empty slots are skipped, and nothing of the
/// previous launcher list survives — renaming an alias or clearing a host
/// must drop the stale entry (#222).
///
/// Manual `[[ssh_targets]]` entries live in `config.toml` and are merged on
/// top by [`merge_manual_ssh_targets`] (#412).
fn build_ssh_targets_from_profiles(profiles: &[SshProfile]) -> Vec<filar_core::SshTarget> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut targets: Vec<filar_core::SshTarget> = Vec::new();
    for (i, profile) in profiles.iter().enumerate() {
        if profile.host.is_empty() { continue; }
        let name = filar_core::ssh_target_display_name(i, &profile.alias);
        if seen.contains(&name) { continue; }
        seen.insert(name.clone());
        let port: u16 = profile.port.parse().unwrap_or_else(|_| {
            tracing::warn!(port = %profile.port, slot = i + 1, "invalid SSH port — using 22");
            22
        });
        let port = if port == 0 { 22 } else { port };
        targets.push(filar_core::SshTarget {
            name,
            host: profile.host.clone(),
            port,
            user: profile.user.clone(),
            auth: slot_ssh_auth(
                profile.auth.unwrap_or_else(SlotAuth::legacy),
                &profile.key_path,
            ),
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: parse_tags(&profile.tags),
        });
    }
    targets
}

/// Merge the launcher's rebuilt targets with the manual `[[ssh_targets]]`
/// entries from `config.toml` (#412).
///
/// The launcher owns its profiles and nothing else: its targets are rebuilt
/// from settings on every launch ([`build_ssh_targets_from_profiles`]), while
/// manual entries pass through untouched — in `config.toml` order, after the
/// launcher's. That way `config.toml`, which may be generated by a script or
/// kept in git, stops losing hosts the moment the GUI is used as the entry
/// point.
///
/// A manual entry whose name equals a launcher target's is dropped: the
/// keyring lookup is keyed by target name (`ssh_target:{name}`), and the
/// entry the user configured in the GUI is the one that connects.
fn merge_manual_ssh_targets(
    launcher: Vec<filar_core::SshTarget>,
    manual: &[filar_core::SshTarget],
) -> Vec<filar_core::SshTarget> {
    let mut merged = launcher;
    for target in manual {
        if merged.iter().any(|t| t.name == target.name) {
            continue;
        }
        merged.push(target.clone());
    }
    merged
}

/// Drop passwords from the launcher's copy of the manual `[[ssh_targets]]`
/// (#412).
///
/// `pending_launch.json` is a plain-text handoff and must never carry
/// secrets (#255) — [`load_pending_launch`] even refuses a file that
/// contains a `"password"` key. A password hand-written into `config.toml`
/// stays in the user's file, but it is not copied into the launch payload:
/// the entry keeps its `Password` auth with the value removed, so the
/// session resolves the credential from the keyring or the password prompt,
/// exactly like a launcher target.
fn strip_manual_ssh_passwords(targets: &[filar_core::SshTarget]) -> Vec<filar_core::SshTarget> {
    targets
        .iter()
        .cloned()
        .map(|mut target| {
            if let filar_core::SshAuth::Password { .. } = target.auth {
                target.auth = filar_core::SshAuth::Password { password: None };
            }
            target
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Host list import (#416)
// ---------------------------------------------------------------------------

/// How an import resolves a name that already exists in the host list.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CollisionPolicy {
    /// Keep the existing host, drop the imported one.
    Skip,
    /// Replace the existing entry in place: connection fields, tags, and
    /// — when the file carries one — the auth type (#417).
    Overwrite,
    /// Import under a free `name-N` alias.
    Rename,
}

/// Counters of one applied import, for the launcher's status line.
#[derive(Debug, Default, PartialEq, Eq)]
struct ImportOutcome {
    added: usize,
    overwritten: usize,
    renamed: usize,
    skipped: usize,
    kube_skipped: usize,
}

/// The outcome of staging an import file (#416).
#[derive(Debug)]
enum ImportStaged {
    /// No collisions: the rows were already applied (the policy is a no-op
    /// when nothing collides).
    Applied(ImportOutcome),
    /// Colliding names found — the user must pick a policy first.
    NeedsDecision {
        rows: Vec<ImportRow>,
        collisions: Vec<String>,
        kube_skipped: usize,
    },
}

/// The login method an import file gave a host (#417, #489).
#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportAuth {
    kind: SlotAuth,
    /// The key `path` of `type = "key"`; empty = the default key.
    key_path: String,
}

/// One parsed import row: the candidate slot plus the auth the file
/// specified, if any (#417).
#[derive(Debug)]
struct ImportRow {
    slot: SshSlot,
    /// `None` — the file did not say, so an overwrite keeps the existing
    /// host's auth.
    auth: Option<ImportAuth>,
}

/// Clean a list of imported tags like the launcher's own field does (#413).
fn clean_tags(tags: &[String]) -> Vec<String> {
    tags.iter()
        .map(|t| clean_tag(t))
        .filter(|t| !t.is_empty())
        .collect()
}

/// Build one launcher row from an imported entry, validating it.
///
/// `where_` labels the offending entry in error messages (`line 3`,
/// `host entry #2`). The alias is stored in the same truncated form
/// [`SshSlot::to_profile`] persists, so collision checks and the 32-char
/// uniqueness rule ([`LauncherApp::validate_ssh_slots`]) see exactly what
/// would be saved. `auth` is the file's auth, if it had one (#417); a new
/// host without one gets the default, agent login (#489).
fn make_import_slot(
    where_: &str,
    name: &str,
    host: &str,
    port: u16,
    user: &str,
    tags: &[String],
    auth: Option<&ImportAuth>,
) -> Result<SshSlot, String> {
    // The alias is rendered in the TUI status bar verbatim; `clean_tag`
    // keeps imported control characters out, like the tag fields do
    // (#416 review).
    let alias: String = clean_tag(name).chars().take(32).collect();
    if alias.is_empty() {
        return Err(format!("{where_}: name must not be empty"));
    }
    let host = host.trim();
    if host.is_empty() {
        return Err(format!("{where_}: host must not be empty"));
    }
    if port == 0 {
        return Err(format!("{where_}: invalid port 0"));
    }
    Ok(SshSlot {
        host: host.to_string(),
        port: port.to_string(),
        user: user.trim().to_string(),
        alias,
        tags: clean_tags(tags).join(", "),
        auth: auth.map(|a| a.kind).unwrap_or_default(),
        key_path: auth.map(|a| a.key_path.clone()).unwrap_or_default(),
        ..SshSlot::blank()
    })
}

/// A duplicate alias would silently hide one of two hosts on launch
/// (`validate_ssh_slots` refuses it), so the import file is rejected as a
/// whole rather than half-applied (#416).
fn ensure_unique_aliases(rows: &[ImportRow]) -> Result<(), String> {
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        if !seen.insert(row.slot.alias.clone()) {
            return Err(format!(
                "duplicate host name \"{}\" in the import file",
                row.slot.alias
            ));
        }
    }
    Ok(())
}

/// Parse a TOML host list — the same `[[ssh_targets]]` shape `config.toml`
/// uses. Unknown keys (`host_key_policy`, other sections) are ignored.
///
/// `auth` contributes its `type` (#417) and, for `key`, the key file `path`
/// (#489) — a location, not a secret. An inline `password` is never read:
/// secrets stay in the keyring (invariant #3).
#[cfg(test)]
fn parse_import_toml(text: &str) -> Result<Vec<ImportRow>, String> {
    Ok(parse_import_toml_details(text)?.rows)
}

struct ParsedImport {
    rows: Vec<ImportRow>,
    kube_skipped: usize,
}

fn parse_import_toml_details(text: &str) -> Result<ParsedImport, String> {
    #[derive(serde::Deserialize)]
    struct File {
        #[serde(default)]
        ssh_targets: Vec<Entry>,
        #[serde(default)]
        kube_clusters: Vec<serde::de::IgnoredAny>,
    }
    #[derive(serde::Deserialize)]
    struct Entry {
        name: String,
        host: String,
        #[serde(default = "default_import_port")]
        port: u16,
        #[serde(default)]
        user: String,
        #[serde(default)]
        tags: Vec<String>,
        #[serde(default)]
        auth: Option<Auth>,
    }
    /// The config's `auth = { type = "…" }` table. A `password` field is
    /// not declared, so it is dropped unread (invariant #3).
    #[derive(serde::Deserialize)]
    struct Auth {
        #[serde(rename = "type")]
        kind: String,
        #[serde(default)]
        path: Option<String>,
    }
    fn default_import_port() -> u16 {
        22
    }

    let file: File = toml::from_str(text).map_err(|e| format!("not valid TOML: {e}"))?;
    if file.ssh_targets.is_empty() && file.kube_clusters.is_empty() {
        return Err("no [[ssh_targets]] or [[kube_clusters]] entries found".to_string());
    }
    let mut rows = Vec::with_capacity(file.ssh_targets.len());
    for (i, entry) in file.ssh_targets.iter().enumerate() {
        let where_ = format!("host entry #{}", i + 1);
        let auth = match entry.auth.as_ref() {
            None => None,
            Some(a) => {
                let kind = match a.kind.as_str() {
                    "password" => SlotAuth::Password,
                    "key" => SlotAuth::Key,
                    "agent" => SlotAuth::Agent,
                    other => return Err(format!("{where_}: unknown auth type \"{other}\"")),
                };
                let key_path = match kind {
                    SlotAuth::Key => a.path.as_deref().unwrap_or("").trim().to_string(),
                    _ => String::new(),
                };
                Some(ImportAuth { kind, key_path })
            }
        };
        rows.push(ImportRow {
            slot: make_import_slot(
                &where_,
                &entry.name,
                &entry.host,
                entry.port,
                &entry.user,
                &entry.tags,
                auth.as_ref(),
            )?,
            auth,
        });
    }
    ensure_unique_aliases(&rows)?;
    Ok(ParsedImport {
        rows,
        kube_skipped: file.kube_clusters.len(),
    })
}

/// Split CSV text into rows of fields (RFC 4180: quoted fields, doubled
/// quotes inside them, `\n` or `\r\n` line endings). A leading UTF-8 BOM is
/// dropped. An unterminated quoted field is an error, so a malformed
/// inventory cannot be half-read silently.
fn split_csv_rows(text: &str) -> Result<Vec<Vec<String>>, String> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if in_quotes {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => in_quotes = false,
                other => field.push(other),
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {
                    if chars.peek() == Some(&'\n') {
                        chars.next();
                    }
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    rows.push(std::mem::take(&mut row));
                }
                other => field.push(other),
            }
        }
    }
    if in_quotes {
        return Err("unterminated quoted field".to_string());
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    Ok(rows)
}

/// Parse a CSV host list for foreign inventories (#416): a header row with
/// `name` and `host` columns (case-insensitive, any order), optional `port`,
/// `user` and `tags` columns. Tags are `;`-separated — `,` is the CSV
/// separator itself. Auth types are a TOML-only field (#417); CSV rows
/// default to agent login (#489).
fn parse_import_csv(text: &str) -> Result<Vec<ImportRow>, String> {
    let all_rows = split_csv_rows(text)?;
    let is_blank = |r: &Vec<String>| r.len() == 1 && r[0].trim().is_empty();
    // The header is the first non-blank row. Blank lines keep their place in
    // the numbering below, so an error carries the file's own line number
    // even when the file has gaps (#416 review).
    let header_idx = all_rows
        .iter()
        .position(|r| !is_blank(r))
        .ok_or("empty CSV file")?;
    let header = &all_rows[header_idx];
    let column = |name: &str| header.iter().position(|h| h.trim().eq_ignore_ascii_case(name));
    let name_col = column("name").ok_or("CSV header must include a \"name\" column")?;
    let host_col = column("host").ok_or("CSV header must include a \"host\" column")?;
    let port_col = column("port");
    let user_col = column("user");
    let tags_col = column("tags");

    let mut out = Vec::new();
    for (idx, row) in all_rows.iter().enumerate().skip(header_idx + 1) {
        if is_blank(row) {
            continue;
        }
        // 1-based: the number the user's editor shows.
        let line = idx + 1;
        let field = |col: Option<usize>| {
            col.and_then(|c| row.get(c)).map(String::as_str).unwrap_or("")
        };
        let port = match field(port_col).trim() {
            "" => 22,
            v => v
                .parse::<u16>()
                .map_err(|_| format!("line {line}: invalid port \"{v}\""))?,
        };
        let tags: Vec<String> = field(tags_col).split(';').map(str::to_string).collect();
        out.push(ImportRow {
            slot: make_import_slot(
                &format!("line {line}"),
                field(Some(name_col)),
                field(Some(host_col)),
                port,
                field(user_col),
                &tags,
                None,
            )?,
            auth: None,
        });
    }
    ensure_unique_aliases(&out)?;
    Ok(out)
}

/// Read and parse an import file. The format follows the extension: `.csv`
/// → CSV, anything else → TOML (#416). Nothing is applied here — a parse
/// failure leaves every existing host untouched.
fn parse_import_file(path: &std::path::Path) -> Result<ParsedImport, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let is_csv = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("csv"));
    if is_csv {
        Ok(ParsedImport {
            rows: parse_import_csv(&text)?,
            kube_skipped: 0,
        })
    } else {
        parse_import_toml_details(&text)
    }
}

/// Imported names that already belong to a persisted host. Blank scratch
/// rows do not count: they are dropped before anything is saved (#411), so
/// an alias typed into one is not a name on disk yet.
fn find_import_collisions(existing: &[SshSlot], imported: &[ImportRow]) -> Vec<String> {
    let mut collisions = Vec::new();
    for row in imported {
        let collides = existing
            .iter()
            .any(|e| !e.host.trim().is_empty() && e.alias == row.slot.alias);
        if collides && !collisions.contains(&row.slot.alias) {
            collisions.push(row.slot.alias.clone());
        }
    }
    collisions
}

/// First free `{base}-N` name not held by a persisted host, kept within the
/// 32-char stored alias limit.
fn unique_alias(existing: &[SshSlot], base: &str) -> String {
    let mut n = 2u32;
    loop {
        let suffix = format!("-{n}");
        let stem: String = base
            .chars()
            .take(32usize.saturating_sub(suffix.chars().count()))
            .collect();
        let candidate = format!("{stem}{suffix}");
        if !existing
            .iter()
            .any(|s| !s.host.trim().is_empty() && s.alias == candidate)
        {
            return candidate;
        }
        n += 1;
    }
}

/// Apply parsed rows to the launcher's host list under the chosen policy.
///
/// The policy only affects colliding rows; fresh names are appended.
/// Overwrite replaces the connection fields in place. The file's auth wins
/// when it carries one; without it the host keeps its own (#417, #489).
/// A saved password or key passphrase survives only while it still belongs
/// to the host: same identity (host, port, user), the same login method,
/// and for a passphrase the same key file — a credential of another
/// machine or another key must not linger (#416/#417 reviews).
fn apply_import(
    existing: &mut Vec<SshSlot>,
    imported: Vec<ImportRow>,
    policy: CollisionPolicy,
) -> ImportOutcome {
    let mut outcome = ImportOutcome::default();
    for row in imported {
        let mut candidate = row.slot;
        let collision = existing
            .iter()
            .position(|e| !e.host.trim().is_empty() && e.alias == candidate.alias);
        match (collision, policy) {
            (None, _) => {
                outcome.added += 1;
                existing.push(candidate);
            }
            (Some(_), CollisionPolicy::Skip) => outcome.skipped += 1,
            (Some(i), CollisionPolicy::Overwrite) => {
                let slot = &mut existing[i];
                let identity_changed = slot.host.trim() != candidate.host.trim()
                    || slot.port.trim() != candidate.port.trim()
                    || slot.user.trim() != candidate.user.trim();
                let (auth, key_path) = match row.auth {
                    Some(a) => (a.kind, a.key_path),
                    None => (slot.auth, slot.key_path.clone()),
                };
                // The launch path deletes the keyring entry of a cleared
                // credential, so a secret for another machine does not
                // silently follow the alias (and TOFU may not warn).
                if identity_changed || auth != SlotAuth::Password {
                    slot.password.clear();
                    slot.save_password = false;
                }
                if identity_changed || auth != SlotAuth::Key || key_path.trim() != slot.key_path.trim() {
                    slot.passphrase.clear();
                    slot.save_passphrase = false;
                }
                slot.auth = auth;
                slot.key_path = key_path;
                slot.host = candidate.host;
                slot.port = candidate.port;
                slot.user = candidate.user;
                slot.tags = candidate.tags;
                outcome.overwritten += 1;
            }
            (Some(_), CollisionPolicy::Rename) => {
                candidate.alias = unique_alias(existing, &candidate.alias);
                outcome.renamed += 1;
                existing.push(candidate);
            }
        }
    }
    outcome
}

/// Parse an import file and either apply it (no collisions) or hand the
/// rows back for the user's collision choice (#416). Mutates nothing when
/// the file cannot be parsed.
fn stage_import(slots: &mut Vec<SshSlot>, path: &std::path::Path) -> Result<ImportStaged, String> {
    let ParsedImport { rows, kube_skipped } = parse_import_file(path)?;
    let collisions = find_import_collisions(slots, &rows);
    if collisions.is_empty() {
        let mut outcome = apply_import(slots, rows, CollisionPolicy::Skip);
        outcome.kube_skipped = kube_skipped;
        Ok(ImportStaged::Applied(outcome))
    } else {
        Ok(ImportStaged::NeedsDecision {
            rows,
            collisions,
            kube_skipped,
        })
    }
}

/// One-line import result for the launcher (#416).
fn import_status_line(outcome: &ImportOutcome) -> String {
    let mut line = format!(
        "Import: {} added, {} overwritten, {} renamed, {} skipped",
        outcome.added, outcome.overwritten, outcome.renamed, outcome.skipped
    );
    if outcome.kube_skipped > 0 {
        line.push_str(&format!("; {} Kubernetes cluster(s) skipped", outcome.kube_skipped));
    }
    line
}

fn export_status_line(count: usize, kube_skipped: usize, path: &std::path::Path) -> String {
    let mut line = format!("Export: {count} hosts → {}", path.display());
    if kube_skipped > 0 {
        line.push_str(&format!("; {kube_skipped} Kubernetes cluster(s) skipped"));
    }
    line
}

/// Serialize a host list to the import format (#417) — one `[[ssh_targets]]`
/// entry per host, shaped exactly like the launcher's `Import…` and
/// `config.toml` read it, so the file can be committed, shared, and
/// imported back as-is.
///
/// Secrets are never written (invariant #3): the auth block carries its
/// `type` and, for key login, the key file `path` (#489) — a location, not
/// a key. Passwords and passphrases have no representation in the format.
fn export_hosts_toml(slots: &[&SshSlot]) -> Result<String, String> {
    #[derive(serde::Serialize)]
    struct File {
        ssh_targets: Vec<Entry>,
    }
    #[derive(serde::Serialize)]
    struct Entry {
        name: String,
        host: String,
        port: u16,
        user: String,
        tags: Vec<String>,
        auth: Auth,
    }
    #[derive(serde::Serialize)]
    struct Auth {
        #[serde(rename = "type")]
        kind: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    }
    let file = File {
        ssh_targets: slots
            .iter()
            .enumerate()
            .map(|(i, slot)| {
                let port = slot.port.trim().parse::<u16>().unwrap_or(22);
                Entry {
                    name: filar_core::ssh_target_display_name(i, &slot.alias),
                    host: slot.host.trim().to_string(),
                    port: if port == 0 { 22 } else { port },
                    user: slot.user.trim().to_string(),
                    tags: parse_tags(&slot.tags),
                    auth: Auth {
                        kind: slot.auth.type_name(),
                        path: match slot.auth {
                            SlotAuth::Key => slot_key_path(&slot.key_path)
                                .map(|p| p.to_string_lossy().into_owned()),
                            _ => None,
                        },
                    },
                }
            })
            .collect(),
    };
    let body = toml::to_string(&file).map_err(|e| format!("cannot serialize host list: {e}"))?;
    Ok(format!(
        "# Filar host list — addresses, ports, users, tags and login methods.\n\
         # No passwords, passphrases or keys; key login lists the key file path.\n\
         # Import it back with the launcher's \"Import…\" button.\n\
         {body}"
    ))
}

/// Write the `[llm]` settings and the `[[host_groups]]` definitions to
/// `{OS data dir}/filar/config.toml` so `filar` invoked without the GUI
/// launcher still picks them up.
///
/// If a `config.toml` already exists, unrelated sections are preserved.
fn save_config_toml(
    settings: &Settings,
    host_groups: &[filar_core::HostGroup],
    risk_scorer: Option<&filar_core::RiskScorerConfig>,
    log_sources: &[filar_core::LogSourceConfig],
) {
    let base = match filar_core::default_base_dir() {
        Ok(b) => b,
        Err(_) => return,
    };
    let app_dir = base.join("filar");
    let path = app_dir.join("config.toml");

    // Load existing config to preserve non-LLM sections.
    let mut config: filar_core::Config = if path.exists() {
        match filar_core::Config::load(&path) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "config.toml exists but is invalid, not overwriting");
                return;
            }
        }
    } else {
        filar_core::Config::default()
    };

    // Update primary LLM section (backward compat) only. Launch-specific
    // data (profiles, ssh_targets, save_dir) is passed to the TUI via
    // `pending_launch.json` (#255) — no longer duplicated into config.toml.
    if !settings.model.is_empty() {
        config.llm.model = settings.model.clone();
    }
    if !settings.api_base_url.is_empty() {
        config.llm.api_base_url = settings.api_base_url.clone();
    }
    if !settings.temperature.is_empty() {
        config.llm.temperature = settings.temperature.parse().ok();
    }
    if !settings.extra_body.is_empty() {
        config.llm.extra_body = serde_json::from_str(&settings.extra_body).ok();
    }
    config.arbiter_profile = settings.arbiter_profile.clone();
    // `[[host_groups]]` is written too (#418): the Groups tab owns the
    // definitions, so the section is replaced wholesale — a removed group
    // must stay removed. The launch-specific sections (llm_profiles,
    // ssh_targets, save_dir) are intentionally left untouched: the GUI
    // passes them via `pending_launch.json` (#255) and must NOT clear them,
    // because direct-TUI launches read them from config.toml as fallback.
    config.host_groups = host_groups.to_vec();
    // The launcher owns `[[kube_clusters]]` too (#566): replace the section
    // wholesale so a removed cluster stays removed. Only target metadata is
    // written; kubeconfig contents and credentials never enter this object.
    config.kube_clusters = settings.kube_profiles.clone();
    // `[risk_scorer]` likewise (#552): the launcher owns it, switched off =
    // no section. Never a key — only `key_env`.
    config.risk_scorer = risk_scorer.cloned();
    // `[[log_sources]]` likewise (#584): the Logs tab owns them; keys are
    // only named (`key_env`), never written.
    config.log_sources = log_sources.to_vec();

    if let Err(e) = std::fs::create_dir_all(&app_dir) {
        tracing::warn!(path = %app_dir.display(), error = %e, "failed to create config directory");
        return;
    }
    match toml::to_string_pretty(&config) {
        Ok(data) => {
            if let Err(e) = std::fs::write(&path, &data) {
                tracing::warn!(path = %path.display(), error = %e, "failed to save config.toml");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize config.toml");
        }
    }
}

// ---------------------------------------------------------------------------
// LauncherApp
// ---------------------------------------------------------------------------

/// Editable Kubernetes target. Only the kubeconfig path is held here; its
/// contents and credentials are read by the transport when needed.
#[derive(Clone, Debug)]
struct KubeSlot {
    id: u64,
    name: String,
    kubeconfig: String,
    context: String,
    contexts: Vec<String>,
    contexts_loaded_for: Option<String>,
    context_error: String,
    namespaces: String,
    default_namespace: String,
    timeout: String,
    tags: String,
    reveal_configmaps: Vec<String>,
    reveal_env: Vec<String>,
    debug_images: Vec<String>,
    diagnostic_ports: Vec<u16>,
    check_status: String,
    check_in_flight: bool,
}

impl KubeSlot {
    fn blank(id: u64) -> Self {
        Self {
            id,
            name: String::new(),
            kubeconfig: String::new(),
            context: String::new(),
            contexts: Vec::new(),
            contexts_loaded_for: None,
            context_error: String::new(),
            namespaces: String::new(),
            default_namespace: String::new(),
            timeout: filar_core::DEFAULT_KUBE_REQUEST_TIMEOUT_SECS.to_string(),
            tags: String::new(),
            reveal_configmaps: Vec::new(),
            reveal_env: Vec::new(),
            debug_images: Vec::new(),
            diagnostic_ports: Vec::new(),
            check_status: String::new(),
            check_in_flight: false,
        }
    }

    fn from_cluster(id: u64, cluster: &filar_core::KubeCluster) -> Self {
        Self {
            id,
            name: cluster.name.clone(),
            kubeconfig: cluster
                .kubeconfig
                .as_ref()
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            context: cluster.context.clone(),
            contexts: Vec::new(),
            contexts_loaded_for: None,
            context_error: String::new(),
            namespaces: cluster.namespaces.join(", "),
            default_namespace: cluster.default_namespace.clone().unwrap_or_default(),
            timeout: cluster.request_timeout_secs.to_string(),
            tags: cluster.tags.join(", "),
            reveal_configmaps: cluster.reveal_configmaps.clone(),
            reveal_env: cluster.reveal_env.clone(),
            debug_images: cluster.debug_images.clone(),
            diagnostic_ports: cluster.diagnostic_ports.clone(),
            check_status: String::new(),
            check_in_flight: false,
        }
    }

    fn is_blank(&self) -> bool {
        self.name.trim().is_empty()
            && self.kubeconfig.trim().is_empty()
            && self.context.trim().is_empty()
            && self.namespaces.trim().is_empty()
            && self.default_namespace.trim().is_empty()
            && self.tags.trim().is_empty()
    }

    fn to_cluster(&self) -> Result<filar_core::KubeCluster, String> {
        let timeout = self.timeout.trim().parse::<u64>().map_err(|_| {
            format!(
                "Kubernetes cluster '{}': timeout must be a whole number from 1 to {}",
                self.name.trim(),
                filar_core::MAX_KUBE_REQUEST_TIMEOUT_SECS
            )
        })?;
        let cluster = filar_core::KubeCluster {
            name: self.name.trim().to_string(),
            context: self.context.trim().to_string(),
            kubeconfig: (!self.kubeconfig.trim().is_empty())
                .then(|| std::path::PathBuf::from(self.kubeconfig.trim())),
            namespaces: parse_tags(&self.namespaces),
            default_namespace: (!self.default_namespace.trim().is_empty())
                .then(|| self.default_namespace.trim().to_string()),
            request_timeout_secs: timeout,
            tags: parse_tags(&self.tags),
            reveal_configmaps: self.reveal_configmaps.clone(),
            reveal_env: self.reveal_env.clone(),
            debug_images: self.debug_images.clone(),
            diagnostic_ports: self.diagnostic_ports.clone(),
        };
        cluster.validate().map_err(|e| e.to_string())?;
        Ok(cluster)
    }

    /// Refresh the drop-down only after the path changes. This is a local file
    /// read; neither context discovery nor rendering contacts the API server.
    fn refresh_contexts(&mut self) {
        let path = self.kubeconfig.trim().to_string();
        if self.contexts_loaded_for.as_deref() == Some(path.as_str()) {
            return;
        }
        let explicit = (!path.is_empty()).then(|| std::path::PathBuf::from(&path));
        match filar_transport::KubeClusterApi::context_names(explicit.as_deref()) {
            Ok(contexts) => {
                self.contexts = contexts;
                self.context_error.clear();
            }
            Err(e) => {
                self.contexts.clear();
                self.context_error = e.to_string();
            }
        }
        self.contexts_loaded_for = Some(path);
    }
}

struct KubeCheckResult {
    slot_id: u64,
    target: filar_core::KubeCluster,
    result: Result<String, String>,
}

/// Perform the launcher's single Kubernetes API request. A namespaced pod
/// list avoids the cluster-wide RBAC fallback and cannot turn into retries.
async fn check_kubernetes_target(target: &filar_core::KubeCluster) -> Result<String, String> {
    let api = filar_transport::KubeClusterApi::connect(target)
        .await
        .map_err(|e| e.to_string())?;
    let namespace = target
        .default_namespace
        .as_deref()
        .or_else(|| target.namespaces.first().map(String::as_str))
        .unwrap_or("default");
    let pods = api
        .list_pods(Some(namespace), None, None)
        .await
        .map_err(|e| e.to_string())?;
    Ok(format!(
        "Available: {} pod(s) in {namespace}",
        pods.len()
    ))
}

struct SshSlot {
    host: String,
    port: String,
    user: String,
    alias: String,
    /// Comma-separated tags as typed in the Tags field (#413).
    tags: String,
    /// Login method (#489).
    auth: SlotAuth,
    password: String,
    save_password: bool,
    /// Key file for key login, as typed; empty = the default key.
    key_path: String,
    /// Passphrase of an encrypted key — held in memory and the OS
    /// credential store only (#489).
    passphrase: String,
    save_passphrase: bool,
}

/// Redacts the password and the key passphrase: `Debug` output can reach a
/// test failure message via `ImportStaged`, and a slot must never print its
/// secrets (#416 review, #489).
impl std::fmt::Debug for SshSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SshSlot")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("alias", &self.alias)
            .field("tags", &self.tags)
            .field("auth", &self.auth)
            .field("password", &"<redacted>")
            .field("save_password", &self.save_password)
            .field("key_path", &self.key_path)
            .field("passphrase", &"<redacted>")
            .field("save_passphrase", &self.save_passphrase)
            .finish()
    }
}

impl SshSlot {
    fn from_profile(p: &SshProfile, slot_idx: usize) -> Self {
        let auth = p.auth.unwrap_or_else(SlotAuth::legacy);
        let password = if auth == SlotAuth::Password && p.save_password {
            load_secret(&ssh_cred_name(slot_idx, &p.alias))
        } else {
            String::new()
        };
        let passphrase = if auth == SlotAuth::Key && p.save_passphrase {
            load_secret(&slot_passphrase_cred_name(slot_idx, &p.alias))
        } else {
            String::new()
        };
        Self {
            host: p.host.clone(),
            port: if p.port.is_empty() {
                "22".to_string()
            } else {
                p.port.clone()
            },
            user: p.user.clone(),
            alias: p.alias.clone(),
            tags: p.tags.clone(),
            auth,
            password,
            save_password: p.save_password,
            key_path: p.key_path.clone(),
            passphrase,
            save_passphrase: p.save_passphrase,
        }
    }

    fn to_profile(&self) -> SshProfile {
        SshProfile {
            host: self.host.clone(),
            port: self.port.clone(),
            user: self.user.clone(),
            alias: self.alias.trim().chars().take(32).collect(),
            tags: self.tags.trim().to_string(),
            save_password: self.save_password,
            auth: Some(self.auth),
            key_path: self.key_path.trim().to_string(),
            save_passphrase: self.save_passphrase,
        }
    }

    /// A blank entry backing the "Add host" button. New hosts log in
    /// through the SSH agent, like `SshAuth::default()` (#489).
    fn blank() -> Self {
        Self {
            host: String::new(),
            port: "22".to_string(),
            user: String::new(),
            alias: String::new(),
            tags: String::new(),
            auth: SlotAuth::default(),
            password: String::new(),
            save_password: false,
            key_path: String::new(),
            passphrase: String::new(),
            save_passphrase: false,
        }
    }

    /// The key file this slot logs in with: the typed path, or the default
    /// key when the field is blank.
    fn resolved_key_path(&self) -> std::path::PathBuf {
        filar_transport::resolve_key_path(slot_key_path(&self.key_path).as_deref())
    }
}

/// OS credential-store name of a slot's key passphrase:
/// `ssh_key_passphrase:<name>`, the name the session looks up (#489).
fn slot_passphrase_cred_name(slot: usize, alias: &str) -> String {
    filar_core::ssh_key_passphrase_name(&filar_core::ssh_target_display_name(slot, alias))
}

/// Row actions from the SSH host list, applied after the render pass so the
/// list is not mutated while it is being drawn.
enum HostAction {
    Up(usize),
    Down(usize),
    Remove(usize),
}

/// Row actions from the Groups tab (#418), applied after the render pass
/// like [`HostAction`]: one pointer delivers at most one click per frame.
enum GroupAction {
    Up(usize),
    Down(usize),
    Remove(usize),
}

/// A staged host import waiting for the collision choice (#416).
struct ImportDialog {
    /// Parsed rows (colliding and fresh ones), auth type included (#417).
    rows: Vec<ImportRow>,
    /// Imported names that already exist in the host list.
    collisions: Vec<String>,
    /// Cluster entries are not part of the SSH inventory format (#566).
    kube_skipped: usize,
}

/// Session label for a GUI launch (#406).
///
/// `target_mode == 0` → `"local"`; an SSH slot → its display name via
/// [`filar_core::ssh_target_display_name`] (alias, or `SSH{n}` for an empty
/// one) — the same name as the keyring entry and the saved config target, so
/// the session line, `SessionMeta.target` and the per-target export folder
/// (#400) all read the alias. The transport literal `"ssh"` must never leak
/// into the label again.
fn launch_target_name(target_mode: usize, slots: &[SshSlot]) -> String {
    if target_mode == 0 {
        "local".to_string()
    } else {
        let idx = target_mode - 1;
        filar_core::ssh_target_display_name(idx, &slots[idx].alias)
    }
}

// ---------------------------------------------------------------------------
// Command safety scorer (#552)
// ---------------------------------------------------------------------------

/// Launcher working copy of `[risk_scorer]` (#552): the command safety
/// scorer, separate from the LLM profiles and the arbiter — its own address,
/// model and key. No `Debug`: it holds a typed key.
#[derive(Clone, PartialEq)]
struct RiskScorerDraft {
    enabled: bool,
    /// `true` = `chat-logprobs`, `false` = Decisions API.
    chat_logprobs: bool,
    base_url: String,
    model: String,
    /// The server needs no key (`key_env = ""`).
    keyless: bool,
    /// Name the key is stored under; kept when "no key" is toggled back.
    key_env: String,
    /// A newly typed key — saved to the OS store at launch, never to a file.
    key: String,
    /// A key is already stored under `key_checked_for` (checked at
    /// start-up and when the key name is edited, not on every frame).
    key_saved: bool,
    /// The key name `key_saved` was checked under.
    key_checked_for: String,
    warn_below: String,
    danger_below: String,
    /// Not edited in the launcher; carried over from `config.toml`.
    timeout_secs: u64,
    /// Show the threshold fields.
    advanced_open: bool,
}

/// Default Decisions API address offered when the field is empty.
const RISK_DECISIONS_BASE_URL: &str = "https://openrouter.ai/api/alpha";
/// Model suggested for the Decisions API.
const RISK_DECISIONS_MODEL_HINT: &str = "typesafe/jev-1.14";

impl RiskScorerDraft {
    /// From `config.toml`'s section; `key_saved` is filled in by the caller.
    fn from_config(cfg: Option<&filar_core::RiskScorerConfig>) -> Self {
        let d = filar_core::RiskScorerConfig::default();
        let c = cfg.unwrap_or(&d);
        Self {
            enabled: cfg.is_some(),
            chat_logprobs: filar_core::RiskScorerKind::parse(&c.kind)
                == Some(filar_core::RiskScorerKind::ChatLogprobs),
            base_url: c.base_url.clone(),
            model: c.model.clone(),
            keyless: cfg.is_some() && c.key_env.trim().is_empty(),
            key_env: if c.key_env.trim().is_empty() {
                filar_core::config::default_risk_key_env()
            } else {
                c.key_env.trim().to_string()
            },
            key: String::new(),
            key_saved: false,
            key_checked_for: String::new(),
            warn_below: c.warn_below.to_string(),
            danger_below: c.danger_below.to_string(),
            timeout_secs: c.timeout_secs,
            advanced_open: false,
        }
    }

    /// Switch the type. Leaving Decisions drops its auto-filled address, so
    /// a chat scorer is never pointed at the Decisions endpoint unnoticed;
    /// an address the user typed is kept.
    fn set_chat_logprobs(&mut self, chat: bool) {
        if chat && !self.chat_logprobs && self.base_url.trim() == RISK_DECISIONS_BASE_URL {
            self.base_url.clear();
        }
        self.chat_logprobs = chat;
    }

    /// Tick or untick "no key". Ticking drops a typed key (it must not be
    /// saved later); unticking reads the name's status if it was never
    /// checked (a keyless section skips the start-up read).
    fn set_keyless(&mut self, keyless: bool) {
        self.keyless = keyless;
        if keyless {
            self.key.clear();
        } else if self.key_env.trim() != self.key_checked_for {
            self.refresh_key_saved();
        }
    }

    /// Whether a key is stored under the name as it reads now. A name not
    /// checked yet (being edited) reads as "not set" until it is.
    fn key_saved_now(&self) -> bool {
        self.key_saved && self.key_env.trim() == self.key_checked_for
    }

    /// Look the key name up in the OS store once (no-op in unit tests).
    fn refresh_key_saved(&mut self) {
        let name = self.key_env.trim().to_string();
        self.key_saved = !name.is_empty() && !load_secret(&name).is_empty();
        self.key_checked_for = name;
    }

    /// The `key_env` the section gets: empty for a keyless server.
    fn effective_key_env(&self) -> String {
        if self.keyless {
            String::new()
        } else {
            self.key_env.trim().to_string()
        }
    }

    /// The section to write and launch with — `None` when switched off —
    /// or the first problem, worded for the launcher. Never contains a key.
    fn to_config(&self) -> Result<Option<filar_core::RiskScorerConfig>, String> {
        if !self.enabled {
            return Ok(None);
        }
        let percent = |raw: &str, name: &str| -> Result<u8, String> {
            raw.trim()
                .parse::<u8>()
                .ok()
                .filter(|v| *v <= 100)
                .ok_or_else(|| format!("Command safety: {name} must be a whole number from 0 to 100"))
        };
        let cfg = filar_core::RiskScorerConfig {
            kind: if self.chat_logprobs { "chat-logprobs" } else { "decisions" }.to_string(),
            base_url: self.base_url.trim().to_string(),
            model: self.model.trim().to_string(),
            key_env: self.effective_key_env(),
            timeout_secs: self.timeout_secs,
            warn_below: percent(&self.warn_below, "warn below")?,
            danger_below: percent(&self.danger_below, "danger below")?,
        };
        if cfg.base_url.is_empty() {
            return Err("Command safety: enter the address".into());
        }
        if !(cfg.base_url.starts_with("http://") || cfg.base_url.starts_with("https://")) {
            return Err("Command safety: the address must start with http:// or https://".into());
        }
        if cfg.model.is_empty() {
            return Err("Command safety: enter the model".into());
        }
        if !self.keyless && cfg.key_env.is_empty() {
            return Err("Command safety: enter the key name, or tick \"no key\"".into());
        }
        cfg.validate()
            .map_err(|e| format!("Command safety: {}", e.to_string().trim_start_matches("config error: ")))?;
        Ok(Some(cfg))
    }

    /// What pending_launch carries for this draft (it is validated first).
    fn to_launch(&self) -> Result<RiskScorerLaunch, String> {
        Ok(match self.to_config()? {
            Some(config) => RiskScorerLaunch::On { config },
            None => RiskScorerLaunch::Off,
        })
    }

    /// A key to store at launch: `(key_env, key)` when one was typed.
    fn key_to_save(&self) -> Option<(String, String)> {
        let key = sanitize_secret_clipboard(&self.key);
        let env = self.effective_key_env();
        (self.enabled && !env.is_empty() && !key.is_empty()).then_some((env, key))
    }
}

// ---------------------------------------------------------------------------
// Log sources (#584)
// ---------------------------------------------------------------------------

/// Launcher working copy of one `[[log_sources]]` entry (#584). Numbers and
/// lists are kept as typed text, like the other launcher fields. No `Debug`:
/// it holds a typed key.
#[derive(Clone, PartialEq)]
struct LogSourceDraft {
    /// Stable id of the row (for check results).
    id: u64,
    name: String,
    kind: filar_core::LogSourceKind,
    url: String,
    auth: filar_core::LogAuth,
    /// Name the key is stored under (OS store / environment).
    key_env: String,
    /// A newly typed key — saved to the OS store at launch, never to a file.
    key: String,
    /// A key is already stored under `key_checked_for`.
    key_saved: bool,
    /// The key name `key_saved` was checked under.
    key_checked_for: String,
    user: String,
    tenant: String,
    index: String,
    /// `k8s_fields` preset name; empty = the usual one for the kind.
    k8s_fields: String,
    max_window_hours: String,
    max_lines: String,
    timeout_secs: String,
    /// Comma-separated `[[kube_clusters]]` names.
    clusters: String,
    /// Comma-separated tags.
    tags: String,
    /// Fields the launcher does not edit (`time_field`, `fields`, `ca_file`,
    /// byte caps), carried over from `config.toml`.
    base: filar_core::LogSourceConfig,
    /// Result of the last Check.
    check_status: String,
    check_in_flight: bool,
}

/// The key name suggested for a source: `FILAR_LOGS_<NAME>`.
fn suggested_log_key_env(name: &str) -> String {
    let upper: String = name
        .trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    format!("FILAR_LOGS_{}", if upper.is_empty() { "SOURCE".into() } else { upper })
}

fn split_list(text: &str) -> Vec<String> {
    text.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned).collect()
}

impl LogSourceDraft {
    /// A new, empty VictoriaLogs row.
    fn blank(id: u64) -> Self {
        let mut d = Self::from_config(
            &filar_core::LogSourceConfig {
                kind: "victorialogs".into(),
                ..Default::default()
            },
            id,
        );
        d.key_env.clear();
        d
    }

    /// From a `config.toml` entry; `key_saved` is filled in by the caller.
    fn from_config(cfg: &filar_core::LogSourceConfig, id: u64) -> Self {
        Self {
            id,
            name: cfg.name.clone(),
            kind: cfg.source_kind().unwrap_or(filar_core::LogSourceKind::VictoriaLogs),
            url: cfg.url.clone(),
            auth: cfg.auth_kind().unwrap_or(filar_core::LogAuth::None),
            key_env: cfg.key_env.clone(),
            key: String::new(),
            key_saved: false,
            key_checked_for: String::new(),
            user: cfg.user.clone().unwrap_or_default(),
            tenant: cfg.tenant.clone().unwrap_or_default(),
            index: cfg.index.clone().unwrap_or_default(),
            k8s_fields: cfg.k8s_fields.clone().unwrap_or_default(),
            max_window_hours: cfg.max_window_hours.to_string(),
            max_lines: cfg.max_lines.to_string(),
            timeout_secs: cfg.timeout_secs.to_string(),
            clusters: cfg.clusters.join(", "),
            tags: cfg.tags.join(", "),
            base: cfg.clone(),
            check_status: String::new(),
            check_in_flight: false,
        }
    }

    /// Whether a key is stored under the name as it reads now.
    fn key_saved_now(&self) -> bool {
        self.key_saved && self.key_env.trim() == self.key_checked_for
    }

    /// Look the key name up in the OS store once (no-op in unit tests).
    fn refresh_key_saved(&mut self) {
        let name = self.key_env.trim().to_string();
        self.key_saved = !name.is_empty() && !load_secret(&name).is_empty();
        self.key_checked_for = name;
    }

    /// The entry to write and launch with, or the first problem, worded for
    /// the launcher. Never contains a key.
    fn to_config(&self) -> Result<filar_core::LogSourceConfig, String> {
        let label = if self.name.trim().is_empty() { "new source".to_string() } else { self.name.trim().to_string() };
        let number = |raw: &str, field: &str| -> Result<u64, String> {
            raw.trim()
                .parse::<u64>()
                .map_err(|_| format!("Log source {label}: {field} must be a whole number"))
        };
        let opt = |s: &str| (!s.trim().is_empty()).then(|| s.trim().to_string());
        let search = matches!(self.kind, filar_core::LogSourceKind::OpenSearch | filar_core::LogSourceKind::Elasticsearch);
        let cfg = filar_core::LogSourceConfig {
            name: self.name.trim().to_string(),
            kind: self.kind.as_str().to_string(),
            url: self.url.trim().to_string(),
            auth: self.auth.as_str().to_string(),
            key_env: if self.auth.needs_key() { self.key_env.trim().to_string() } else { String::new() },
            user: if self.auth == filar_core::LogAuth::Basic { opt(&self.user) } else { None },
            tenant: if search { None } else { opt(&self.tenant) },
            index: if search { opt(&self.index) } else { None },
            k8s_fields: opt(&self.k8s_fields),
            max_window_hours: number(&self.max_window_hours, "max window (hours)")?,
            max_lines: number(&self.max_lines, "max lines")? as usize,
            timeout_secs: number(&self.timeout_secs, "timeout (s)")?,
            clusters: split_list(&self.clusters),
            tags: split_list(&self.tags),
            time_field: if search { self.base.time_field.clone() } else { None },
            fields: if search { self.base.fields.clone() } else { Vec::new() },
            ..self.base.clone()
        };
        cfg.validate()
            .map_err(|e| format!("Log source {label}: {}", e.to_string().trim_start_matches("config error: ").trim_start_matches("log_sources: ")))?;
        Ok(cfg)
    }

    /// A key to store at launch: `(key_env, key)` when one was typed.
    fn key_to_save(&self) -> Option<(String, String)> {
        let key = sanitize_secret_clipboard(&self.key);
        let env = self.key_env.trim().to_string();
        (self.auth.needs_key() && !env.is_empty() && !key.is_empty()).then_some((env, key))
    }

    /// The key for a Check: the typed one, else the stored one, else the
    /// environment. Empty when the source needs none.
    fn key_for_check(&self) -> String {
        if !self.auth.needs_key() {
            return String::new();
        }
        let typed = sanitize_secret_clipboard(&self.key);
        if !typed.is_empty() {
            return typed;
        }
        let name = self.key_env.trim();
        let stored = load_secret(name);
        if !stored.is_empty() {
            return stored;
        }
        std::env::var(name).unwrap_or_default()
    }
}

/// The query a Check sends: anything at all in the last 5 minutes.
fn log_check_query(cfg: &filar_core::LogSourceConfig) -> String {
    match cfg.source_kind() {
        Some(filar_core::LogSourceKind::Loki) => {
            let label = cfg.k8s_preset().map(|p| p.fields().namespace).unwrap_or("namespace");
            format!("{{{label}=~\".+\"}}")
        }
        _ => "*".to_string(),
    }
}

/// One Check of a log source: a query over the last 5 minutes with limit 1.
async fn check_log_source(cfg: &filar_core::LogSourceConfig, key: &str) -> Result<String, String> {
    let source = filar_agent::logs::build_log_source(cfg, key).map_err(|e| e.to_string())?;
    let end = chrono::Utc::now();
    let query = filar_agent::LogQuery {
        query: log_check_query(cfg),
        start: end - chrono::Duration::minutes(5),
        end,
        limit: 1,
    };
    let result = source.query(&query).await.map_err(|e| e.to_string())?;
    Ok(if result.lines.is_empty() && result.series.is_empty() {
        "Available, but no lines in the last 5 minutes.".to_string()
    } else {
        "Available, data found.".to_string()
    })
}

struct LogCheckResult {
    id: u64,
    config: filar_core::LogSourceConfig,
    result: Result<String, String>,
}

// ---------------------------------------------------------------------------
// Host groups (#418)
// ---------------------------------------------------------------------------

/// Editable host group in the Groups tab (#418).
///
/// Numeric limits are kept as typed strings, like the LLM profile fields: a
/// half-typed value must not be normalised to the default under the user's
/// fingers.
#[derive(Clone, Debug)]
struct HostGroupDraft {
    name: String,
    /// Comma-separated tags; a host must carry **all** of them to match.
    match_tags: String,
    policy: filar_core::HostGroupPolicy,
    /// Parallelism limit; empty = the built-in default.
    max_parallel: String,
    /// Per-host command deadline in seconds; empty = the built-in default.
    per_host_timeout_secs: String,
    /// Kubernetes selection is preserved while editing the other fields.
    kube: Option<filar_core::KubeGroup>,
    /// LLM profile name; empty = the session profile.
    llm_profile: String,
}

impl HostGroupDraft {
    /// Build an editable row from a persisted group: tags are joined with
    /// `, ` like the host tag fields, limits become their text form.
    fn from_group(group: &filar_core::HostGroup) -> Self {
        Self {
            name: group.name.clone(),
            match_tags: group.match_tags.join(", "),
            policy: group.policy,
            max_parallel: group.max_parallel.to_string(),
            per_host_timeout_secs: group.per_host_timeout_secs.to_string(),
            kube: group.kube.clone(),
            llm_profile: group.llm_profile.clone().unwrap_or_default(),
        }
    }

    /// A row with neither a name nor a rule is an in-UI scratch entry: it is
    /// not persisted, like a blank host row (#411/#418).
    fn is_blank(&self) -> bool {
        self.name.trim().is_empty() && self.match_tags.trim().is_empty()
    }

    /// Convert into the config form, validating it (#418).
    ///
    /// Empty limit fields mean "use the default"; a typed value must be a
    /// positive integer — silently coercing a typo into a limit would be
    /// worse than refusing the launch.
    fn to_group(&self) -> Result<filar_core::HostGroup, String> {
        let defaults = filar_core::HostGroup::default();
        let name = clean_tag(&self.name);
        if name.is_empty() {
            return Err("name must not be empty".to_string());
        }
        let max_parallel = match parse_group_limit(&self.max_parallel, "max_parallel")? {
            None => defaults.max_parallel,
            Some(v) => u32::try_from(v).map_err(|_| "max_parallel is too large".to_string())?,
        };
        let per_host_timeout_secs =
            parse_group_limit(&self.per_host_timeout_secs, "per_host_timeout_secs")?
                .unwrap_or(defaults.per_host_timeout_secs);
        let llm_profile = clean_tag(&self.llm_profile);
        Ok(filar_core::HostGroup {
            name,
            match_tags: parse_tags(&self.match_tags),
            policy: self.policy,
            max_parallel,
            per_host_timeout_secs,
            kube: self.kube.clone(),
            llm_profile: if llm_profile.is_empty() {
                None
            } else {
                Some(llm_profile)
            },
        })
    }
}

/// Parse an optional positive limit from a Groups-tab text field (#418):
/// empty = the group default; a positive integer = the typed value; anything
/// else (zero included) is an error.
fn parse_group_limit(raw: &str, field: &str) -> Result<Option<u64>, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<u64>() {
        Ok(v) if v > 0 => Ok(Some(v)),
        _ => Err(format!("{field} must be a positive integer")),
    }
}

/// The composition preview shown under a group rule (#418): which hosts the
/// rule selects right now, against the same target list a launch would use.
///
/// An empty rule and a rule that matches nobody are stated explicitly, not
/// as errors — they are facts about the current tag picture, not mistakes.
/// An empty rule selects **nothing** (`select_hosts_for_group`): an
/// unfinished definition must never look like the whole fleet.
fn group_preview_line(match_tags: &str, targets: &[filar_core::SshTarget]) -> String {
    let tags = parse_tags(match_tags);
    if tags.is_empty() {
        return "Effective now: 0 hosts — the rule lists no tags yet.".to_string();
    }
    let group = filar_core::HostGroup {
        match_tags: tags,
        ..Default::default()
    };
    let matched = filar_core::select_hosts_for_group(&group, targets);
    if matched.is_empty() {
        return "Effective now: 0 hosts — no host carries all of these tags.".to_string();
    }
    let shown: Vec<&str> = matched.iter().map(|t| t.name.as_str()).take(6).collect();
    let mut line = format!(
        "Effective now: {} host(s) — {}",
        matched.len(),
        shown.join(", ")
    );
    if matched.len() > shown.len() {
        line.push_str(&format!(", +{} more", matched.len() - shown.len()));
    }
    line
}

struct LauncherApp {
    sessions: Vec<SessionMeta>,
    selected_session: Option<usize>,
    target_mode: usize,
    /// Selected Kubernetes row. When set, `target_mode` is only the remembered
    /// SSH selection and does not describe the launch target.
    selected_kube: Option<usize>,
    ssh_slots: Vec<SshSlot>,
    kube_slots: Vec<KubeSlot>,
    next_kube_slot_id: u64,
    kube_check_tx: std::sync::mpsc::Sender<KubeCheckResult>,
    kube_check_rx: std::sync::mpsc::Receiver<KubeCheckResult>,
    /// Manual `[[ssh_targets]]` from the `config.toml` the GUI process
    /// loaded, passwords stripped ([`strip_manual_ssh_passwords`]) — merged
    /// after the launcher's own targets at launch (#412).
    manual_targets: Vec<filar_core::SshTarget>,
    /// All configured profiles.
    profiles: Vec<LlmProfileData>,
    /// Currently selected profile index.
    selected_profile: usize,
    validation_error: String,
    /// Whether `validation_error` currently holds a profile-field message.
    ///
    /// The slot is shared with the session/SSH-target warning, so a successful
    /// save clears only what profile validation itself put there instead of
    /// wiping an unrelated warning.
    profile_error_shown: bool,
    /// Directory for Ctrl+S session exports (`None` = CWD).
    save_dir: Option<std::path::PathBuf>,
    /// Reveal SSH password field (not persisted).
    show_ssh_password: bool,
    /// Reveal the key passphrase field — its own flag, so revealing a
    /// password never reveals a passphrase (#489 review). Not persisted.
    show_key_passphrase: bool,
    /// Reveal API key field (not persisted).
    show_api_key: bool,
    /// Arbiter profile selection (`None` = same as session profile).
    arbiter_profile: Option<String>,
    /// Command safety scorer settings (#552).
    risk_scorer: RiskScorerDraft,
    /// Working copies of `[[log_sources]]` (#584).
    log_sources: Vec<LogSourceDraft>,
    next_log_source_id: u64,
    log_check_tx: std::sync::mpsc::Sender<LogCheckResult>,
    log_check_rx: std::sync::mpsc::Receiver<LogCheckResult>,
    /// Staged import waiting for the collision policy choice (#416).
    import_dialog: Option<ImportDialog>,
    /// Result of the last host-file operation — import or export (#416/#417),
    /// shown under the host list.
    file_status: String,
    /// Working copies of the `[[host_groups]]` definitions (#418).
    host_groups: Vec<HostGroupDraft>,
    /// Active central tab: 0 = session & hosts, 1 = groups (#418), 2 = log
    /// sources (#584).
    active_tab: usize,
    /// Last "is this key encrypted?" answer for the key login fields (#489).
    key_probe: Option<KeyProbe>,
}

/// Whether a key file needs a passphrase, cached per path so the launcher
/// does not re-read the file on every frame (#489).
struct KeyProbe {
    path: std::path::PathBuf,
    result: Result<filar_transport::KeyProtection, String>,
}

/// Local copy of an LLM profile for GUI editing.
#[derive(Clone)]
struct LlmProfileData {
    name: String,
    model: String,
    api_base_url: String,
    key_env: String,
    api_key: String,
    temperature: String,
    /// Upper bound on the length of one reply, as typed. Empty = the built-in
    /// default. A string like `temperature` so a half-typed value does not
    /// reset the field.
    max_tokens: String,
    /// Nucleus sampling probability, as typed. Empty = provider default.
    top_p: String,
    extra_body: String,
    /// Compaction threshold in prompt tokens, as typed. Empty = profile
    /// default; `0` disables compaction. Kept as a string like `temperature`
    /// so a half-typed value does not reset the field.
    compact_at_tokens: String,
}

impl LlmProfileData {
    /// Convert the edited fields into the profile stored in `settings.json`
    /// and handed to the TUI through `pending_launch.json`.
    ///
    /// A single conversion point on purpose. These lines used to be written
    /// out at four call sites, and a field missing from one of them was reset
    /// on every save without any error: that is how `max_tokens` and `top_p`
    /// were lost (#380).
    fn to_profile(&self) -> filar_core::LlmProfile {
        filar_core::LlmProfile {
            name: self.name.clone(),
            model: self.model.clone(),
            api_base_url: self.api_base_url.clone(),
            key_env: self.key_env.clone(),
            max_tokens: parse_max_tokens(&self.max_tokens),
            temperature: self.temperature.trim().parse().ok(),
            top_p: self.top_p.trim().parse().ok(),
            extra_body: serde_json::from_str(&self.extra_body).ok(),
            compact_at_tokens: parse_compact_at_tokens(&self.compact_at_tokens),
        }
    }

    /// Build the editor fields from a stored profile — the reverse of
    /// [`LlmProfileData::to_profile`].
    ///
    /// `api_key` is passed in because it lives in the OS credential store, not
    /// in the profile.
    fn from_profile(profile: &filar_core::LlmProfile, api_key: String) -> Self {
        Self {
            name: profile.name.clone(),
            model: profile.model.clone(),
            api_base_url: profile.api_base_url.clone(),
            key_env: profile.key_env.clone(),
            api_key,
            temperature: profile.temperature.map(|t| t.to_string()).unwrap_or_default(),
            max_tokens: profile.max_tokens.to_string(),
            top_p: profile.top_p.map(|t| t.to_string()).unwrap_or_default(),
            extra_body: profile
                .extra_body
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
            compact_at_tokens: profile.compact_at_tokens.to_string(),
        }
    }
}

/// Apply the dark theme, matching the TUI accent palette:
/// muted dark background, one accent colour (used for buttons and highlights),
/// readable grey scale for secondary text.
fn configure_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    // Accent — a teal/cyan that matches the TUI's mode colours.
    let accent = egui::Color32::from_rgb(0x3d, 0xb3, 0xb3);
    visuals.override_text_color = Some(egui::Color32::from_gray(0xe0));
    visuals.widgets.noninteractive.bg_fill = egui::Color32::from_gray(0x22);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(0x2a);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_gray(0x38);
    visuals.widgets.active.bg_fill = accent;
    visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_gray(0xc0);
    visuals.selection.bg_fill = accent.linear_multiply(0.3);
    ctx.set_visuals(visuals);
}

/// Parse `user@host[:port]` (the persisted `ssh_info` format) into
/// `(host, port)`. Supports bracketed IPv6 with an optional port.
fn parse_ssh_host_port(info: &str) -> Option<(String, u16)> {
    let (_, host_port) = info.split_once('@')?;
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let after = &rest[end + 1..];
        let port = if after.is_empty() {
            22
        } else {
            after.strip_prefix(':')?.parse().ok()?
        };
        (host.to_string(), port)
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), p.parse().ok()?),
            None => (host_port.to_string(), 22),
        }
    };
    if host.is_empty() {
        return None;
    }
    Some((host, port))
}

/// Fill a profile's model / API base URL fields from a session's launch
/// context (empty session values leave the profile untouched).
fn fill_profile_from_meta(p: &mut LlmProfileData, meta: &SessionMeta) {
    if let Some(model) = &meta.model {
        if !model.is_empty() {
            p.model = model.clone();
        }
    }
    if let Some(url) = &meta.api_base_url {
        if !url.is_empty() {
            p.api_base_url = url.clone();
        }
    }
}

impl LauncherApp {
    fn render_session_list(&mut self, ui: &mut egui::Ui) {
        ui.label("Recent sessions:");
        if self.sessions.is_empty() {
            ui.label("  (no saved sessions yet)");
            return;
        }
        let new_selected = self.selected_session.is_none();
        if ui
            .selectable_label(new_selected, "  + Start new session")
            .clicked()
        {
            self.selected_session = None;
        }
        let mut clicked: Option<usize> = None;
        egui::ScrollArea::vertical()
            .max_height(100.0)
            .show(ui, |ui| {
                for (i, session) in self.sessions.iter().enumerate() {
                    let selected = self.selected_session == Some(i);
                    let host = session
                        .ssh_info
                        .clone()
                        .unwrap_or_else(|| session.target.clone());
                    let model = session
                        .model
                        .clone()
                        .or_else(|| session.llm_profile.clone())
                        .unwrap_or_default();
                    let text = format!(
                        "  {} | {} | {} | {}",
                        session.timestamp, host, model, session.preview
                    );
                    if ui.selectable_label(selected, &text).clicked() {
                        clicked = Some(i);
                    }
                }
            });
        if let Some(i) = clicked {
            self.on_session_selected(i);
        }
    }

    /// Select a saved session and auto-configure the launcher from its launch
    /// context: SSH target (matched by `ssh_info`), LLM profile (matched by
    /// name), and the model / API base URL fields.
    fn on_session_selected(&mut self, i: usize) {
        self.selected_session = Some(i);
        let Some(meta) = self.sessions.get(i).cloned() else {
            return;
        };

        // Auto-select the LLM profile by name and fill its model / API base
        // URL from the session launch context. Only the matched profile is
        // touched when a name is present; without a name, the flat model/URL
        // is restored into the current selection.
        match &meta.llm_profile {
            Some(name) => {
                if let Some(idx) = self.profiles.iter().position(|p| p.name == *name) {
                    self.selected_profile = idx;
                    fill_profile_from_meta(&mut self.profiles[idx], &meta);
                }
            }
            None => {
                if let Some(p) = self.profiles.get_mut(self.selected_profile) {
                    fill_profile_from_meta(p, &meta);
                }
            }
        }

        // Auto-select the SSH target by ssh_info.
        match meta.ssh_info.as_deref().and_then(parse_ssh_host_port) {
            Some((host, port)) => {
                if let Some(slot_idx) = self.ssh_slots.iter().position(|s| {
                    let trimmed = s.port.trim();
                    let slot_port = if trimmed.is_empty() {
                        Some(22)
                    } else {
                        trimmed.parse::<u16>().ok()
                    };
                    s.host == host && slot_port == Some(port)
                }) {
                    self.target_mode = slot_idx + 1;
                    self.selected_kube = None;
                    self.clear_error();
                } else {
                    self.target_mode = 0;
                    self.selected_kube = None;
                    self.set_other_error(format!(
                        "No SSH profile matches '{}'. Select a target manually.",
                        meta.ssh_info.as_deref().unwrap_or_default()
                    ));
                }
            }
            None => {
                self.target_mode = 0;
                self.selected_kube = None;
                self.clear_error();
            }
        }
    }

    fn render_target_selector(&mut self, ui: &mut egui::Ui) {
        ui.label("Target:");
        let mut mode = self.target_mode;
        let mut kube_mode = self.selected_kube;
        if ui
            .radio(kube_mode.is_none() && mode == 0, "Local")
            .clicked()
        {
            mode = 0;
            kube_mode = None;
        }

        // While the collision dialog is open the host list must not change
        // under it — a second import would replace the staged one and
        // `apply_import` matches rows by alias (#416 review).
        let editing_locked = self.import_dialog.is_some();

        ui.horizontal(|ui| {
            ui.label(format!("SSH hosts ({}):", self.ssh_slots.len()));
            if ui
                .add_enabled(!editing_locked, egui::Button::new("+ Add host"))
                .on_hover_text("Add a host to the list")
                .clicked()
            {
                self.add_host();
                mode = self.target_mode;
                kube_mode = None;
            }
            if ui
                .add_enabled(!editing_locked, egui::Button::new("Import…"))
                .on_hover_text(
                    "Import addresses, ports, users, tags and auth types from a TOML or CSV file — secrets are never imported",
                )
                .clicked()
            {
                self.start_host_import();
            }
            let has_targets = self.ssh_slots.iter().any(|s| !s.host.trim().is_empty())
                || self.kube_slots.iter().any(|slot| !slot.is_blank());
            if ui
                .add_enabled(!editing_locked && has_targets, egui::Button::new("Export…"))
                .on_hover_text(
                    "Save SSH hosts to TOML; Kubernetes clusters are skipped with a notice",
                )
                .clicked()
            {
                self.start_host_export();
            }
        });
        if !self.file_status.is_empty() {
            ui.label(egui::RichText::new(&self.file_status).weak().small());
        }
        if self.ssh_slots.is_empty() {
            ui.label(egui::RichText::new("  (no saved hosts yet)").weak());
        } else {
            // One pointer delivers at most one click per frame, so a single
            // pending action is enough even when a row changes the indices.
            let mut action: Option<HostAction> = None;
            egui::ScrollArea::vertical()
                .id_salt("ssh_host_list")
                .max_height(140.0)
                .show(ui, |ui| {
                    for (i, slot) in self.ssh_slots.iter().enumerate() {
                        ui.horizontal(|ui| {
                            if ui
                                .radio(kube_mode.is_none() && mode == i + 1, "")
                                .clicked()
                            {
                                mode = i + 1;
                                kube_mode = None;
                            }
                            let alias = slot.alias.trim();
                            let label = if alias.is_empty() {
                                egui::RichText::new("(no alias)").weak()
                            } else if alias.chars().count() > 32 {
                                egui::RichText::new(format!(
                                    "{}…",
                                    alias.chars().take(31).collect::<String>()
                                ))
                            } else {
                                egui::RichText::new(alias.to_string())
                            };
                            ui.label(label);
                            let host = slot.host.trim();
                            let detail = if host.is_empty() {
                                "(no address)".to_string()
                            } else if slot.user.trim().is_empty() {
                                host.to_string()
                            } else {
                                format!("{}@{}", slot.user.trim(), host)
                            };
                            ui.label(egui::RichText::new(detail).weak());
                            if ui
                                .add_enabled(
                                    !editing_locked && i > 0,
                                    egui::Button::new("⬆").small(),
                                )
                                .on_hover_text("Move up")
                                .clicked()
                            {
                                action = Some(HostAction::Up(i));
                            }
                            if ui
                                .add_enabled(
                                    !editing_locked && i + 1 < self.ssh_slots.len(),
                                    egui::Button::new("⬇").small(),
                                )
                                .on_hover_text("Move down")
                                .clicked()
                            {
                                action = Some(HostAction::Down(i));
                            }
                            if ui
                                .add_enabled(!editing_locked, egui::Button::new("X"))
                                .on_hover_text("Remove host")
                                .clicked()
                            {
                                action = Some(HostAction::Remove(i));
                            }
                        });
                    }
                });
            let had_action = action.is_some();
            match action {
                Some(HostAction::Up(i)) => self.move_host_up(i),
                Some(HostAction::Down(i)) => self.move_host_down(i),
                Some(HostAction::Remove(i)) => self.remove_host(i),
                None => {}
            }
            if had_action {
                mode = self.target_mode;
            }
        }

        ui.horizontal(|ui| {
            ui.label(format!("Kubernetes clusters ({}):", self.kube_slots.len()));
            if ui.button("+ Add cluster").clicked() {
                self.add_kube();
                kube_mode = self.selected_kube;
            }
        });
        if self.kube_slots.is_empty() {
            ui.label(egui::RichText::new("  (no clusters yet)").weak());
        } else {
            let mut remove = None;
            for (i, slot) in self.kube_slots.iter().enumerate() {
                ui.horizontal(|ui| {
                    if ui.radio(kube_mode == Some(i), "").clicked() {
                        kube_mode = Some(i);
                    }
                    ui.label(if slot.name.trim().is_empty() {
                        "(no name)"
                    } else {
                        slot.name.trim()
                    });
                    ui.label(
                        egui::RichText::new(if slot.context.trim().is_empty() {
                            "(no context)"
                        } else {
                            slot.context.trim()
                        })
                        .weak(),
                    );
                    if ui.button("X").on_hover_text("Remove cluster").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if let Some(i) = remove {
                self.remove_kube(i);
                kube_mode = self.selected_kube;
            }
        }
        self.target_mode = mode;
        self.selected_kube = kube_mode;
    }

    fn render_ssh_fields(&mut self, ui: &mut egui::Ui) {
        if self.selected_kube.is_some() || self.target_mode == 0 {
            return;
        }
        // Editing connection fields is also a host-list mutation: a renamed
        // alias would make `apply_import` match different rows than the ones
        // the dialog is asking about (#416 review).
        let locked = self.import_dialog.is_some();
        let idx = self.target_mode - 1;
        let mut show = self.show_ssh_password;
        ui.add_enabled_ui(!locked, |ui| {
            let slot = &mut self.ssh_slots[idx];
            egui::Grid::new("ssh_grid")
                .num_columns(2)
                .spacing([10.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Host:");
                    ui.add(
                        egui::TextEdit::singleline(&mut slot.host)
                            .hint_text("192.168.1.100"),
                    );
                    ui.end_row();
                    ui.label("Port:");
                    ui.add(
                        egui::TextEdit::singleline(&mut slot.port)
                            .hint_text("22"),
                    );
                    ui.end_row();
                    ui.label("User:");
                    ui.add(
                        egui::TextEdit::singleline(&mut slot.user)
                            .hint_text("root"),
                    );
                    ui.end_row();
                    ui.label("Alias:");
                    ui.add(
                        egui::TextEdit::singleline(&mut slot.alias)
                            .hint_text("deploy (required)")
                            .desired_width(120.0),
                    );
                    ui.end_row();
                    ui.label("Tags:");
                    ui.add(
                        egui::TextEdit::singleline(&mut slot.tags)
                            .hint_text("prod, web (comma-separated)")
                            .desired_width(160.0),
                    );
                    ui.end_row();
                    ui.label("Login:");
                    ui.horizontal(|ui| {
                        ui.radio_value(&mut slot.auth, SlotAuth::Agent, "SSH agent");
                        ui.radio_value(&mut slot.auth, SlotAuth::Key, "Key file");
                        ui.radio_value(&mut slot.auth, SlotAuth::Password, "Password");
                    });
                    ui.end_row();
                    match slot.auth {
                        SlotAuth::Password => {
                            ui.label("Password:");
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut show, "Show");
                                secret_text_edit(ui, "ssh_password", &mut slot.password, show);
                            });
                            ui.end_row();
                        }
                        SlotAuth::Key => {
                            let default_key = filar_transport::resolve_key_path(None);
                            ui.label("Key file:");
                            ui.add(
                                egui::TextEdit::singleline(&mut slot.key_path)
                                    .hint_text(format!("{} (default)", default_key.display()))
                                    .desired_width(240.0),
                            );
                            ui.end_row();
                        }
                        SlotAuth::Agent => {}
                    }
                });
        });
        // Probed after the fields: an edited path is re-read on the next
        // frame, and only when it actually changed.
        let key_status = self.refresh_key_probe(idx);
        let hint = egui::Color32::from_rgb(140, 140, 140);
        let mut show_passphrase = self.show_key_passphrase;
        ui.add_enabled_ui(!locked, |ui| {
            let slot = &mut self.ssh_slots[idx];
            match slot.auth {
                SlotAuth::Password => {
                    ui.checkbox(
                        &mut slot.save_password,
                        "Save password (encrypted in OS credential store)",
                    );
                }
                SlotAuth::Key => match key_status {
                    Some(Ok(filar_transport::KeyProtection::Encrypted)) => {
                        ui.checkbox(
                            &mut slot.save_passphrase,
                            "Save passphrase (encrypted in OS credential store)",
                        );
                        if slot.save_passphrase {
                            ui.horizontal(|ui| {
                                ui.label("Passphrase:");
                                ui.checkbox(&mut show_passphrase, "Show");
                                secret_text_edit(
                                    ui,
                                    "ssh_key_passphrase",
                                    &mut slot.passphrase,
                                    show_passphrase,
                                );
                            });
                        } else {
                            ui.colored_label(
                                hint,
                                "The key is encrypted: its passphrase is asked in the terminal at launch.",
                            );
                        }
                    }
                    Some(Ok(filar_transport::KeyProtection::Plain)) => {
                        ui.colored_label(hint, "The key has no passphrase.");
                    }
                    Some(Err(e)) => {
                        ui.colored_label(egui::Color32::from_rgb(220, 150, 60), e);
                    }
                    None => {}
                },
                SlotAuth::Agent => {
                    ui.colored_label(
                        hint,
                        "Uses the keys loaded into your SSH agent (ssh-add). On Windows: \
                         the \"OpenSSH Authentication Agent\" service or Pageant.",
                    );
                }
            }
        });
        self.show_ssh_password = show;
        self.show_key_passphrase = show_passphrase;
    }

    fn render_kube_fields(&mut self, ui: &mut egui::Ui) {
        let Some(idx) = self.selected_kube else {
            return;
        };
        let Some(slot) = self.kube_slots.get_mut(idx) else {
            return;
        };
        slot.refresh_contexts();
        let mut check = false;
        egui::Grid::new("kube_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Name:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.name)
                        .hint_text("prod-eu (required)")
                        .desired_width(180.0),
                );
                ui.end_row();
                ui.label("Kubeconfig:");
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut slot.kubeconfig)
                            .hint_text("empty = $KUBECONFIG / ~/.kube/config")
                            .desired_width(220.0),
                    );
                    if ui.button("Browse…").clicked() {
                        if let Some(path) = rfd::FileDialog::new().pick_file() {
                            slot.kubeconfig = path.to_string_lossy().into_owned();
                        }
                    }
                });
                ui.end_row();
                ui.label("Context:");
                let selected = if slot.context.is_empty() {
                    "Select context"
                } else {
                    slot.context.as_str()
                };
                egui::ComboBox::from_id_salt(("kube_context", slot.id))
                    .selected_text(selected)
                    .show_ui(ui, |ui| {
                        for context in &slot.contexts {
                            ui.selectable_value(&mut slot.context, context.clone(), context);
                        }
                    });
                ui.end_row();
                ui.label("Namespaces:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.namespaces)
                        .hint_text("shop, pay (empty = discover)")
                        .desired_width(200.0),
                );
                ui.end_row();
                ui.label("Default namespace:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.default_namespace)
                        .hint_text("shop (empty = default)")
                        .desired_width(160.0),
                );
                ui.end_row();
                ui.label("Timeout, sec:");
                ui.add(egui::TextEdit::singleline(&mut slot.timeout).desired_width(60.0));
                ui.end_row();
                ui.label("Tags:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.tags)
                        .hint_text("prod, k8s (comma-separated)")
                        .desired_width(180.0),
                );
                ui.end_row();
                ui.label("Connection:");
                check = ui
                    .add_enabled(!slot.check_in_flight, egui::Button::new("Check"))
                    .on_hover_text("Make one read-only pod-list request")
                    .clicked();
                ui.end_row();
            });
        if !slot.context_error.is_empty() {
            ui.colored_label(egui::Color32::from_rgb(220, 150, 60), &slot.context_error);
        } else if slot.contexts.is_empty() {
            ui.label(egui::RichText::new("No contexts read from this kubeconfig.").weak());
        }
        if slot.check_in_flight {
            ui.label("Checking…");
        } else if !slot.check_status.is_empty() {
            ui.label(&slot.check_status);
        }
        if check {
            match slot.to_cluster() {
                Ok(target) => self.start_kube_check(idx, target),
                Err(msg) => self.set_other_error(msg),
            }
        }
    }

    fn start_kube_check(&mut self, idx: usize, target: filar_core::KubeCluster) {
        let Some(slot) = self.kube_slots.get_mut(idx) else {
            return;
        };
        slot.check_in_flight = true;
        slot.check_status.clear();
        let slot_id = slot.id;
        let tx = self.kube_check_tx.clone();
        std::thread::spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime.block_on(check_kubernetes_target(&target)),
                Err(e) => Err(format!("cannot start the Kubernetes check runtime: {e}")),
            };
            let _ = tx.send(KubeCheckResult {
                slot_id,
                target,
                result,
            });
        });
    }

    fn poll_kube_checks(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.kube_check_rx.try_recv() {
            if let Some(slot) = self
                .kube_slots
                .iter_mut()
                .find(|slot| slot.id == done.slot_id)
            {
                slot.check_in_flight = false;
                slot.check_status = if slot.to_cluster().ok().as_ref() == Some(&done.target) {
                    match done.result {
                        Ok(message) => message,
                        Err(message) => format!("Check failed: {message}"),
                    }
                } else {
                    "Check result discarded: the cluster fields changed.".to_string()
                };
            }
            ctx.request_repaint();
        }
    }

    /// Whether the selected slot's key file is encrypted, re-read only when
    /// its path changed (#489). `None` unless the slot uses key login.
    fn refresh_key_probe(
        &mut self,
        idx: usize,
    ) -> Option<Result<filar_transport::KeyProtection, String>> {
        let slot = self.ssh_slots.get(idx)?;
        if slot.auth != SlotAuth::Key {
            return None;
        }
        let path = slot.resolved_key_path();
        if self.key_probe.as_ref().map(|p| &p.path) != Some(&path) {
            let result = filar_transport::key_protection(&path).map_err(|e| e.to_string());
            self.key_probe = Some(KeyProbe { path, result });
        }
        self.key_probe.as_ref().map(|p| p.result.clone())
    }

    fn render_llm_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("Models");
        let profile_names: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
        if profile_names.is_empty() {
            ui.label("No profiles defined. Click Add to create one.");
            if ui.button("Add Profile").clicked() {
                self.profiles.push(LlmProfileData {
                    name: "default".into(), model: String::new(),
                    api_base_url: String::new(),
                    key_env: filar_core::secrets::env_vars::LLM_API_KEY.into(),
                    api_key: String::new(), temperature: String::new(),
                    max_tokens: String::new(), top_p: String::new(),
                    extra_body: String::new(), compact_at_tokens: String::new(),
                });
                self.selected_profile = 0;
                self.save_profiles();
            }
            return;
        }
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Profile")
                .selected_text(&profile_names[self.selected_profile])
                .show_ui(ui, |ui| {
                    for (i, name) in profile_names.iter().enumerate() {
                        if ui.selectable_label(i == self.selected_profile, name).clicked() {
                            self.selected_profile = i;
                        }
                    }
                });
            if ui.button("+").on_hover_text("Add profile").clicked() {
                let name = unique_profile_name(&self.profiles, "profile");
                let key_env = format!("api_key_{name}");
                self.profiles.push(LlmProfileData {
                    name, key_env,
                    model: String::new(), api_base_url: String::new(),
                    api_key: String::new(), temperature: String::new(),
                    max_tokens: String::new(), top_p: String::new(),
                    extra_body: String::new(), compact_at_tokens: String::new(),
                });
                self.selected_profile = self.profiles.len() - 1;
                self.save_profiles();
            }
            if self.profiles.len() > 1 && ui.button("X").on_hover_text("Delete profile").clicked() {
                // Validate what the list *would* be before touching anything.
                // `delete_secret` is irreversible and `save_profiles` may now
                // refuse to write, which would strand the profile in
                // settings.json with its API key already gone from the OS store.
                let idx = self.selected_profile;
                let remaining = self
                    .profiles
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| *i != idx)
                    .map(|(_, p)| p);
                match validate_all_profile_fields(remaining) {
                    Err(msg) => self.set_profile_error(msg),
                    Ok(()) => {
                        delete_secret(&self.profiles[idx].key_env);
                        self.profiles.remove(idx);
                        self.selected_profile = idx.min(self.profiles.len().saturating_sub(1));
                        self.save_profiles();
                    }
                }
            }
        });
        let mut show_api_key = self.show_api_key;
        {
            let p = &mut self.profiles[self.selected_profile];
            ui.horizontal(|ui| { ui.label("Name:"); ui.text_edit_singleline(&mut p.name); });
            ui.horizontal(|ui| { ui.label("Model:"); ui.text_edit_singleline(&mut p.model); });
            ui.horizontal(|ui| {
                ui.label("API URL:");
                ui.add(
                    egui::TextEdit::singleline(&mut p.api_base_url)
                        .hint_text("e.g. http://localhost:11434/v1")
                        .desired_width(280.0),
                );
            });
            ui.horizontal(|ui| {
                ui.label("API key:");
                ui.checkbox(&mut show_api_key, "Show");
                secret_text_edit(
                    ui,
                    ("api_key", self.selected_profile),
                    &mut p.api_key,
                    show_api_key,
                );
            });
            ui.horizontal(|ui| {
                ui.label("Key env:");
                ui.add(
                    egui::TextEdit::singleline(&mut p.key_env)
                        .hint_text("empty = no key (local)")
                        .desired_width(200.0),
                );
            });
            if p.key_env.trim().is_empty() {
                let url = if p.api_base_url.trim().is_empty() {
                    "(set API URL above)".to_string()
                } else {
                    p.api_base_url.trim().to_string()
                };
                ui.colored_label(
                    egui::Color32::from_rgb(180, 180, 100),
                    format!(
                        "No API key — requests go only to {url} (local / air-gapped)."
                    ),
                );
            } else if p.api_key.trim().is_empty() {
                ui.colored_label(
                    egui::Color32::from_rgb(140, 140, 140),
                    "API key empty: loaded from OS store / env on Launch, or clear Key env for local models.",
                );
            }
            ui.horizontal(|ui| { ui.label("Temp:"); ui.text_edit_singleline(&mut p.temperature); });
            ui.horizontal(|ui| {
                ui.label("Max tokens:");
                ui.add(
                    egui::TextEdit::singleline(&mut p.max_tokens)
                        .hint_text(format!("{} — default when empty", filar_core::DEFAULT_MAX_TOKENS))
                        .desired_width(160.0),
                )
                .on_hover_text(
                    "Upper bound on the length of a single reply, in tokens. \
                     Raise it if long answers are being cut off; leave empty \
                     for the default.",
                );
            });
            ui.horizontal(|ui| {
                ui.label("Top-p:");
                ui.add(
                    egui::TextEdit::singleline(&mut p.top_p)
                        .hint_text("provider default when empty")
                        .desired_width(160.0),
                )
                .on_hover_text(
                    "Nucleus sampling: above 0.0 and up to 1.0. Leave empty to \
                     let the provider decide.",
                );
            });
            ui.horizontal(|ui| {
                ui.label("Compact at:");
                ui.add(
                    egui::TextEdit::singleline(&mut p.compact_at_tokens)
                        .hint_text(format!(
                            "{} — default when empty, 0 = off",
                            filar_core::DEFAULT_COMPACT_AT_TOKENS
                        )),
                )
                .on_hover_text(
                    "Prompt tokens at which the session history is compacted. \
                     Set below the model's context window. Leave empty for the \
                     default; enter 0 to disable compaction for this profile.",
                );
            });
            ui.label("Extra body (JSON):");
            ui.add(egui::TextEdit::multiline(&mut p.extra_body)
                .hint_text("e.g. {\"thinking\":{\"type\":\"disabled\"}}")
                .desired_rows(2).desired_width(f32::INFINITY));
        }
        self.show_api_key = show_api_key;

        ui.separator();
        ui.label("Arbiter profile (optional — independent command audit before confirm):");
        let mut arbiter_choice = self.arbiter_profile.clone().unwrap_or_default();
        let same_label = "(same as session profile)";
        egui::ComboBox::from_label("Arbiter")
            .selected_text(if arbiter_choice.is_empty() {
                same_label
            } else {
                &arbiter_choice
            })
            .show_ui(ui, |ui| {
                if ui.selectable_label(arbiter_choice.is_empty(), same_label).clicked() {
                    arbiter_choice.clear();
                }
                for name in &profile_names {
                    if ui.selectable_label(arbiter_choice == *name, name).clicked() {
                        arbiter_choice = name.clone();
                    }
                }
            });
        self.arbiter_profile = if arbiter_choice.is_empty() {
            None
        } else {
            Some(arbiter_choice)
        };
        ui.label(
            egui::RichText::new(
                "Tip: a different vendor catches mismatches between command and explanation.",
            )
            .small()
            .weak(),
        );

        ui.separator();
        self.render_risk_scorer(ui);
    }

    /// Command safety scorer settings (#552): a decision model that rates
    /// every command the agent proposes, 0–100% (100% = safe). Separate from
    /// the LLM profiles and the arbiter, with its own address and key.
    fn render_risk_scorer(&mut self, ui: &mut egui::Ui) {
        let d = &mut self.risk_scorer;
        ui.checkbox(&mut d.enabled, "Command safety score")
            .on_hover_text(
                "A separate decision model rates every command the agent proposes: \
                 0–100%, 100% = safe. Shown in the confirmation dialog and next to \
                 the command. Advisory only — it never approves or blocks anything.",
            );
        if !d.enabled {
            return;
        }
        ui.horizontal(|ui| {
            ui.label("Type:");
            if ui.radio(!d.chat_logprobs, "Decisions API").clicked() {
                d.set_chat_logprobs(false);
            }
            if ui
                .radio(d.chat_logprobs, "Chat + logprobs")
                .on_hover_text(
                    "Any OpenAI-compatible chat model whose server returns logprobs \
                     (e.g. a local llama.cpp or vLLM server).",
                )
                .clicked()
            {
                d.set_chat_logprobs(true);
            }
        });
        if !d.chat_logprobs && d.base_url.trim().is_empty() {
            d.base_url = RISK_DECISIONS_BASE_URL.to_string();
        }
        ui.horizontal(|ui| {
            ui.label("Address:");
            ui.add(
                egui::TextEdit::singleline(&mut d.base_url)
                    .hint_text(if d.chat_logprobs {
                        "http://127.0.0.1:8080/v1"
                    } else {
                        RISK_DECISIONS_BASE_URL
                    })
                    .desired_width(f32::INFINITY),
            );
        });
        ui.horizontal(|ui| {
            ui.label("Model:");
            ui.add(
                egui::TextEdit::singleline(&mut d.model)
                    .hint_text(if d.chat_logprobs {
                        "local model name"
                    } else {
                        RISK_DECISIONS_MODEL_HINT
                    })
                    .desired_width(f32::INFINITY),
            );
        });
        let mut keyless = d.keyless;
        if ui.checkbox(&mut keyless, "No key (local server)").changed() {
            d.set_keyless(keyless);
        }
        if !d.keyless {
            ui.horizontal(|ui| {
                ui.label("Key name:");
                let resp = ui
                    .add(egui::TextEdit::singleline(&mut d.key_env).desired_width(180.0))
                    .on_hover_text("Environment variable / OS store entry the key is kept under.");
                // The "saved" status is for the name it was checked under:
                // re-check once the name is edited, not on every keystroke.
                if resp.lost_focus() && d.key_env.trim() != d.key_checked_for {
                    d.refresh_key_saved();
                }
            });
            let saved = d.key_saved_now();
            ui.horizontal(|ui| {
                ui.label("Key:");
                let id = ui.make_persistent_id("risk_scorer_key");
                apply_focused_secret_paste(ui, id, &mut d.key);
                ui.add(
                    egui::TextEdit::singleline(&mut d.key)
                        .id(id)
                        .password(true)
                        .hint_text(if saved { "saved — type to replace" } else { "not set" })
                        .desired_width(180.0),
                );
                ui.label(
                    egui::RichText::new(if saved { "saved" } else { "not set" })
                        .small()
                        .weak(),
                );
            });
        }
        egui::CollapsingHeader::new("Advanced")
            .id_salt("risk_scorer_advanced")
            .default_open(d.advanced_open)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Warn below %:");
                    ui.add(egui::TextEdit::singleline(&mut d.warn_below).desired_width(50.0));
                    ui.label("Danger below %:");
                    ui.add(egui::TextEdit::singleline(&mut d.danger_below).desired_width(50.0));
                });
                ui.label(
                    egui::RichText::new(
                        "Colours of the rating. Tune them to the model: probabilities differ between models.",
                    )
                    .small()
                    .weak(),
                );
            });
    }

    /// The Logs tab (#584): `[[log_sources]]` the agent may search with
    /// `query_logs`, each with a Check.
    fn render_log_sources(&mut self, ui: &mut egui::Ui) {
        ui.label(egui::RichText::new("Log sources").strong());
        ui.label(
            egui::RichText::new(
                "Centralised log stores the agent searches with query_logs (read-only). \
                 Keys go to the OS credential store at Launch, never to config.toml.",
            )
            .small()
            .weak(),
        );
        let mut remove = None;
        let mut check = None;
        let cluster_names: Vec<String> = self.kube_slots.iter().map(|k| k.name.clone()).collect();
        for (i, d) in self.log_sources.iter_mut().enumerate() {
            let title = format!(
                "{} ({})",
                if d.name.trim().is_empty() { "new source" } else { d.name.trim() },
                d.kind.as_str()
            );
            egui::CollapsingHeader::new(title)
                .id_salt(("log_source", d.id))
                .default_open(d.name.trim().is_empty())
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label("Name:");
                        let resp = ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(160.0));
                        if resp.lost_focus() && d.key_env.trim().is_empty() && d.auth.needs_key() {
                            d.key_env = suggested_log_key_env(&d.name);
                        }
                        ui.label("Kind:");
                        egui::ComboBox::from_id_salt(("log_kind", d.id))
                            .selected_text(d.kind.as_str())
                            .show_ui(ui, |ui| {
                                for kind in filar_core::LogSourceKind::ALL {
                                    ui.selectable_value(&mut d.kind, kind, kind.as_str());
                                }
                            });
                    });
                    if !d.kind.auths().contains(&d.auth) {
                        d.auth = filar_core::LogAuth::None;
                    }
                    ui.horizontal(|ui| {
                        ui.label("URL:");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.url)
                                .hint_text("https://vlogs.example.com")
                                .desired_width(f32::INFINITY),
                        );
                    });
                    ui.horizontal(|ui| {
                        ui.label("Auth:");
                        egui::ComboBox::from_id_salt(("log_auth", d.id))
                            .selected_text(d.auth.as_str())
                            .show_ui(ui, |ui| {
                                for auth in d.kind.auths() {
                                    ui.selectable_value(&mut d.auth, *auth, auth.as_str());
                                }
                            });
                        if d.auth == filar_core::LogAuth::Basic {
                            ui.label("User:");
                            ui.add(egui::TextEdit::singleline(&mut d.user).desired_width(120.0));
                        }
                    });
                    if d.auth.needs_key() {
                        if d.key_env.trim().is_empty() {
                            d.key_env = suggested_log_key_env(&d.name);
                        }
                        ui.horizontal(|ui| {
                            ui.label("Key name:");
                            let resp = ui
                                .add(egui::TextEdit::singleline(&mut d.key_env).desired_width(200.0))
                                .on_hover_text("Environment variable / OS store entry the key is kept under.");
                            if resp.lost_focus() && d.key_env.trim() != d.key_checked_for {
                                d.refresh_key_saved();
                            }
                        });
                        let saved = d.key_saved_now();
                        ui.horizontal(|ui| {
                            ui.label("Key:");
                            let id = ui.make_persistent_id(("log_source_key", d.id));
                            apply_focused_secret_paste(ui, id, &mut d.key);
                            ui.add(
                                egui::TextEdit::singleline(&mut d.key)
                                    .id(id)
                                    .password(true)
                                    .hint_text(if saved { "saved — type to replace" } else { "not set" })
                                    .desired_width(200.0),
                            );
                            ui.label(egui::RichText::new(if saved { "saved" } else { "not set" }).small().weak());
                        });
                    }
                    ui.horizontal(|ui| match d.kind {
                        filar_core::LogSourceKind::VictoriaLogs => {
                            ui.label("Tenant:");
                            ui.add(egui::TextEdit::singleline(&mut d.tenant).hint_text("0:0").desired_width(80.0));
                        }
                        filar_core::LogSourceKind::Loki => {
                            ui.label("Tenant (X-Scope-OrgID):");
                            ui.add(egui::TextEdit::singleline(&mut d.tenant).desired_width(120.0));
                        }
                        filar_core::LogSourceKind::OpenSearch | filar_core::LogSourceKind::Elasticsearch => {
                            ui.label("Index:");
                            ui.add(egui::TextEdit::singleline(&mut d.index).hint_text("logs-*").desired_width(160.0));
                        }
                    });
                    ui.horizontal(|ui| {
                        ui.label("Kubernetes fields:");
                        let shown = if d.k8s_fields.is_empty() {
                            format!(
                                "default ({})",
                                filar_core::K8sFieldPreset::default_for(d.kind).as_str()
                            )
                        } else {
                            d.k8s_fields.clone()
                        };
                        egui::ComboBox::from_id_salt(("log_k8s", d.id))
                            .selected_text(shown)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut d.k8s_fields, String::new(), "default");
                                for preset in filar_core::K8sFieldPreset::ALL {
                                    ui.selectable_value(&mut d.k8s_fields, preset.as_str().to_string(), preset.as_str());
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.label("Max window (h):");
                        ui.add(egui::TextEdit::singleline(&mut d.max_window_hours).desired_width(40.0));
                        ui.label("Max lines:");
                        ui.add(egui::TextEdit::singleline(&mut d.max_lines).desired_width(50.0));
                        ui.label("Timeout (s):");
                        ui.add(egui::TextEdit::singleline(&mut d.timeout_secs).desired_width(40.0));
                    });
                    ui.horizontal(|ui| {
                        ui.label("Clusters:");
                        ui.add(
                            egui::TextEdit::singleline(&mut d.clusters)
                                .hint_text(cluster_names.join(", "))
                                .desired_width(200.0),
                        )
                        .on_hover_text("[[kube_clusters]] whose logs this source holds — enables pod_logs_history there.");
                        ui.label("Tags:");
                        ui.add(egui::TextEdit::singleline(&mut d.tags).desired_width(120.0));
                    });
                    ui.horizontal(|ui| {
                        if ui
                            .add_enabled(!d.check_in_flight, egui::Button::new("Check"))
                            .on_hover_text("One query over the last 5 minutes with limit 1.")
                            .clicked()
                        {
                            check = Some(i);
                        }
                        if ui.button("Remove").clicked() {
                            remove = Some(i);
                        }
                        if d.check_in_flight {
                            ui.label(egui::RichText::new("checking…").small().weak());
                        } else if !d.check_status.is_empty() {
                            ui.label(egui::RichText::new(&d.check_status).small());
                        }
                    });
                });
        }
        if let Some(i) = check {
            self.start_log_check(i);
        }
        if let Some(i) = remove {
            self.log_sources.remove(i);
        }
        if ui.button("Add source").clicked() {
            let id = self.next_log_source_id;
            self.next_log_source_id += 1;
            self.log_sources.push(LogSourceDraft::blank(id));
        }
    }

    fn start_log_check(&mut self, idx: usize) {
        let Some(d) = self.log_sources.get_mut(idx) else {
            return;
        };
        let cfg = match d.to_config() {
            Ok(cfg) => cfg,
            Err(msg) => {
                d.check_status = msg;
                return;
            }
        };
        let key = d.key_for_check();
        if cfg.auth_kind().is_some_and(filar_core::LogAuth::needs_key) && key.is_empty() {
            d.check_status = format!("Check failed: no key under {} (type it above)", cfg.key_env);
            return;
        }
        d.check_in_flight = true;
        d.check_status.clear();
        let id = d.id;
        let tx = self.log_check_tx.clone();
        std::thread::spawn(move || {
            let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
                Ok(runtime) => runtime.block_on(check_log_source(&cfg, &key)),
                Err(e) => Err(format!("cannot start the check runtime: {e}")),
            };
            let _ = tx.send(LogCheckResult { id, config: cfg, result });
        });
    }

    fn poll_log_checks(&mut self, ctx: &egui::Context) {
        while let Ok(done) = self.log_check_rx.try_recv() {
            if let Some(d) = self.log_sources.iter_mut().find(|d| d.id == done.id) {
                d.check_in_flight = false;
                d.check_status = if d.to_config().ok().as_ref() == Some(&done.config) {
                    match done.result {
                        Ok(message) => message,
                        Err(message) => format!("Check failed: {message}"),
                    }
                } else {
                    "Check result discarded: the source fields changed.".to_string()
                };
            }
            ctx.request_repaint();
        }
    }

    /// Every log source must be complete, uniquely named, and linked only to
    /// clusters the launcher knows (#584).
    fn validate_log_sources(&self) -> Result<Vec<filar_core::LogSourceConfig>, String> {
        // By name: a cluster row with other problems is reported by the
        // cluster validation itself, not as "not in the list" here.
        let known: std::collections::HashSet<String> = self
            .kube_slots
            .iter()
            .map(|k| k.name.trim().to_string())
            .filter(|name| !name.is_empty())
            .collect();
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for d in &self.log_sources {
            let cfg = d.to_config()?;
            if !seen.insert(cfg.name.clone()) {
                return Err(format!("Log source {}: the name is used twice", cfg.name));
            }
            if let Some(cluster) = cfg.clusters.iter().find(|c| !known.contains(*c)) {
                return Err(format!("Log source {}: cluster {cluster} is not in the cluster list", cfg.name));
            }
            out.push(cfg);
        }
        Ok(out)
    }

    /// Folder picker for the Ctrl+S session export directory.
    fn render_save_dir_field(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Save directory:");
            let mut display = match &self.save_dir {
                Some(p) => p.display().to_string(),
                None => "CWD (where filar was launched)".to_string(),
            };
            ui.add(
                egui::TextEdit::singleline(&mut display)
                    .desired_width(240.0)
                    .interactive(false),
            );
            if ui.button("Browse…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.save_dir = Some(dir);
                }
            }
            if self.save_dir.is_some() && ui.button("Reset").clicked() {
                self.save_dir = None;
            }
        });
    }

    /// Import hosts from a file the user picks (#416): parse it, then either
    /// apply it directly or open the collision dialog. A failed parse shows
    /// an error and changes nothing.
    fn start_host_import(&mut self) {
        // Belt-and-braces: the Import button is disabled while a staged
        // decision is pending — a second file must not replace it (#416 review).
        if self.import_dialog.is_some() {
            return;
        }
        // The last file operation's verdict does not survive a new attempt.
        self.file_status.clear();
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Host list", &["toml", "csv"])
            .pick_file()
        else {
            return;
        };
        match stage_import(&mut self.ssh_slots, &path) {
            Err(msg) => self.set_other_error(format!("Import failed: {msg}")),
            Ok(ImportStaged::Applied(outcome)) => {
                self.clear_error();
                self.file_status = import_status_line(&outcome);
            }
            Ok(ImportStaged::NeedsDecision {
                rows,
                collisions,
                kube_skipped,
            }) => {
                self.import_dialog = Some(ImportDialog {
                    rows,
                    collisions,
                    kube_skipped,
                });
            }
        }
    }

    /// Export the host list to a TOML file the user picks (#417) — the very
    /// format `Import…` reads back, safe to commit and share. Secrets are
    /// never written (invariant #3).
    fn start_host_export(&mut self) {
        // The last file operation's verdict does not survive a new attempt.
        self.file_status.clear();
        // Serialize in its own scope: the borrow of `ssh_slots` must end
        // before the status fields are written.
        let prepared = {
            let rows: Vec<&SshSlot> = self
                .ssh_slots
                .iter()
                .filter(|s| !s.host.trim().is_empty())
                .collect();
            if rows.is_empty() {
                let kube_skipped = self.kube_slots.iter().filter(|slot| !slot.is_blank()).count();
                if kube_skipped > 0 {
                    self.clear_error();
                    self.file_status = format!(
                        "Export: no SSH hosts; {kube_skipped} Kubernetes cluster(s) skipped"
                    );
                }
                return;
            }
            export_hosts_toml(&rows).map(|text| (text, rows.len()))
        };
        let (text, count) = match prepared {
            Ok(prepared) => prepared,
            Err(msg) => {
                self.set_other_error(format!("Export failed: {msg}"));
                return;
            }
        };
        let kube_skipped = self
            .kube_slots
            .iter()
            .filter(|slot| !slot.is_blank())
            .count();
        let Some(path) = rfd::FileDialog::new()
            .add_filter("Host list", &["toml"])
            .set_file_name("filar-hosts.toml")
            .save_file()
        else {
            return;
        };
        match std::fs::write(&path, text) {
            Ok(()) => {
                self.clear_error();
                self.file_status = export_status_line(count, kube_skipped, &path);
            }
            Err(e) => self.set_other_error(format!("Export failed: {e}")),
        }
    }

    /// Groups tab (#418): name, tag rule, policy, limits and a live preview
    /// of the current composition. The preview is the point — the blast
    /// radius must be visible while the rule is written, not at the first
    /// run.
    fn render_groups(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(format!("Groups ({}):", self.host_groups.len()));
            if ui
                .button("+ Add group")
                .on_hover_text("Define a group by tag intersection")
                .clicked()
            {
                self.add_group();
            }
        });
        ui.label(
            egui::RichText::new(
                "A host joins the group when it carries all of the tags below. \
                 The group carries the policy and limits for the fleet dialogue.",
            )
            .small()
            .weak(),
        );
        if self.host_groups.is_empty() {
            ui.label(egui::RichText::new("  (no groups yet)").weak());
            return;
        }
        // One pointer delivers at most one click per frame: a single pending
        // action applied after the loop, like the host list (#411).
        let mut action: Option<GroupAction> = None;
        // The preview reads the same merged list a launch would use, built
        // once before the loop — current tag edits included (#418).
        let targets = self.launch_ssh_targets();
        let profile_names: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
        let count = self.host_groups.len();
        for i in 0..count {
            let draft = &mut self.host_groups[i];
            ui.group(|ui| {
                ui.horizontal(|ui| {
                    ui.label("Name:");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.name)
                            .hint_text("Work prod")
                            .desired_width(150.0),
                    );
                    if ui
                        .add_enabled(i > 0, egui::Button::new("⬆").small())
                        .on_hover_text("Move up")
                        .clicked()
                    {
                        action = Some(GroupAction::Up(i));
                    }
                    if ui
                        .add_enabled(i + 1 < count, egui::Button::new("⬇").small())
                        .on_hover_text("Move down")
                        .clicked()
                    {
                        action = Some(GroupAction::Down(i));
                    }
                    if ui.button("X").on_hover_text("Remove group").clicked() {
                        action = Some(GroupAction::Remove(i));
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Match tags (all must match):");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.match_tags)
                            .hint_text("work, prod (comma-separated)")
                            .desired_width(200.0),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label("Policy:");
                    egui::ComboBox::from_id_salt(format!("group_policy_{i}"))
                        .selected_text(match draft.policy {
                            filar_core::HostGroupPolicy::ReadOnly => "read-only",
                        })
                        .show_ui(ui, |ui| {
                            // The only choice today — the fleet dialogue is
                            // read-only by definition (#418). The combo keeps
                            // the binding visible and extendable.
                            ui.selectable_value(
                                &mut draft.policy,
                                filar_core::HostGroupPolicy::ReadOnly,
                                "read-only",
                            );
                        });
                    ui.label("Max parallel:");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.max_parallel).desired_width(40.0),
                    )
                    .on_hover_text("Upper bound on executors running at the same time");
                    ui.label("Per-host timeout (s):");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.per_host_timeout_secs)
                            .desired_width(48.0),
                    )
                    .on_hover_text("Deadline for one command on one host");
                });
                ui.horizontal(|ui| {
                    ui.label("LLM profile:");
                    egui::ComboBox::from_id_salt(format!("group_llm_{i}"))
                        .selected_text(if draft.llm_profile.trim().is_empty() {
                            "(session profile)"
                        } else {
                            draft.llm_profile.as_str()
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut draft.llm_profile,
                                String::new(),
                                "(session profile)",
                            );
                            for name in &profile_names {
                                ui.selectable_value(&mut draft.llm_profile, name.clone(), name);
                            }
                        });
                });
                // Live preview (#418): recomputed every frame, so a tag edit
                // in the Hosts tab shows up here immediately.
                ui.label(
                    egui::RichText::new(group_preview_line(&draft.match_tags, &targets))
                        .weak()
                        .small(),
                );
            });
            ui.add_space(4.0);
        }
        match action {
            Some(GroupAction::Up(i)) => self.move_group_up(i),
            Some(GroupAction::Down(i)) => self.move_group_down(i),
            Some(GroupAction::Remove(i)) => self.remove_group(i),
            None => {}
        }
    }

    /// Collision-choice window of a staged import (#416). Skip/overwrite/
    /// rename is the user's call — the launcher never decides silently.
    fn render_import_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = self.import_dialog.take() else {
            return;
        };
        enum Decision {
            Apply(CollisionPolicy),
            Cancel,
        }
        let mut open = true;
        let mut decision: Option<Decision> = None;
        egui::Window::new("Import host list")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .open(&mut open)
            .show(ctx, |ui| {
                ui.label(format!(
                    "{} of {} imported hosts use a name that already exists:",
                    dialog.collisions.len(),
                    dialog.rows.len()
                ));
                egui::ScrollArea::vertical()
                    .id_salt("import_collisions")
                    .max_height(120.0)
                    .show(ui, |ui| {
                        for name in &dialog.collisions {
                            ui.label(format!("  {name}"));
                        }
                    });
                ui.separator();
                ui.label("For every colliding host:");
                ui.horizontal(|ui| {
                    if ui
                        .button("Skip")
                        .on_hover_text("Keep the existing host, drop the imported one")
                        .clicked()
                    {
                        decision = Some(Decision::Apply(CollisionPolicy::Skip));
                    }
                    if ui
                        .button("Overwrite")
                        .on_hover_text(
                            "Replace connection fields, tags and auth type of the existing entry",
                        )
                        .clicked()
                    {
                        decision = Some(Decision::Apply(CollisionPolicy::Overwrite));
                    }
                    if ui
                        .button("Rename")
                        .on_hover_text("Import under a free name like name-2")
                        .clicked()
                    {
                        decision = Some(Decision::Apply(CollisionPolicy::Rename));
                    }
                });
                ui.separator();
                if ui.button("Cancel import").clicked() {
                    decision = Some(Decision::Cancel);
                }
            });
        match decision {
            Some(Decision::Apply(policy)) => {
                let mut outcome = apply_import(&mut self.ssh_slots, dialog.rows, policy);
                outcome.kube_skipped = dialog.kube_skipped;
                self.clear_error();
                self.file_status = import_status_line(&outcome);
            }
            Some(Decision::Cancel) => {
                self.file_status = "Import cancelled.".to_string();
            }
            None if open => {
                // No choice yet — keep the window for the next frame.
                self.import_dialog = Some(dialog);
            }
            None => {
                // Closed with the X — same as Cancel.
                self.file_status = "Import cancelled.".to_string();
            }
        }
    }

    /// Show a profile-validation message and remember that it is ours.
    ///
    /// Every other write to `validation_error` must go through
    /// [`LauncherApp::set_other_error`] or [`LauncherApp::clear_error`], which
    /// drop the flag. Setting the text directly would leave the flag standing
    /// over someone else's message, and the next successful save would erase it.
    fn set_profile_error(&mut self, msg: String) {
        self.validation_error = msg;
        self.profile_error_shown = true;
    }

    /// Show a message that did not come from profile validation.
    fn set_other_error(&mut self, msg: String) {
        self.validation_error = msg;
        self.profile_error_shown = false;
    }

    /// Clear the slot whoever wrote it.
    fn clear_error(&mut self) {
        self.validation_error.clear();
        self.profile_error_shown = false;
    }

    /// Drop a previously shown profile-validation message, leaving any other
    /// warning (an unmatched SSH target, for one) on screen.
    fn clear_profile_error(&mut self) {
        if self.profile_error_shown {
            self.clear_error();
        }
    }

    /// Persist settings, refusing to write when any profile has a malformed
    /// field.
    ///
    /// The whole list goes through [`LlmProfileData::to_profile`] below, which
    /// converts with fallbacks, so *keeping* an invalid value is not an option:
    /// `settings.json` holds typed fields and `max_tokens = "abc"` has no
    /// representation there. The choice is between rewriting it as a default
    /// silently and refusing with a message; this refuses (#385).
    fn save_profiles(&mut self) {
        if let Err(msg) = validate_all_profile_fields(&self.profiles) {
            self.set_profile_error(msg);
            return;
        }
        self.clear_profile_error();
        let p = self.profiles.get(self.selected_profile);
        // Profile add/delete is independent of Kubernetes editing. Start
        // from the saved settings and leave both kube fields untouched: an
        // incomplete draft stays in memory, and Launch remains the operation
        // that validates and persists the current cluster list.
        let mut settings = Settings::load();
        settings.model = p.map_or_else(String::new, |x| x.model.clone());
        settings.api_base_url = p.map_or_else(String::new, |x| x.api_base_url.clone());
        settings.ssh_profiles = self.persistable_ssh_profiles();
        settings.ssh_list_version = SSH_LIST_VERSION;
        settings.last_ssh = self.persisted_last_ssh();
        settings.temperature = p.map_or_else(String::new, |x| x.temperature.clone());
        settings.extra_body = p.map_or_else(String::new, |x| x.extra_body.clone());
        settings.profiles = self.profiles.iter().map(LlmProfileData::to_profile).collect();
        settings.selected_profile = self.selected_profile;
        settings.save_dir = self.save_dir.clone();
        settings.arbiter_profile = self.arbiter_profile.clone();
        settings.save();
    }

    /// Check everything that must hold before a launch, reporting the first
    /// problem through `validation_error`.
    ///
    /// Split out of [`LauncherApp::do_launch`] so it can be tested: the rest of
    /// `do_launch` ends in `std::process::exit`, which would take the test
    /// harness with it — and, worse, exit `0`, so a lost check would look like
    /// a passing run.
    fn validate_launch(&mut self) -> bool {
        self.clear_error();
        // Field checks cover every profile, not just the selected one: a launch
        // writes the whole list to settings.json and pending_launch.json, so an
        // unnoticed typo elsewhere would be normalised away (#385).
        if let Err(msg) = validate_all_profile_fields(&self.profiles) {
            self.set_profile_error(msg);
            return false;
        }
        let Some(p) = self.profiles.get(self.selected_profile) else {
            self.set_other_error("No profile selected".to_string());
            return false;
        };
        // Validate unique names and non-empty name.
        if p.name.trim().is_empty() {
            self.set_other_error("Profile name must not be empty.".to_string());
            return false;
        }
        if p.key_env.trim().is_empty() && p.api_base_url.trim().is_empty() {
            self.set_other_error(
                "Keyless (local) profile needs an API URL (e.g. http://localhost:11434/v1)."
                    .to_string(),
            );
            return false;
        }
        let duplicate = self
            .profiles
            .iter()
            .enumerate()
            .any(|(i, other)| i != self.selected_profile && other.name == p.name);
        if duplicate {
            let name = p.name.clone();
            self.set_other_error(format!(
                "Duplicate profile name: \"{name}\". Names must be unique."
            ));
            return false;
        }
        if let Err(msg) = self.validate_ssh_slots() {
            self.set_other_error(msg);
            return false;
        }
        if let Err(msg) = self.validate_selected_key() {
            self.set_other_error(msg);
            return false;
        }
        if let Err(msg) = self.validate_kube_clusters() {
            self.set_other_error(msg);
            return false;
        }
        if let Err(msg) = self.validate_host_groups() {
            self.set_other_error(msg);
            return false;
        }
        if let Err(msg) = self.risk_scorer.to_config() {
            self.set_other_error(msg);
            return false;
        }
        if let Err(msg) = self.validate_log_sources() {
            self.set_other_error(msg);
            return false;
        }
        true
    }

    /// Every configured host must be launchable as written (#411).
    ///
    /// A blank row is an in-UI scratch entry and may stay — only the selected
    /// target must be complete. A row with an address but no alias is not
    /// fine: the alias names the keyring entry and the per-target export
    /// folder, and a name shared by two hosts silently hides one of them
    /// from the Ctrl+O list.
    fn validate_ssh_slots(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for (i, slot) in self.ssh_slots.iter().enumerate() {
            let host = slot.host.trim();
            if host.is_empty() {
                if self.selected_kube.is_none() && self.target_mode == i + 1 {
                    return Err("Host is required for the selected SSH target.".to_string());
                }
                continue;
            }
            // Checked in the same truncated form `to_profile` persists: the
            // stored alias — not the typed one — names the keyring entry.
            let alias: String = slot.alias.trim().chars().take(32).collect();
            if alias.is_empty() {
                return Err(format!(
                    "SSH host '{host}' needs an alias — it names the keyring entry and the export folder."
                ));
            }
            if !seen.insert(alias.clone()) {
                return Err(format!(
                    "Duplicate SSH host alias \"{alias}\". Aliases must be unique."
                ));
            }
        }
        Ok(())
    }

    /// The selected host's key must be usable before the launcher closes
    /// (#489): the file readable as a private key and, for an encrypted key
    /// whose passphrase is to be saved, that passphrase checked locally —
    /// a wrong one is reported here, not as a failed login later. Only the
    /// selected host is checked: decrypting runs a deliberately slow KDF.
    fn validate_selected_key(&self) -> Result<(), String> {
        if self.selected_kube.is_some() {
            return Ok(());
        }
        let Some(slot) = self.target_mode.checked_sub(1).and_then(|i| self.ssh_slots.get(i)) else {
            return Ok(());
        };
        if slot.auth != SlotAuth::Key {
            return Ok(());
        }
        let path = slot.resolved_key_path();
        let protection = filar_transport::key_protection(&path).map_err(|e| e.to_string())?;
        if protection == filar_transport::KeyProtection::Encrypted && slot.save_passphrase {
            let passphrase = sanitize_secret_clipboard(&slot.passphrase);
            if passphrase.is_empty() {
                return Err(format!(
                    "Enter the passphrase of SSH key {} — or uncheck \"Save passphrase\" \
                     to be asked for it in the terminal.",
                    path.display()
                ));
            }
            if !filar_transport::key_passphrase_matches(&path, &passphrase) {
                return Err(format!("Wrong passphrase for SSH key {}.", path.display()));
            }
        }
        Ok(())
    }

    /// Every non-blank group must be definable as written (#418): a name, a
    /// well-formed limit, and a name nobody else uses — later features
    /// address groups by name.
    fn validate_host_groups(&self) -> Result<(), String> {
        let mut seen = std::collections::HashSet::new();
        for (i, draft) in self.host_groups.iter().enumerate() {
            if draft.is_blank() {
                continue;
            }
            let group = draft
                .to_group()
                .map_err(|e| format!("Group #{}: {e}", i + 1))?;
            if !seen.insert(group.name.clone()) {
                return Err(format!(
                    "Duplicate group name \"{}\". Names must be unique.",
                    group.name
                ));
            }
        }
        Ok(())
    }

    /// Group drafts worth the config file: blank scratch rows never reach
    /// `config.toml`, like blank host rows never reach `settings.json`
    /// (#418).
    fn persistable_host_groups(&self) -> Result<Vec<filar_core::HostGroup>, String> {
        self.host_groups
            .iter()
            .filter(|d| !d.is_blank())
            .map(HostGroupDraft::to_group)
            .collect()
    }

    /// SSH profiles worth persisting: blank scratch rows never reach the
    /// disk (#411) — the saved list holds real hosts only.
    fn persistable_ssh_profiles(&self) -> Vec<SshProfile> {
        self.ssh_slots
            .iter()
            .filter(|s| !s.host.trim().is_empty())
            .map(SshSlot::to_profile)
            .collect()
    }

    fn validate_kube_clusters(&self) -> Result<(), String> {
        self.persistable_kube_clusters().map(|_| ())
    }

    fn persistable_kube_clusters(&self) -> Result<Vec<filar_core::KubeCluster>, String> {
        let mut seen: std::collections::HashSet<String> = self
            .launch_ssh_targets()
            .into_iter()
            .map(|target| target.name)
            .collect();
        let mut clusters = Vec::new();
        for slot in self.kube_slots.iter().filter(|slot| !slot.is_blank()) {
            let cluster = slot.to_cluster()?;
            if !seen.insert(cluster.name.clone()) {
                return Err(format!(
                    "Duplicate target name \"{}\". SSH hosts and Kubernetes clusters share one namespace.",
                    cluster.name
                ));
            }
            clusters.push(cluster);
        }
        if let Some(selected) = self.selected_kube {
            if self
                .kube_slots
                .get(selected)
                .is_none_or(KubeSlot::is_blank)
            {
                return Err("Name and context are required for the selected Kubernetes cluster.".into());
            }
        }
        Ok(clusters)
    }

    fn selected_kube_name(&self) -> Option<String> {
        self.selected_kube
            .and_then(|idx| self.kube_slots.get(idx))
            .filter(|slot| !slot.is_blank())
            .map(|slot| slot.name.trim().to_string())
    }

    /// The merged SSH target list a launch hands to the TUI (#412): the
    /// launcher's profiles rebuilt from the current fields, plus the manual
    /// `[[ssh_targets]]` entries.
    ///
    /// The Groups tab previews over the same list (#418): the composition it
    /// shows is exactly what would launch, manual hosts included.
    fn launch_ssh_targets(&self) -> Vec<filar_core::SshTarget> {
        merge_manual_ssh_targets(
            build_ssh_targets_from_profiles(&self.persistable_ssh_profiles()),
            &self.manual_targets,
        )
    }

    /// The value to store as `Settings::last_ssh`: the selection encoded as
    /// 1 + its position in the persisted list (0 = Local, nothing to
    /// restore) (#411).
    ///
    /// A raw slot index is not stable across a save/load round trip: blank
    /// scratch rows are dropped from `ssh_profiles`, so a blank row *before*
    /// the selected host shifts every later position and a restart would
    /// preselect a different host (`run_launcher` maps `last_ssh` back
    /// against the saved list). The 1-based encoding keeps Local (`0`) apart
    /// from the first persisted host (`1`): every host, `SSH1` included,
    /// survives a restart. A selected blank row has no persisted counterpart
    /// and restores as Local, the same way `target_mode == 0` does.
    fn persisted_last_ssh(&self) -> usize {
        if self.selected_kube.is_some() || self.target_mode == 0 {
            return 0;
        }
        let idx = self.target_mode - 1;
        // Defensive: the add/remove/move invariants keep `target_mode` at
        // most `len()`. The early return is what guarantees `idx < len`
        // for the slice below.
        let Some(slot) = self.ssh_slots.get(idx) else {
            return 0;
        };
        if slot.host.trim().is_empty() {
            return 0;
        }
        self.ssh_slots[..idx]
            .iter()
            .filter(|s| !s.host.trim().is_empty())
            .count()
            + 1
    }

    /// The keyring operations `do_launch` performs for the SSH rows, as
    /// `(key, Some(password))` to save and `(key, None)` to clear (#411).
    ///
    /// Only persisted rows are touched: a blank scratch row has an empty
    /// alias, and `ssh_cred_name(i, "")` falls back to the index-derived
    /// `ssh_target:SSH{i+1}` — a name a migrated host may legitimately own
    /// (positional aliases survive the migration even when earlier blank
    /// slots are dropped). Deleting through that key would silently drop
    /// the host's saved password.
    fn ssh_credential_ops(&self) -> Vec<(String, Option<String>)> {
        self.ssh_slots
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.host.trim().is_empty() && !s.alias.trim().is_empty())
            .map(|(i, s)| {
                let key = ssh_cred_name(i, &s.alias);
                if s.auth == SlotAuth::Password && s.save_password && !s.password.is_empty() {
                    (key, Some(s.password.clone()))
                } else {
                    (key, None)
                }
            })
            .collect()
    }

    /// The same for key passphrases (#489): saved under
    /// `ssh_key_passphrase:<alias>` for a key-login host with "Save
    /// passphrase" checked, cleared otherwise — a host switched to another
    /// login method must not keep its old key's secret. Persisted rows only,
    /// for the reason [`Self::ssh_credential_ops`] gives.
    fn key_passphrase_ops(&self) -> Vec<(String, Option<String>)> {
        self.ssh_slots
            .iter()
            .enumerate()
            .filter(|(_, s)| !s.host.trim().is_empty() && !s.alias.trim().is_empty())
            .map(|(i, s)| {
                let key = slot_passphrase_cred_name(i, &s.alias);
                if s.auth == SlotAuth::Key && s.save_passphrase && !s.passphrase.is_empty() {
                    (key, Some(s.passphrase.clone()))
                } else {
                    (key, None)
                }
            })
            .collect()
    }

    /// Append a blank host and select it, so its fields open below.
    fn add_host(&mut self) {
        self.ssh_slots.push(SshSlot::blank());
        self.target_mode = self.ssh_slots.len();
        self.selected_kube = None;
    }

    fn add_kube(&mut self) {
        let id = self.next_kube_slot_id;
        self.next_kube_slot_id = self.next_kube_slot_id.saturating_add(1);
        self.kube_slots.push(KubeSlot::blank(id));
        self.selected_kube = Some(self.kube_slots.len() - 1);
    }

    fn remove_kube(&mut self, idx: usize) {
        if idx >= self.kube_slots.len() {
            return;
        }
        self.kube_slots.remove(idx);
        match self.selected_kube {
            Some(selected) if selected == idx => self.selected_kube = None,
            Some(selected) if selected > idx => self.selected_kube = Some(selected - 1),
            _ => {}
        }
    }

    /// Remove a host; a saved password goes with it. The selection falls
    /// back to Local — never silently to another host.
    fn remove_host(&mut self, idx: usize) {
        if idx >= self.ssh_slots.len() {
            return;
        }
        let slot = self.ssh_slots.remove(idx);
        // Both entries go, whatever the save flags say now: a flag unchecked
        // since the last launch still leaves that launch's secret behind
        // (#489 review).
        if !slot.alias.trim().is_empty() {
            delete_secret(&ssh_cred_name(idx, &slot.alias));
            delete_secret(&slot_passphrase_cred_name(idx, &slot.alias));
        }
        if self.target_mode == idx + 1 {
            self.target_mode = 0;
        } else if self.target_mode > idx + 1 {
            self.target_mode -= 1;
        }
    }

    /// Move a host one row up, keeping the selection on the same host.
    fn move_host_up(&mut self, idx: usize) {
        if idx == 0 || idx >= self.ssh_slots.len() {
            return;
        }
        self.ssh_slots.swap(idx - 1, idx);
        if self.target_mode == idx + 1 {
            self.target_mode = idx;
        } else if self.target_mode == idx {
            self.target_mode = idx + 1;
        }
    }

    /// Move a host one row down, keeping the selection on the same host.
    fn move_host_down(&mut self, idx: usize) {
        if idx + 1 >= self.ssh_slots.len() {
            return;
        }
        self.ssh_slots.swap(idx, idx + 1);
        if self.target_mode == idx + 1 {
            self.target_mode = idx + 2;
        } else if self.target_mode == idx + 2 {
            self.target_mode = idx + 1;
        }
    }

    /// Add a blank group row and switch to the Groups tab so the new row is
    /// visible. Limits start at the built-in defaults, like a fresh host row
    /// starts at port 22.
    fn add_group(&mut self) {
        let defaults = filar_core::HostGroup::default();
        self.host_groups.push(HostGroupDraft {
            name: String::new(),
            match_tags: String::new(),
            policy: filar_core::HostGroupPolicy::default(),
            max_parallel: defaults.max_parallel.to_string(),
            per_host_timeout_secs: defaults.per_host_timeout_secs.to_string(),
            kube: None,
            llm_profile: String::new(),
        });
        self.active_tab = 1;
    }

    /// Remove a group row by index; out-of-range indices are ignored.
    fn remove_group(&mut self, idx: usize) {
        if idx < self.host_groups.len() {
            self.host_groups.remove(idx);
        }
    }

    /// Move a group row one position up; the first row stays put.
    fn move_group_up(&mut self, idx: usize) {
        if idx > 0 && idx < self.host_groups.len() {
            self.host_groups.swap(idx - 1, idx);
        }
    }

    /// Move a group row one position down; the last row stays put.
    fn move_group_down(&mut self, idx: usize) {
        if idx + 1 < self.host_groups.len() {
            self.host_groups.swap(idx, idx + 1);
        }
    }

    fn do_launch(&mut self) {
        if !self.validate_launch() {
            return;
        }
        let Some(p) = self.profiles.get(self.selected_profile) else {
            return;
        };
        let mut target = launch_target_name(self.target_mode, &self.ssh_slots);
        let ssh = if self.selected_kube.is_none() && self.target_mode > 0 {
            let slot = &self.ssh_slots[self.target_mode - 1];
            Some(SshConnection {
                host: slot.host.clone(),
                port: slot.port.parse().unwrap_or(22),
                user: slot.user.clone(),
                password: sanitize_secret_clipboard(&slot.password),
                auth: slot.auth,
                key_path: slot_key_path(&slot.key_path),
                slot: self.target_mode.saturating_sub(1),
                alias: slot.alias.clone(),
                tags: parse_tags(&slot.tags),
            })
        } else { None };
        let kube_clusters = match self.persistable_kube_clusters() {
            Ok(clusters) => clusters,
            Err(msg) => {
                self.set_other_error(msg);
                return;
            }
        };
        let kube = self.selected_kube_name().and_then(|name| {
            kube_clusters
                .iter()
                .find(|cluster| cluster.name == name)
                .map(|cluster| filar_core::KubeSessionSnapshot {
                    target: cluster.name.clone(),
                    namespace: cluster.default_namespace.clone(),
                    pod: None,
                })
        });
        if let Some(snapshot) = &kube {
            target = snapshot.target.clone();
        }

        let settings = Settings {
            model: p.model.clone(), api_base_url: p.api_base_url.clone(),
            ssh_profiles: self.persistable_ssh_profiles(),
            kube_profiles: kube_clusters.clone(),
            last_kube: self.selected_kube_name(),
            ssh_list_version: SSH_LIST_VERSION,
            last_ssh: self.persisted_last_ssh(),
            temperature: p.temperature.clone(), extra_body: p.extra_body.clone(),
            profiles: self.profiles.iter().map(LlmProfileData::to_profile).collect(),
            selected_profile: self.selected_profile,
            save_dir: self.save_dir.clone(),
            arbiter_profile: self.arbiter_profile.clone(),
        };
        let host_groups = match self.persistable_host_groups() {
            Ok(groups) => groups,
            // validate_launch already refused invalid rows; never write a
            // half-defined fleet.
            Err(msg) => {
                self.set_other_error(msg);
                return;
            }
        };
        let risk_scorer = match self.risk_scorer.to_launch() {
            Ok(choice) => choice,
            // validate_launch already refused it; never launch half-set.
            Err(msg) => {
                self.set_other_error(msg);
                return;
            }
        };
        let risk_section = match &risk_scorer {
            RiskScorerLaunch::On { config } => Some(config.clone()),
            RiskScorerLaunch::Off => None,
        };
        let log_sources = match self.validate_log_sources() {
            Ok(sources) => sources,
            // validate_launch already refused it; never launch half-set.
            Err(msg) => {
                self.set_other_error(msg);
                return;
            }
        };
        settings.save();
        save_config_toml(&settings, &host_groups, risk_section.as_ref(), &log_sources);
        if let Some((key_env, key)) = self.risk_scorer.key_to_save() {
            save_secret(&key_env, &key);
        }
        for d in &self.log_sources {
            if let Some((key_env, key)) = d.key_to_save() {
                save_secret(&key_env, &key);
            }
        }
        for prof in &self.profiles {
            if !prof.key_env.trim().is_empty() && !prof.api_key.is_empty() {
                save_secret(&prof.key_env, &prof.api_key);
            }
        }
        for (key, password) in self
            .ssh_credential_ops()
            .into_iter()
            .chain(self.key_passphrase_ops())
        {
            match password {
                Some(p) => save_secret(&key, &p),
                None => delete_secret(&key),
            }
        }
        let session_id = self.selected_session.map(|i| self.sessions[i].id.clone());
        let ssh_targets = self.launch_ssh_targets();
        let cfg = LaunchConfig {
            target,
            ssh,
            kube,
            kube_clusters: Some(kube_clusters),
            model: p.model.clone(), api_base_url: p.api_base_url.clone(),
            api_key: sanitize_secret_clipboard(&p.api_key), session_id,
            temperature: p.temperature.clone(), extra_body: p.extra_body.clone(),
            selected_profile: Some(p.name.clone()), key_env: p.key_env.clone(),
            save_dir: self.save_dir.clone(),
            profiles: self.profiles.iter().map(LlmProfileData::to_profile).collect(),
            ssh_targets,
            arbiter_profile: self.arbiter_profile.clone(),
            host_groups: Some(host_groups),
            risk_scorer: Some(risk_scorer),
            log_sources: Some(log_sources),
        };
        save_pending_launch(&cfg);
        std::process::exit(0);
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Dark theme matching the TUI accent palette.
        configure_theme(ctx);
        self.poll_kube_checks(ctx);
        self.poll_log_checks(ctx);

        // Fixed bottom panel: Launch/Cancel always visible, regardless of
        // window height. The rest of the content scrolls above it.
        egui::TopBottomPanel::bottom("bottom_buttons")
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    // A pending collision decision must be answered first:
                    // launching now would drop the staged import silently
                    // (#416 review).
                    if ui
                        .add_enabled(
                            self.import_dialog.is_none(),
                            egui::Button::new("Launch"),
                        )
                        .clicked()
                    {
                        self.do_launch();
                    }
                if ui.button("Cancel").clicked() {
                    std::process::exit(0);
                }
            });
            if !self.validation_error.is_empty() {
                ui.colored_label(egui::Color32::RED, &self.validation_error);
            }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Filar");
                ui.label("Terminal with an AI agent over SSH");
                ui.separator();

                // Two tabs: the launch setup and the group definitions
                // (#418). The bottom Launch/Cancel panel stays outside, so
                // it is visible on both.
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut self.active_tab, 0, "Hosts");
                    ui.selectable_value(&mut self.active_tab, 1, "Groups");
                    ui.selectable_value(&mut self.active_tab, 2, "Logs");
                });
                ui.separator();

                if self.active_tab == 1 {
                    self.render_groups(ui);
                } else if self.active_tab == 2 {
                    self.render_log_sources(ui);
                } else {
                    self.render_session_list(ui);
                    ui.separator();
                    self.render_target_selector(ui);
                    ui.separator();
                    self.render_ssh_fields(ui);
                    self.render_kube_fields(ui);
                    ui.separator();
                    self.render_llm_settings(ui);
                    ui.separator();
                    self.render_save_dir_field(ui);
                }
            });
        });

        // The collision-choice window floats over the panels; drawing it
        // last keeps it on top (#416).
        self.render_import_dialog(ctx);
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run_launcher(config: &Config) {
    let sessions = SessionStore::with_default_dir()
        .ok()
        .and_then(|s| s.list().ok())
        .unwrap_or_default();

    let settings = Settings::load();

    tracing::info!(sessions = sessions.len(), "GUI launcher starting");

    let ssh_slots: Vec<SshSlot> = settings
        .ssh_profiles
        .iter()
        .enumerate()
        .map(|(i, p)| SshSlot::from_profile(p, i))
        .collect();
    // Settings are the launcher's latest edited list; config-only entries are
    // merged in so hand-written clusters remain editable. A launcher save
    // replaces both lists, so removed entries do not reappear.
    let mut kube_clusters = settings.kube_profiles.clone();
    for cluster in &config.kube_clusters {
        if !kube_clusters.iter().any(|saved| saved.name == cluster.name) {
            kube_clusters.push(cluster.clone());
        }
    }
    let kube_slots: Vec<KubeSlot> = kube_clusters
        .iter()
        .enumerate()
        .map(|(i, cluster)| KubeSlot::from_cluster(i as u64, cluster))
        .collect();
    let next_kube_slot_id = kube_slots.len() as u64;
    let selected_kube = settings.last_kube.as_ref().and_then(|name| {
        kube_slots.iter().position(|slot| slot.name == *name)
    });
    let (kube_check_tx, kube_check_rx) = std::sync::mpsc::channel();
    let (log_check_tx, log_check_rx) = std::sync::mpsc::channel();
    let log_sources: Vec<LogSourceDraft> = config
        .log_sources
        .iter()
        .enumerate()
        .map(|(i, cfg)| {
            let mut d = LogSourceDraft::from_config(cfg, i as u64);
            // One OS-store read per keyed source at start-up, not per frame.
            if d.auth.needs_key() {
                d.refresh_key_saved();
            }
            d
        })
        .collect();
    let next_log_source_id = log_sources.len() as u64;

    // Build profile list. Migrate old flat settings into a default profile
    // on first upgrade, preserving existing api_key.
    let mut profiles: Vec<LlmProfileData> = settings
        .profiles
        .iter()
        .map(|p| LlmProfileData::from_profile(p, load_secret_for_profile(p)))
        .collect();

    // If no profiles exist, create a default from flat fields / config.
    if profiles.is_empty() {
        let api_key = load_secret(api_key_cred_name());
        profiles.push(LlmProfileData {
            name: "default".into(),
            model: if settings.model.is_empty() { config.llm.model.clone() } else { settings.model.clone() },
            api_base_url: if settings.api_base_url.is_empty() { config.llm.api_base_url.clone() } else { settings.api_base_url.clone() },
            key_env: api_key_cred_name().to_string(),
            api_key,
            temperature: settings.temperature.clone(),
            // This profile stands in for the `[llm]` section, so it inherits
            // that section's values rather than resetting them to the default.
            max_tokens: config.llm.max_tokens.to_string(),
            top_p: config.llm.top_p.map(|t| t.to_string()).unwrap_or_default(),
            extra_body: settings.extra_body.clone(),
            compact_at_tokens: String::new(),
        });
    }

    // Repair any collisions that survived from v0.7.0 pre-fix code.
    let profiles_before = profiles
        .iter()
        .map(|p| (p.name.clone(), p.key_env.clone()))
        .collect::<Vec<_>>();
    deduplicate_profiles(&mut profiles);
    let profiles_after: Vec<_> = profiles
        .iter()
        .map(|p| (p.name.clone(), p.key_env.clone()))
        .collect();
    if profiles_before != profiles_after {
        // Persist the repaired config immediately so the migration survives.
        Settings {
            model: settings.model.clone(),
            api_base_url: settings.api_base_url.clone(),
            ssh_profiles: settings.ssh_profiles.clone(),
            kube_profiles: settings.kube_profiles.clone(),
            last_kube: settings.last_kube.clone(),
            ssh_list_version: SSH_LIST_VERSION,
            last_ssh: settings.last_ssh,
            temperature: settings.temperature.clone(),
            extra_body: settings.extra_body.clone(),
            profiles: profiles.iter().map(LlmProfileData::to_profile).collect(),
            selected_profile: settings.selected_profile.min(profiles.len().saturating_sub(1)),
            save_dir: settings.save_dir.clone(),
            arbiter_profile: settings.arbiter_profile.clone(),
        }.save();
        tracing::info!("persisted deduplicated profile config on startup");
    }

    let selected_profile = settings.selected_profile.min(profiles.len().saturating_sub(1));

    let app = LauncherApp {
        sessions,
        selected_session: None,
        target_mode: if settings.last_ssh > 0 && settings.last_ssh <= ssh_slots.len() {
            settings.last_ssh
        } else {
            0
        },
        selected_kube,
        ssh_slots,
        kube_slots,
        next_kube_slot_id,
        kube_check_tx,
        kube_check_rx,
        manual_targets: strip_manual_ssh_passwords(&config.ssh_targets),
        profiles,
        selected_profile,
        validation_error: String::new(),
        profile_error_shown: false,
        save_dir: settings.save_dir.clone(),
        show_ssh_password: false,
        show_key_passphrase: false,
        show_api_key: false,
        arbiter_profile: settings.arbiter_profile.clone(),
        risk_scorer: {
            let mut d = RiskScorerDraft::from_config(config.risk_scorer.as_ref());
            // One OS-store read at start-up, not one per frame — and none for
            // a keyless scorer, whose key name is not in use.
            if !d.keyless {
                d.refresh_key_saved();
            }
            d
        },
        log_sources,
        next_log_source_id,
        log_check_tx,
        log_check_rx,
        import_dialog: None,
        file_status: String::new(),
        host_groups: config.host_groups.iter().map(HostGroupDraft::from_group).collect(),
        active_tab: 0,
        key_probe: None,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 620.0])
            .with_min_inner_size([440.0, 300.0])
            .with_title("Filar — Launcher")
            .with_icon(std::sync::Arc::new(load_icon())),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Filar",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    );
}

// ---------------------------------------------------------------------------
// Icon loading — embed PNG from pics/ folder at compile time
// ---------------------------------------------------------------------------

/// Load the window icon from the PNG file in `pics/filar_256.png`.
///
/// The PNG is embedded into the binary at compile time via `include_bytes!`,
/// then decoded to RGBA at runtime using the `image` crate.
fn load_icon() -> egui::IconData {
    let png_data = include_bytes!("../../../pics/filar_256.png");
    let img = image::load_from_memory(png_data)
        .expect("Failed to decode filar_256.png")
        .to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_secret_clipboard_strips_pasteboard_artifacts() {
        assert_eq!(sanitize_secret_clipboard("sk-abc\n"), "sk-abc");
        assert_eq!(sanitize_secret_clipboard("sk-abc\r\n"), "sk-abc");
        assert_eq!(sanitize_secret_clipboard("\u{feff}sk-abc\n"), "sk-abc");
        // Surrounding ASCII spaces are kept (valid in some SSH passwords).
        // Paste intercept runs before egui can turn a trailing newline into a space.
        assert_eq!(sanitize_secret_clipboard("  sk-abc  \n"), "  sk-abc  ");
        assert_eq!(sanitize_secret_clipboard("sk-abc "), "sk-abc ");
        assert_eq!(sanitize_secret_clipboard("pass word"), "pass word");
        assert_eq!(sanitize_secret_clipboard("line1\nline2"), "line1");
        assert_eq!(sanitize_secret_clipboard("\n"), "");
        assert_eq!(sanitize_secret_clipboard("sk-\u{200b}abc"), "sk-abc");
    }

    #[test]
    fn launch_config_serialization_excludes_secrets() {
        let cfg = LaunchConfig {
            target: "ssh".into(),
            ssh: Some(SshConnection {
                host: "10.0.0.5".into(),
                port: 22,
                user: "root".into(),
                password: "supersecret".into(),
                auth: SlotAuth::Key,
                key_path: Some("/home/ops/.ssh/deploy".into()),
                slot: 0,
                alias: "prod".into(),
                tags: vec!["prod".into()],
            }),
            kube: None,
            kube_clusters: None,
            model: "glm".into(),
            api_base_url: "https://api.example.com".into(),
            api_key: "sk-test-key-12345".into(),
            session_id: None,
            temperature: String::new(),
            extra_body: String::new(),
            selected_profile: None,
            key_env: "GLM_API_KEY".into(),
            save_dir: None,
            profiles: vec![],
            ssh_targets: vec![],
            arbiter_profile: None,
            host_groups: None,
            risk_scorer: None,
            log_sources: None,
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(!json.contains("supersecret"));
        assert!(!json.contains("sk-test-key-12345"));
        assert!(json.contains("10.0.0.5"));
        assert!(json.contains("glm"));
        // Round-trip: non-secret fields survive serialize→deserialize.
        let loaded: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.target, "ssh");
        assert_eq!(loaded.model, "glm");
        assert!(loaded.ssh.is_some());
        assert_eq!(loaded.ssh.as_ref().unwrap().slot, 0);
        assert_eq!(loaded.ssh.as_ref().unwrap().alias, "prod");
        assert_eq!(loaded.ssh.as_ref().unwrap().tags, vec!["prod"]);
        // The login method and key file reach the session (#489) — and the
        // handoff still does not trip the plaintext-secret heuristic.
        assert_eq!(loaded.ssh.as_ref().unwrap().auth, SlotAuth::Key);
        assert_eq!(
            loaded.ssh.as_ref().unwrap().key_path.as_deref(),
            Some(std::path::Path::new("/home/ops/.ssh/deploy"))
        );
        assert!(!json.contains("\"password\":"), "got {json}");
        // Secrets must be absent after deserialization (serde(skip) → default).
        assert!(loaded.api_key.is_empty());
        assert!(loaded.ssh.as_ref().unwrap().password.is_empty());
    }

    #[test]
    fn kube_handoff_and_config_keep_only_the_kubeconfig_path() {
        let cluster = filar_core::KubeCluster {
            name: "prod-eu".into(),
            context: "kind-prod".into(),
            kubeconfig: Some("C:/ops/prod.kubeconfig".into()),
            namespaces: vec!["shop".into()],
            default_namespace: Some("shop".into()),
            ..filar_core::KubeCluster::default()
        };
        let cfg = LaunchConfig {
            target: "prod-eu".into(),
            ssh: None,
            kube: Some(filar_core::KubeSessionSnapshot {
                target: "prod-eu".into(),
                namespace: Some("shop".into()),
                pod: None,
            }),
            kube_clusters: Some(vec![cluster.clone()]),
            model: "local".into(),
            api_base_url: "http://127.0.0.1:11434/v1".into(),
            api_key: String::new(),
            session_id: None,
            temperature: String::new(),
            extra_body: String::new(),
            selected_profile: None,
            key_env: String::new(),
            save_dir: None,
            profiles: vec![],
            ssh_targets: vec![],
            arbiter_profile: None,
            host_groups: None,
            risk_scorer: None,
            log_sources: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let loaded: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.kube.unwrap().namespace.as_deref(), Some("shop"));
        assert_eq!(loaded.kube_clusters.unwrap(), vec![cluster.clone()]);
        assert!(json.contains("C:/ops/prod.kubeconfig"));
        assert!(!json.contains("certificate-authority-data"));
        assert!(!json.contains("client-key-data"));
        let config = filar_core::Config {
            kube_clusters: vec![cluster],
            ..filar_core::Config::default()
        };
        let toml = toml::to_string(&config).unwrap();
        assert!(toml.contains("[[kube_clusters]]"));
        assert!(toml.contains("kubeconfig"));
        assert!(!toml.contains("certificate-authority-data"));
        assert!(!toml.contains("client-key-data"));
    }

    #[test]
    fn launch_target_name_uses_the_alias_or_slot_number_not_the_literal_ssh() {
        let slot = |alias: &str| SshSlot {
            host: "10.0.0.1".into(),
            user: "root".into(),
            alias: alias.into(),
            ..SshSlot::blank()
        };
        let slots = vec![slot("VPS DE"), slot("")];
        assert_eq!(launch_target_name(0, &slots), "local");
        assert_eq!(launch_target_name(1, &slots), "VPS DE");
        assert_eq!(launch_target_name(2, &slots), "SSH2");
        assert_ne!(launch_target_name(1, &slots), "ssh");
    }

    #[test]
    fn ssh_connection_serialization_excludes_password() {
        let conn = SshConnection {
            host: "host".into(),
            port: 22,
            user: "admin".into(),
            password: "p@ssw0rd".into(),
            auth: SlotAuth::Password,
            key_path: None,
            slot: 2,
            alias: String::new(),
            tags: Vec::new(),
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(!json.contains("p@ssw0rd"));
        assert!(json.contains("host"));
        // Round-trip: non-secret fields survive, secret doesn't.
        let loaded: SshConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.host, "host");
        assert_eq!(loaded.slot, 2);
        assert!(loaded.password.is_empty());
    }

    #[test]
    fn a_pre_489_handoff_file_means_password_login() {
        // A handoff written before the login method existed was always
        // password login; reading it must not switch to the agent (#489).
        let old = r#"{"host":"h","port":22,"user":"u","slot":0,"alias":"web-1"}"#;
        let conn: SshConnection = serde_json::from_str(old).unwrap();
        assert_eq!(conn.auth, SlotAuth::Password);
        assert!(conn.key_path.is_none());
    }

    #[test]
    fn ssh_connection_alias_survives_round_trip() {
        let conn = SshConnection {
            host: "h".into(),
            port: 22,
            user: "u".into(),
            password: "secret".into(),
            auth: SlotAuth::Password,
            key_path: None,
            slot: 1,
            alias: "VPS DE".into(),
            tags: Vec::new(),
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(json.contains("VPS DE"));
        assert!(!json.contains("secret"));
        let loaded: SshConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.alias, "VPS DE");
        assert_eq!(
            filar_core::ssh_cred_name(loaded.slot, &loaded.alias),
            "ssh_target:VPS DE"
        );
    }

    #[test]
    fn launch_config_serialization_omits_password_key() {
        // Regression: SshAuth::Password { password: None } must NOT emit a
        // `"password":` key, otherwise load_pending_launch() mistakes the file
        // for an old-format plaintext-secret payload and deletes it (#258).
        let cfg = LaunchConfig {
            target: "ssh".into(),
            ssh: None,
            kube: None,
            kube_clusters: None,
            model: "glm".into(),
            api_base_url: "https://api.example.com".into(),
            api_key: String::new(),
            session_id: None,
            temperature: String::new(),
            extra_body: String::new(),
            selected_profile: None,
            key_env: "GLM_API_KEY".into(),
            save_dir: None,
            profiles: vec![],
            ssh_targets: vec![filar_core::SshTarget {
                name: "srv".into(),
                host: "10.0.0.1".into(),
                port: 22,
                user: "root".into(),
                auth: filar_core::SshAuth::Password { password: None },
                host_key_policy: filar_core::HostKeyPolicy::Tofu,
                tags: Vec::new(),
            }],
            arbiter_profile: None,
            host_groups: None,
            risk_scorer: None,
            log_sources: None,
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(!json.contains("\"password\":"), "must not emit password key when None, got: {json}");
        assert!(json.contains("10.0.0.1"), "ssh target must survive");
        // And the secret-detection heuristic in load_pending_launch must not trip.
        assert!(!json.contains("\"api_key\":"), "must not emit api_key");
    }

    /// A profile as edited in the launcher, with every optional field filled
    /// in with a non-default value. Anything a conversion drops shows up as a
    /// changed field on the way back.
    fn edited_profile() -> LlmProfileData {
        LlmProfileData {
            name: "glm".into(),
            model: "glm-5.1".into(),
            api_base_url: "https://example.invalid/v1".into(),
            key_env: "GLM_API_KEY".into(),
            api_key: String::new(),
            temperature: "0.3".into(),
            max_tokens: "32000".into(),
            top_p: "0.85".into(),
            extra_body: String::new(),
            compact_at_tokens: "120000".into(),
        }
    }

    #[test]
    fn compact_threshold_survives_the_launcher_round_trip() {
        // The launcher edits profiles through a flat string struct. A field
        // missing from the conversion is reset without any error — which is
        // exactly how `max_tokens` was lost (see #380).
        let edited = edited_profile();
        let stored = edited.to_profile();
        assert_eq!(stored.compact_at_tokens, 120_000);

        // …and back into the editor, unchanged.
        assert_eq!(stored.compact_at_tokens.to_string(), edited.compact_at_tokens);

        // The value must also survive serialisation into settings.json /
        // pending_launch.json.
        let json = serde_json::to_string(&stored).unwrap();
        let loaded: filar_core::LlmProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.compact_at_tokens, 120_000);
    }

    #[test]
    fn max_tokens_and_top_p_survive_the_launcher_round_trip() {
        // Regression for #380: both fields were hard-coded in the conversion
        // (`max_tokens: 4096, top_p: None`), so every save reset them. On the
        // pre-fix code this test fails at the first assertion.
        let edited = edited_profile();
        let stored = edited.to_profile();

        assert_eq!(stored.max_tokens, 32_000, "max_tokens must not be reset");
        assert_eq!(stored.top_p, Some(0.85), "top_p must not be dropped");
        assert_eq!(stored.temperature, Some(0.3), "temperature must survive too");

        // Through settings.json / pending_launch.json…
        let json = serde_json::to_string(&stored).unwrap();
        let loaded: filar_core::LlmProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.max_tokens, 32_000);
        assert_eq!(loaded.top_p, Some(0.85));

        // …and back into the editor, so the next save keeps them as well.
        let reopened = LlmProfileData::from_profile(&loaded, String::new());
        assert_eq!(reopened.max_tokens, "32000");
        assert_eq!(reopened.top_p, "0.85");
        assert_eq!(reopened.to_profile().max_tokens, 32_000);
        assert_eq!(reopened.to_profile().top_p, Some(0.85));
    }

    #[test]
    fn pending_launch_carries_max_tokens_and_top_p_to_the_tui() {
        // The handoff file is what the TUI actually reads (#255), so the
        // values have to arrive there, not only in settings.json.
        let cfg = LaunchConfig {
            target: "local".into(),
            ssh: None,
            kube: None,
            kube_clusters: None,
            model: "glm-5.1".into(),
            api_base_url: "https://example.invalid/v1".into(),
            api_key: String::new(),
            session_id: None,
            temperature: "0.3".into(),
            extra_body: String::new(),
            selected_profile: Some("glm".into()),
            key_env: "GLM_API_KEY".into(),
            save_dir: None,
            profiles: vec![edited_profile().to_profile()],
            ssh_targets: vec![],
            arbiter_profile: None,
            host_groups: None,
            risk_scorer: None,
            log_sources: None,
        };

        let json = serde_json::to_string(&cfg).unwrap();
        let loaded: LaunchConfig = serde_json::from_str(&json).unwrap();
        let profile = &loaded.profiles[0];
        assert_eq!(profile.max_tokens, 32_000, "must not arrive as the 4096 default");
        assert_eq!(profile.top_p, Some(0.85), "must not arrive as None");
    }

    #[test]
    fn blank_max_tokens_falls_back_to_the_default_and_blank_top_p_to_none() {
        assert_eq!(parse_max_tokens(""), filar_core::DEFAULT_MAX_TOKENS);
        assert_eq!(parse_max_tokens("   "), filar_core::DEFAULT_MAX_TOKENS);
        assert_eq!(parse_max_tokens("nonsense"), filar_core::DEFAULT_MAX_TOKENS);
        // `0` would mean "generate nothing", so it is never worth saving.
        assert_eq!(parse_max_tokens("0"), filar_core::DEFAULT_MAX_TOKENS);
        assert_eq!(parse_max_tokens(" 8192 "), 8_192);

        let blank = LlmProfileData {
            max_tokens: String::new(),
            top_p: String::new(),
            ..edited_profile()
        };
        let stored = blank.to_profile();
        assert_eq!(stored.max_tokens, filar_core::DEFAULT_MAX_TOKENS);
        assert_eq!(stored.top_p, None, "empty top_p means provider default");
    }

    #[test]
    fn blank_compact_threshold_falls_back_to_the_default_not_to_zero() {
        // `0` means "compaction disabled"; an empty box must not mean that.
        assert_eq!(
            parse_compact_at_tokens(""),
            filar_core::DEFAULT_COMPACT_AT_TOKENS
        );
        assert_eq!(
            parse_compact_at_tokens("  "),
            filar_core::DEFAULT_COMPACT_AT_TOKENS
        );
        assert_eq!(
            parse_compact_at_tokens("nonsense"),
            filar_core::DEFAULT_COMPACT_AT_TOKENS
        );
        assert_eq!(parse_compact_at_tokens("0"), 0, "an explicit 0 disables it");
        assert_eq!(parse_compact_at_tokens(" 65536 "), 65_536);
    }

    /// Build a launcher holding `profiles`, with the first one selected.
    fn app_with_profiles(profiles: Vec<LlmProfileData>) -> LauncherApp {
        let mut app = make_app(make_meta(None, None, None, None));
        app.profiles = profiles;
        app.selected_profile = 0;
        app
    }

    /// The four fields `to_profile` converts with a fallback, each with a value
    /// that fails to parse and the wording the message must carry.
    fn invalid_field_cases() -> Vec<(&'static str, LlmProfileData, &'static str)> {
        vec![
            (
                "temperature",
                LlmProfileData { name: "other".into(), temperature: "5".into(), ..edited_profile() },
                "Invalid temperature",
            ),
            (
                "max_tokens",
                LlmProfileData { name: "other".into(), max_tokens: "abc".into(), ..edited_profile() },
                "Invalid max tokens",
            ),
            (
                "top_p",
                LlmProfileData { name: "other".into(), top_p: "2.5".into(), ..edited_profile() },
                "Invalid top_p",
            ),
            (
                "compact_at_tokens",
                LlmProfileData { name: "other".into(), compact_at_tokens: "-1".into(), ..edited_profile() },
                "Invalid compaction threshold",
            ),
        ]
    }

    #[test]
    fn save_is_blocked_by_an_invalid_field_in_an_unselected_profile() {
        // The regression from #385: validation looked at the selected profile
        // only, so a typo left in another one was rewritten as a default by the
        // next save — adding a profile with "+" was enough to trigger it.
        for (field, bad, expected) in invalid_field_cases() {
            let selected = LlmProfileData { name: "selected".into(), ..edited_profile() };
            let mut app = app_with_profiles(vec![selected, bad]);

            app.save_profiles();

            assert!(
                app.validation_error.contains(expected),
                "{field}: message must name the field, got {:?}",
                app.validation_error
            );
            assert!(
                app.validation_error.contains("other"),
                "{field}: message must name the offending profile, got {:?}",
                app.validation_error
            );
            assert!(
                app.profile_error_shown,
                "{field}: the error must be marked as ours so a later save can clear it"
            );
        }
    }

    #[test]
    fn launch_is_blocked_by_an_invalid_field_in_an_unselected_profile() {
        // Launch writes every profile to settings.json and pending_launch.json
        // too, so it needs the same check. `validate_launch` is the part of
        // `do_launch` that can be called here — the rest exits the process.
        for (field, bad, expected) in invalid_field_cases() {
            let selected = LlmProfileData { name: "selected".into(), ..edited_profile() };
            let mut app = app_with_profiles(vec![selected, bad]);

            assert!(!app.validate_launch(), "{field}: launch must be refused");
            assert!(
                app.validation_error.contains(expected),
                "{field}: launch must report the field, got {:?}",
                app.validation_error
            );
            assert!(
                app.validation_error.contains("other"),
                "{field}: launch must name the offending profile, got {:?}",
                app.validation_error
            );
        }
    }

    #[test]
    fn launch_accepts_a_list_where_every_profile_is_valid() {
        let mut app = app_with_profiles(vec![
            LlmProfileData { name: "one".into(), ..edited_profile() },
            LlmProfileData { name: "two".into(), temperature: String::new(), ..edited_profile() },
        ]);
        assert!(app.validate_launch(), "got {:?}", app.validation_error);
    }

    #[test]
    fn valid_and_empty_fields_pass_validation() {
        let empty = LlmProfileData {
            name: "blank".into(),
            temperature: String::new(),
            max_tokens: String::new(),
            top_p: String::new(),
            compact_at_tokens: String::new(),
            extra_body: String::new(),
            ..edited_profile()
        };
        assert!(validate_all_profile_fields(&[edited_profile(), empty]).is_ok());
    }

    #[test]
    fn a_successful_save_leaves_an_unrelated_warning_alone() {
        // `validation_error` is one slot shared with the SSH-target warning
        // from session selection. Clearing a profile error must not take that
        // with it.
        let mut app = app_with_profiles(vec![edited_profile()]);
        app.set_other_error("No SSH profile matches 'root@10.0.0.9'.".into());

        app.clear_profile_error();

        assert_eq!(
            app.validation_error, "No SSH profile matches 'root@10.0.0.9'.",
            "an SSH warning is not ours to clear"
        );

        app.set_profile_error("Profile \"x\": Invalid top_p: '9'.".into());
        app.clear_profile_error();
        assert!(app.validation_error.is_empty(), "our own error must clear");
    }

    #[test]
    fn a_warning_arriving_after_a_profile_error_survives_the_next_save() {
        // The first version of the test above only covered a slot that was
        // never ours. The reachable case is the other order: a profile error is
        // shown, the user then clicks a session whose SSH target does not
        // match, and that warning overwrites the text. If the flag stayed set,
        // the next successful save would wipe the warning (review of #389).
        let mut app = app_with_profiles(vec![edited_profile()]);
        app.set_profile_error("Profile \"x\": Invalid top_p: '9'.".into());

        app.set_other_error("No SSH profile matches 'root@10.0.0.9'.".into());
        app.clear_profile_error();

        assert_eq!(
            app.validation_error, "No SSH profile matches 'root@10.0.0.9'.",
            "the warning that replaced our error is not ours to clear"
        );
    }

    #[test]
    fn deleting_a_profile_is_refused_before_the_credential_is_dropped() {
        // `delete_secret` cannot be undone and `save_profiles` can now refuse,
        // so the list has to be checked *without* the doomed profile before
        // anything is removed — otherwise settings.json keeps a profile whose
        // API key is already gone from the OS store (review of #389).
        let doomed = LlmProfileData { name: "doomed".into(), ..edited_profile() };
        let broken = LlmProfileData {
            name: "broken".into(),
            max_tokens: "abc".into(),
            ..edited_profile()
        };
        let profiles = vec![doomed, broken];

        // What the delete handler checks: everything except the selected entry.
        let remaining = profiles.iter().enumerate().filter(|(i, _)| *i != 0).map(|(_, p)| p);
        let verdict = validate_all_profile_fields(remaining);

        assert!(
            verdict.is_err(),
            "an invalid profile elsewhere must block the delete before delete_secret runs"
        );
        assert!(verdict.unwrap_err().contains("broken"));
    }

    #[test]
    fn deleting_a_profile_is_allowed_when_the_rest_are_valid() {
        let profiles = vec![
            LlmProfileData { name: "doomed".into(), ..edited_profile() },
            LlmProfileData { name: "fine".into(), ..edited_profile() },
        ];
        let remaining = profiles.iter().enumerate().filter(|(i, _)| *i != 0).map(|(_, p)| p);
        assert!(validate_all_profile_fields(remaining).is_ok());
    }

    #[test]
    fn launch_config_round_trips_arbiter_profile() {        let cfg = LaunchConfig {
            target: "local".into(),
            ssh: None,
            kube: None,
            kube_clusters: None,
            model: "glm".into(),
            api_base_url: "https://api.example.com".into(),
            api_key: String::new(),
            session_id: None,
            temperature: String::new(),
            extra_body: String::new(),
            selected_profile: Some("session-a".into()),
            key_env: "GLM_API_KEY".into(),
            save_dir: None,
            profiles: vec![],
            ssh_targets: vec![],
            arbiter_profile: Some("arbiter-b".into()),
            host_groups: None,
            risk_scorer: None,
            log_sources: None,
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let loaded: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.arbiter_profile.as_deref(), Some("arbiter-b"));
    }

    /// #501: the Groups tab travels with the launch; "all removed" and
    /// "not said" (a file from before #501) stay distinguishable.
    #[test]
    fn launch_config_carries_host_groups() {
        let json = r#"{"target":"local","ssh":null,"model":"m","api_base_url":"u","session_id":null}"#;
        let old: LaunchConfig = serde_json::from_str(json).unwrap();
        assert!(old.host_groups.is_none(), "a pre-#501 file says nothing about groups");

        let group: filar_core::HostGroup =
            toml::from_str("name = \"sim-all\"\nmatch = [\"sim\"]").unwrap();
        for groups in [vec![], vec![group]] {
            let mut cfg: LaunchConfig = serde_json::from_str(json).unwrap();
            cfg.host_groups = Some(groups.clone());
            let loaded: LaunchConfig =
                serde_json::from_str(&serde_json::to_string(&cfg).unwrap()).unwrap();
            assert_eq!(loaded.host_groups, Some(groups));
        }
    }

    #[test]
    fn settings_save_pattern_preserves_data() {
        // Test the internal pattern: create_dir_all before write,
        // then write→read round-trip verifying data integrity.
        let dir = std::env::temp_dir().join(format!("filar_test_save_{}", std::process::id()));
        let file = dir.join("filar").join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!file.parent().unwrap().exists());
        // Replicate what Settings::save does: create_dir_all + write.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let s = Settings {
            model: "test-model".into(), api_base_url: "https://example.com".into(),
            ssh_profiles: vec![], kube_profiles: vec![], last_kube: None,
            ssh_list_version: SSH_LIST_VERSION, last_ssh: 0, temperature: "0.5".into(),
            extra_body: String::new(),
            profiles: vec![],
            selected_profile: 0,
            save_dir: None,
            arbiter_profile: None,
        };
        std::fs::write(&file, serde_json::to_string_pretty(&s).unwrap()).unwrap();

        let loaded: Settings = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.temperature, "0.5");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_migrates_a_valid_pre_411_file_and_persists_it() {
        let dir = std::env::temp_dir().join(format!("filar_test_migrate_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        // `model`/`api_base_url` are the only fields without serde defaults.
        let old = r#"{"model":"","api_base_url":"","ssh_profiles":[
            {"host":"10.0.0.11","port":"22","user":"root"},
            {"host":"","port":"22","user":""}
        ]}"#;
        std::fs::write(&file, old).unwrap();

        let settings = Settings::load_from(Some(file.clone()));
        assert_eq!(settings.ssh_profiles.len(), 1, "blank placeholder dropped");
        assert_eq!(settings.ssh_profiles[0].alias, "SSH1");

        // The migrated file is on disk right away, marker included.
        let reloaded: Settings =
            serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(reloaded.ssh_list_version, SSH_LIST_VERSION);
        assert_eq!(reloaded.ssh_profiles[0].alias, "SSH1");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_from_leaves_a_corrupt_settings_file_untouched() {
        let dir = std::env::temp_dir().join(format!("filar_test_corrupt_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("settings.json");
        let corrupt = "{ this is not json";
        std::fs::write(&file, corrupt).unwrap();

        let settings = Settings::load_from(Some(file.clone()));
        assert!(settings.ssh_profiles.is_empty(), "fallback is in-memory only");
        // The bytes — the only copy of the user's host list and profiles — must stay.
        assert_eq!(std::fs::read_to_string(&file).unwrap(), corrupt);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_save_preserves_existing_sections() {
        let dir = std::env::temp_dir().join(format!("filar_test_cfg_{}", std::process::id()));
        let app_dir = dir.join("filar");
        let path = app_dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        // Write a config with non-LLM sections that must survive.
        let existing = "[llm]\nmodel = \"old\"\napi_base_url = \"https://old.example.com\"\n\n[[ssh_targets]]\nname = \"dev\"\nhost = \"10.0.0.1\"\nuser = \"root\"\nauth = { type = \"agent\" }\n";
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(&path, existing).unwrap();

        // Simulate what save_config_toml does after loading the config.
        let mut config: filar_core::Config = filar_core::Config::load(&path).unwrap();
        config.llm.model = "new".into();
        let saved = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&path, &saved).unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("model = \"new\""), "model must be updated");
        assert!(result.contains("[[ssh_targets]]"), "ssh_targets must survive");
        assert!(result.contains("dev"), "ssh target name must survive");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_save_writes_launcher_ssh_profiles() {
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "2222".into(), user: "admin".into(), alias: String::new(), tags: "  ".into(), auth: Some(SlotAuth::Key), key_path: " /home/ops/.ssh/deploy ".into(), ..SshProfile::default() },
            SshProfile { host: "10.0.0.2".into(), port: "22".into(), user: "root".into(), alias: "prod-web".into(), tags: "prod, web".into(), save_password: true, auth: Some(SlotAuth::Password), ..SshProfile::default() },
            SshProfile::default(), // empty slot — skipped
            SshProfile::default(),
            SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert_eq!(result.len(), 2, "only non-empty profiles become targets");
        assert_eq!(result[0].name, "SSH1", "no alias → uses slot name");
        assert_eq!(result[0].host, "10.0.0.1");
        assert_eq!(result[0].port, 2222);
        assert_eq!(result[1].name, "prod-web", "alias overrides slot name");
        assert_eq!(result[1].host, "10.0.0.2");
        assert_eq!(result[1].port, 22);
        // Auth follows the slot's login method, key path included (#489).
        match &result[0].auth {
            filar_core::SshAuth::Key { path, passphrase } => {
                assert_eq!(path.as_deref(), Some(std::path::Path::new("/home/ops/.ssh/deploy")));
                assert!(passphrase.is_none(), "no secret in the synced target");
            }
            other => panic!("expected Key auth, got {other:?}"),
        }
        assert!(matches!(result[1].auth, filar_core::SshAuth::Password { password: None }));
        // Tags pass through the launcher profile → SshTarget conversion; a
        // whitespace-only tag field is simply empty (#413).
        assert!(result[0].tags.is_empty(), "blank tag field → no tags");
        assert_eq!(result[1].tags, vec!["prod", "web"]);
    }

    #[test]
    fn build_rewrites_all_targets_no_preservation() {
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "22".into(), user: "admin".into(), alias: String::new(), tags: String::new(), save_password: false, ..SshProfile::default() },
            SshProfile::default(), SshProfile::default(), SshProfile::default(), SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert_eq!(result.len(), 1, "full rewrite only produces current profiles");
        assert_eq!(result[0].name, "SSH1");
    }

    #[test]
    fn build_removes_old_target_when_alias_changes() {
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "22".into(), user: "admin".into(), alias: "prod-api".into(), tags: String::new(), save_password: false, ..SshProfile::default() },
            SshProfile::default(), SshProfile::default(), SshProfile::default(), SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert!(result.iter().any(|t| t.name == "prod-api"), "new alias target must be added");
        assert!(result.iter().all(|t| t.name != "prod-web"), "old alias target with matching host must be removed");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn build_removes_old_target_by_host_match() {
        // Profile with no alias — uses slot name SSH1. Old stale target
        // with same host/port/user but different name must be removed.
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "22".into(), user: "admin".into(), alias: String::new(), tags: String::new(), save_password: false, ..SshProfile::default() },
            SshProfile::default(), SshProfile::default(), SshProfile::default(), SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert_eq!(result.len(), 1, "only SSH1 must remain, stale target removed");
        assert_eq!(result[0].name, "SSH1");
    }

    #[test]
    fn build_clears_when_all_profiles_empty() {
        // All profiles cleared (empty).
        let profiles = vec![SshProfile::default(); 5];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert!(result.is_empty(), "all launcher targets must be removed when slots are cleared");
    }

    #[test]
    fn sync_maps_every_login_method_and_old_profiles_to_password() {
        let profile = |auth: Option<SlotAuth>, save_password: bool| SshProfile {
            host: "10.0.0.1".into(),
            alias: "web-1".into(),
            save_password,
            auth,
            ..SshProfile::default()
        };
        let auth_of = |p: SshProfile| build_ssh_targets_from_profiles(&[p]).remove(0).auth;
        assert!(matches!(auth_of(profile(Some(SlotAuth::Agent), false)), filar_core::SshAuth::Agent));
        assert!(matches!(
            auth_of(profile(Some(SlotAuth::Key), false)),
            filar_core::SshAuth::Key { path: None, .. }
        ));
        // A pre-#489 slot logged in by password whatever its save flag said:
        // Launch always built password auth, so the migration keeps it.
        for save in [true, false] {
            assert!(matches!(
                auth_of(profile(None, save)),
                filar_core::SshAuth::Password { password: None }
            ));
        }
    }

    // ── Manual `[[ssh_targets]]` from config.toml (#412) ─────────────

    fn config_target(name: &str, host: &str) -> filar_core::SshTarget {
        filar_core::SshTarget {
            name: name.into(),
            host: host.into(),
            port: 22,
            user: "ops".into(),
            auth: filar_core::SshAuth::Agent,
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
            tags: Vec::new(),
        }
    }

    #[test]
    fn a_manual_config_host_survives_a_launcher_save() {
        // A host written into config.toml by hand or by a script must be part
        // of the launch payload next to the launcher's own targets — not
        // shadowed by their full rebuild (#412).
        let mut fleet = config_target("fleet-01", "10.9.0.1");
        fleet.port = 2222;
        fleet.host_key_policy = filar_core::HostKeyPolicy::Strict;
        let merged = merge_manual_ssh_targets(
            build_ssh_targets_from_profiles(&[ssh_profile("10.0.0.11", "web-1")]),
            &[fleet],
        );
        assert_eq!(merged.len(), 2, "the manual host must not be dropped");
        assert_eq!(merged[0].name, "web-1", "launcher targets come first");
        assert_eq!(merged[1].name, "fleet-01", "manual entries follow");
        assert_eq!(merged[1].host, "10.9.0.1");
        assert_eq!(merged[1].port, 2222, "manual fields pass through untouched");
        assert_eq!(merged[1].host_key_policy, filar_core::HostKeyPolicy::Strict);
    }

    #[test]
    fn a_manual_host_never_shadows_a_launcher_target() {
        let merged = merge_manual_ssh_targets(
            build_ssh_targets_from_profiles(&[ssh_profile("10.0.0.11", "web-1")]),
            &[config_target("web-1", "10.9.0.1"), config_target("fleet-01", "10.9.0.2")],
        );
        let web_1: Vec<_> = merged.iter().filter(|t| t.name == "web-1").collect();
        assert_eq!(web_1.len(), 1, "one entry per name");
        assert_eq!(web_1[0].host, "10.0.0.11", "the launcher's host is the one that connects");
        assert!(
            merged.iter().any(|t| t.name == "fleet-01"),
            "the non-colliding manual host stays"
        );
    }

    #[test]
    fn a_host_removed_in_the_launcher_disappears_from_the_merged_list() {
        let manual = [config_target("fleet-01", "10.9.0.1")];
        let before = merge_manual_ssh_targets(
            build_ssh_targets_from_profiles(&[
                ssh_profile("10.0.0.11", "web-1"),
                ssh_profile("10.0.0.12", "web-2"),
            ]),
            &manual,
        );
        assert!(before.iter().any(|t| t.name == "web-1"));

        // The user removed web-1 in the launcher — its target must be gone,
        // while the manual host is unaffected.
        let after = merge_manual_ssh_targets(
            build_ssh_targets_from_profiles(&[ssh_profile("10.0.0.12", "web-2")]),
            &manual,
        );
        assert!(after.iter().all(|t| t.name != "web-1"), "removed launcher host must be gone");
        assert!(after.iter().any(|t| t.name == "web-2"));
        assert!(after.iter().any(|t| t.name == "fleet-01"), "the manual host stays");
    }

    #[test]
    fn a_manual_password_never_reaches_the_launch_payload() {
        let mut target = config_target("pw-host", "10.9.0.88");
        target.auth = filar_core::SshAuth::Password { password: Some("hunter2".into()) };
        let merged = merge_manual_ssh_targets(Vec::new(), &strip_manual_ssh_passwords(&[target]));

        // `pending_launch.json` is plain text and `load_pending_launch`
        // rejects any file that carries a password key — the secret must be
        // gone while the auth variant survives (the keyring/prompt resolves
        // the credential).
        let json = serde_json::to_string(&merged).expect("launch payload must serialize");
        assert!(!json.contains("\"password\":"), "no password key in the payload: {json}");
        assert!(!json.contains("hunter2"), "no password value in the payload: {json}");
        assert!(json.contains("\"type\":\"password\""), "password auth stays: {json}");
    }

    #[test]
    fn unique_profile_name_fills_gaps() {
        let existing = vec![
            LlmProfileData { name: "profile-1".into(), model: String::new(), api_base_url: String::new(), key_env: String::new(), api_key: String::new(), temperature: String::new(), max_tokens: String::new(), top_p: String::new(), extra_body: String::new(), compact_at_tokens: String::new() },
            LlmProfileData { name: "profile-3".into(), model: String::new(), api_base_url: String::new(), key_env: String::new(), api_key: String::new(), temperature: String::new(), max_tokens: String::new(), top_p: String::new(), extra_body: String::new(), compact_at_tokens: String::new() },
        ];
        let name = unique_profile_name(&existing, "profile");
        assert_eq!(name, "profile-2", "must find first free number, not len+1");
    }

    #[test]
    fn unique_profile_name_first_free() {
        let existing = vec![] as Vec<LlmProfileData>;
        let name = unique_profile_name(&existing, "profile");
        assert_eq!(name, "profile-1");
    }

    #[test]
    fn deduplicate_profiles_renames_collisions() {
        let mut profiles = vec![
            LlmProfileData { name: "dup".into(), model: String::new(), api_base_url: String::new(), key_env: "k1".into(), api_key: String::new(), temperature: String::new(), max_tokens: String::new(), top_p: String::new(), extra_body: String::new(), compact_at_tokens: String::new() },
            LlmProfileData { name: "dup".into(), model: String::new(), api_base_url: String::new(), key_env: "k2".into(), api_key: String::new(), temperature: String::new(), max_tokens: String::new(), top_p: String::new(), extra_body: String::new(), compact_at_tokens: String::new() },
        ];
        deduplicate_profiles(&mut profiles);
        assert_ne!(profiles[0].name, profiles[1].name, "names must differ after dedup");
    }

    #[test]
    fn deduplicate_three_identical_names_completes() {
        let mut profiles = vec![
            LlmProfileData { name: "x".into(), model: String::new(), api_base_url: String::new(), key_env: "k1".into(), api_key: String::new(), temperature: String::new(), max_tokens: String::new(), top_p: String::new(), extra_body: String::new(), compact_at_tokens: String::new() },
            LlmProfileData { name: "x".into(), model: String::new(), api_base_url: String::new(), key_env: "k2".into(), api_key: String::new(), temperature: String::new(), max_tokens: String::new(), top_p: String::new(), extra_body: String::new(), compact_at_tokens: String::new() },
            LlmProfileData { name: "x".into(), model: String::new(), api_base_url: String::new(), key_env: "k3".into(), api_key: String::new(), temperature: String::new(), max_tokens: String::new(), top_p: String::new(), extra_body: String::new(), compact_at_tokens: String::new() },
        ];
        deduplicate_profiles(&mut profiles);
        assert_ne!(profiles[0].name, profiles[1].name);
        assert_ne!(profiles[1].name, profiles[2].name);
        assert_ne!(profiles[0].name, profiles[2].name, "all three must be unique after dedup");
    }

    #[test]
    fn ssh_cred_name_empty_alias_uses_slot() {
        assert_eq!(ssh_cred_name(0, ""), "ssh_target:SSH1");
        assert_eq!(ssh_cred_name(4, ""), "ssh_target:SSH5");
    }

    #[test]
    fn ssh_cred_name_nonempty_alias_uses_alias() {
        assert_eq!(ssh_cred_name(0, "VPS DE"), "ssh_target:VPS DE");
        assert_eq!(ssh_cred_name(2, "prod-web"), "ssh_target:prod-web");
    }

    #[test]
    fn ssh_cred_name_special_chars_preserved() {
        assert_eq!(ssh_cred_name(0, "my server!"), "ssh_target:my server!");
        assert_eq!(ssh_cred_name(1, "сервер"), "ssh_target:сервер");
    }

    fn make_meta(ssh_info: Option<&str>, llm_profile: Option<&str>, model: Option<&str>, api_base_url: Option<&str>) -> SessionMeta {
        SessionMeta {
            id: "1".into(),
            timestamp: "2026-08-14 00:00:00".into(),
            target: "local".into(),
            llm_profile: llm_profile.map(str::to_string),
            ssh_info: ssh_info.map(str::to_string),
            model: model.map(str::to_string),
            api_base_url: api_base_url.map(str::to_string),
            preview: "hi".into(),
            fleet_group: None,
        }
    }

    fn make_app(meta: SessionMeta) -> LauncherApp {
        let (kube_check_tx, kube_check_rx) = std::sync::mpsc::channel();
        let (log_check_tx, log_check_rx) = std::sync::mpsc::channel();
        LauncherApp {
            sessions: vec![meta],
            selected_session: None,
            target_mode: 0,
            selected_kube: None,
            ssh_slots: vec![SshSlot {
                host: "10.0.0.5".into(),
                user: "root".into(),
                alias: "SSH1".into(),
                auth: SlotAuth::Password,
                ..SshSlot::blank()
            }],
            kube_slots: Vec::new(),
            next_kube_slot_id: 0,
            kube_check_tx,
            kube_check_rx,
            manual_targets: Vec::new(),
            profiles: vec![LlmProfileData {
                name: "glm".into(),
                model: String::new(),
                api_base_url: String::new(),
                key_env: "GLM_API_KEY".into(),
                api_key: String::new(),
                temperature: String::new(),
                max_tokens: String::new(),
                top_p: String::new(),
                extra_body: String::new(),
                compact_at_tokens: String::new(),
            }],
            selected_profile: 0,
            validation_error: String::new(),
            profile_error_shown: false,
            save_dir: None,
            show_ssh_password: false,
            show_key_passphrase: false,
            show_api_key: false,
            arbiter_profile: None,
            risk_scorer: RiskScorerDraft::from_config(None),
            log_sources: Vec::new(),
            next_log_source_id: 0,
            log_check_tx,
            log_check_rx,
            import_dialog: None,
            file_status: String::new(),
            host_groups: Vec::new(),
            active_tab: 0,
            key_probe: None,
        }
    }

    #[test]
    fn parse_ssh_host_port_parses_user_host_port() {
        assert_eq!(
            parse_ssh_host_port("root@10.0.0.5:22"),
            Some(("10.0.0.5".to_string(), 22))
        );
        assert_eq!(
            parse_ssh_host_port("admin@devbox"),
            Some(("devbox".to_string(), 22))
        );
        assert_eq!(
            parse_ssh_host_port("root@[::1]:2222"),
            Some(("::1".to_string(), 2222))
        );
        assert!(parse_ssh_host_port("no-at-sign").is_none());
    }

    #[test]
    fn session_click_autoselects_ssh_and_profile() {
        let meta = make_meta(
            Some("root@10.0.0.5:22"),
            Some("glm"),
            Some("glm-5.1"),
            Some("https://example.com"),
        );
        let mut app = make_app(meta);
        app.on_session_selected(0);
        assert_eq!(app.target_mode, 1, "matching SSH slot must be selected");
        assert_eq!(app.selected_profile, 0);
        assert_eq!(app.profiles[0].model, "glm-5.1");
        assert_eq!(app.profiles[0].api_base_url, "https://example.com");
        assert!(app.validation_error.is_empty());
    }

    #[test]
    fn session_click_without_ssh_info_stays_local() {
        let meta = make_meta(None, None, None, None);
        let mut app = make_app(meta);
        app.on_session_selected(0);
        assert_eq!(app.target_mode, 0, "no ssh_info → Local");
    }

    #[test]
    fn session_click_unmatched_ssh_warns_and_stays_local() {
        let meta = make_meta(Some("root@192.168.9.9:22"), None, None, None);
        let mut app = make_app(meta);
        app.on_session_selected(0);
        assert_eq!(app.target_mode, 0, "no matching slot → Local");
        assert!(
            app.validation_error.contains("No SSH profile matches"),
            "must warn about unmatched ssh_info"
        );
    }

    #[test]
    fn session_click_unknown_profile_does_not_overwrite() {
        let meta = make_meta(
            None,
            Some("nonexistent"),
            Some("other-model"),
            Some("https://other.example.com"),
        );
        let mut app = make_app(meta);
        app.profiles[0].model = "keep-me".into();
        app.profiles[0].api_base_url = "https://keep.example.com".into();
        app.on_session_selected(0);
        assert_eq!(app.selected_profile, 0, "no matching profile → keep current");
        assert_eq!(app.profiles[0].model, "keep-me", "unrelated profile must not be overwritten");
        assert_eq!(app.profiles[0].api_base_url, "https://keep.example.com");
    }

    #[test]
    fn session_click_matches_default_port_when_slot_empty() {
        let meta = make_meta(Some("root@10.0.0.5"), None, None, None);
        let mut app = make_app(meta);
        app.ssh_slots[0].port = String::new();
        app.on_session_selected(0);
        assert_eq!(app.target_mode, 1, "empty slot port must be treated as 22");
    }

    #[test]
    fn session_click_clears_stale_warning_on_local() {
        let meta = make_meta(None, None, None, None);
        let mut app = make_app(meta);
        app.validation_error = "stale warning".into();
        app.on_session_selected(0);
        assert!(app.validation_error.is_empty(), "Local selection must clear stale warning");
    }

    #[test]
    fn session_click_flat_model_fills_current_profile() {
        let meta = make_meta(None, None, Some("flat-model"), Some("https://flat.example.com"));
        let mut app = make_app(meta);
        app.on_session_selected(0);
        assert_eq!(app.selected_profile, 0);
        assert_eq!(app.profiles[0].model, "flat-model");
        assert_eq!(app.profiles[0].api_base_url, "https://flat.example.com");
    }

    #[test]
    fn session_click_invalid_slot_port_does_not_match() {
        let meta = make_meta(Some("root@10.0.0.5:22"), None, None, None);
        let mut app = make_app(meta);
        app.ssh_slots[0].port = "22abc".into();
        app.on_session_selected(0);
        assert_eq!(app.target_mode, 0, "invalid non-empty port must not match 22");
    }

    #[test]
    fn session_click_slot_port_with_spaces_matches() {
        let meta = make_meta(Some("root@10.0.0.5:22"), None, None, None);
        let mut app = make_app(meta);
        app.ssh_slots[0].port = " 22 ".into();
        app.on_session_selected(0);
        assert_eq!(app.target_mode, 1, "whitespace around port must be trimmed");
    }

    // ── SSH host list (#411) ─────────────────────────────────────────

    fn ssh_profile(host: &str, alias: &str) -> SshProfile {
        SshProfile {
            host: host.into(),
            port: "22".into(),
            user: "root".into(),
            alias: alias.into(),
            ..SshProfile::default()
        }
    }

    #[test]
    fn migration_gives_unnamed_slots_their_positional_aliases() {
        // The pre-#411 layout: five slots, all nameless. Each display name
        // must become a real alias unchanged — the keyring keys
        // (`ssh_target:SSH3`) rely on it, and an orphaned key silently
        // drops the saved password (#411).
        let mut profiles: Vec<SshProfile> = (1..=5)
            .map(|i| ssh_profile(&format!("10.0.0.1{i}"), ""))
            .collect();
        assert!(migrate_ssh_profiles(&mut profiles));
        assert_eq!(profiles.len(), 5);
        let aliases: Vec<&str> = profiles.iter().map(|p| p.alias.as_str()).collect();
        assert_eq!(aliases, ["SSH1", "SSH2", "SSH3", "SSH4", "SSH5"]);
        for (i, p) in profiles.iter().enumerate() {
            assert_eq!(
                ssh_cred_name(i, &p.alias),
                format!("ssh_target:SSH{}", i + 1),
                "slot {i} keyring key must not change"
            );
        }
    }

    #[test]
    fn migration_drops_blank_slots_and_keeps_positions_of_the_rest() {
        // The old layout padded to exactly five; blank entries were
        // placeholders, not hosts. Alias assignment must happen before the
        // drop, or the survivors would be renumbered.
        let mut profiles = vec![
            ssh_profile("10.0.0.11", ""),
            SshProfile::default(),
            ssh_profile("10.0.0.13", ""),
            ssh_profile("10.0.0.14", "prod-web"),
            SshProfile::default(),
        ];
        assert!(migrate_ssh_profiles(&mut profiles));
        assert_eq!(profiles.len(), 3);
        assert_eq!(profiles[0].alias, "SSH1");
        assert_eq!(profiles[1].alias, "SSH3", "the third slot keeps its name");
        assert_eq!(profiles[2].alias, "prod-web", "an explicit alias stays");
    }

    #[test]
    fn migration_renames_a_collision_and_the_second_run_is_a_no_op() {
        let mut profiles = vec![
            ssh_profile("10.0.0.11", ""),
            ssh_profile("10.0.0.12", "SSH1"),
        ];
        assert!(migrate_ssh_profiles(&mut profiles));
        assert_eq!(profiles[0].alias, "SSH1");
        assert_eq!(profiles[1].alias, "SSH1_dup", "a duplicate hides a host from Ctrl+O");
        assert!(!migrate_ssh_profiles(&mut profiles), "second run must change nothing");
    }

    #[test]
    fn the_version_marker_stops_a_second_migration() {
        let mut settings = Settings {
            ssh_profiles: vec![ssh_profile("10.0.0.11", "")],
            ..Settings::default()
        };
        assert!(settings.apply_ssh_list_migration());
        assert_eq!(settings.ssh_profiles[0].alias, "SSH1");
        assert_eq!(settings.ssh_list_version, SSH_LIST_VERSION);

        // A later load (marker present) must leave a new-format list alone —
        // even a host whose alias is temporarily empty while it is edited:
        // renumbering it would change its keyring key under the user's hands.
        settings.ssh_profiles = vec![ssh_profile("10.0.0.12", "")];
        assert!(!settings.apply_ssh_list_migration());
        assert_eq!(settings.ssh_profiles[0].alias, "");
    }

    // ── Tags (#413) ─────────────────────────────────────────────────

    #[test]
    fn parse_tags_splits_trims_and_drops_empties() {
        assert_eq!(parse_tags("prod, web"), vec!["prod", "web"]);
        assert_eq!(parse_tags(" prod ,web ,, "), vec!["prod", "web"]);
        assert_eq!(parse_tags(""), Vec::<String>::new());
        assert_eq!(parse_tags("  ,  "), Vec::<String>::new());
    }

    #[test]
    fn parse_tags_strips_control_characters() {
        // Escape sequences must not survive into the persisted field: the
        // tags are rendered verbatim in the TUI status bar (#413 review).
        assert_eq!(parse_tags("a\u{1b}b, c\u{7}d"), vec!["ab", "cd"]);
        // A tag that is nothing but control characters (or blanks) drops.
        assert_eq!(parse_tags("\u{1b}\u{7}, \u{1b}"), Vec::<String>::new());
    }

    #[test]
    fn ssh_profile_tags_survive_json_and_old_files_default_empty() {
        let profile = SshProfile {
            host: "10.0.0.5".into(),
            port: "22".into(),
            user: "root".into(),
            alias: "web-1".into(),
            tags: "prod, web".into(),
            ..SshProfile::default()
        };
        let json = serde_json::to_string(&profile).unwrap();
        assert!(json.contains("\"tags\":\"prod, web\""), "json: {json}");
        let back: SshProfile = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tags, "prod, web");

        // A pre-#413 settings file has no `tags` key — the field defaults.
        let old = r#"{"host":"10.0.0.11","port":"22","user":"root","alias":"SSH1"}"#;
        let back: SshProfile = serde_json::from_str(old).unwrap();
        assert!(back.tags.is_empty());
    }

    #[test]
    fn slot_round_trip_preserves_tags() {
        let mut slot = SshSlot::blank();
        slot.host = "10.0.0.5".into();
        slot.alias = "web-1".into();
        slot.tags = "prod, web".into();
        let profile = slot.to_profile();
        assert_eq!(profile.tags, "prod, web");
        let back = SshSlot::from_profile(&profile, 0);
        assert_eq!(back.tags, "prod, web");
    }

    fn app_with_hosts(hosts: &[(&str, &str)]) -> LauncherApp {
        let mut app = make_app(make_meta(None, None, None, None));
        app.ssh_slots = hosts
            .iter()
            // Password login, the method every pre-#489 host had: the
            // credential tests below are about the saved password.
            .map(|(host, alias)| SshSlot {
                host: (*host).into(),
                user: "root".into(),
                alias: (*alias).into(),
                auth: SlotAuth::Password,
                ..SshSlot::blank()
            })
            .collect();
        app.target_mode = 0;
        app
    }

    #[test]
    fn add_host_appends_a_blank_row_and_selects_it() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.add_host();
        assert_eq!(app.ssh_slots.len(), 2);
        assert_eq!(app.target_mode, 2, "the new row becomes the edited target");
        assert!(app.ssh_slots[1].host.is_empty());
        assert_eq!(app.ssh_slots[1].port, "22", "a fresh row defaults to port 22");
    }

    #[test]
    fn move_host_up_keeps_the_selection_on_the_moved_host() {
        let mut app = app_with_hosts(&[("a", "one"), ("b", "two"), ("c", "three")]);
        app.target_mode = 2; // "two"
        app.move_host_up(1);
        let aliases: Vec<&str> = app.ssh_slots.iter().map(|s| s.alias.as_str()).collect();
        assert_eq!(aliases, ["two", "one", "three"]);
        assert_eq!(app.target_mode, 1, "the selection must follow the host");
    }

    #[test]
    fn move_host_adjusts_the_selection_of_a_passed_over_row() {
        let mut app = app_with_hosts(&[("a", "one"), ("b", "two"), ("c", "three")]);
        app.target_mode = 3; // "three"
        app.move_host_down(1);
        let aliases: Vec<&str> = app.ssh_slots.iter().map(|s| s.alias.as_str()).collect();
        assert_eq!(aliases, ["one", "three", "two"]);
        assert_eq!(app.target_mode, 2, "the passed-over row moved one position up");
    }

    #[test]
    fn removing_the_selected_host_falls_back_to_local() {
        let mut app = app_with_hosts(&[("a", "one"), ("b", "two")]);
        app.target_mode = 2;
        app.remove_host(1);
        assert_eq!(app.ssh_slots.len(), 1);
        assert_eq!(app.target_mode, 0, "never silently switch to another host");
    }

    #[test]
    fn removing_an_earlier_host_shifts_the_selection() {
        let mut app = app_with_hosts(&[("a", "one"), ("b", "two"), ("c", "three")]);
        app.target_mode = 3;
        app.remove_host(0);
        let aliases: Vec<&str> = app.ssh_slots.iter().map(|s| s.alias.as_str()).collect();
        assert_eq!(aliases, ["two", "three"]);
        assert_eq!(app.target_mode, 2, "the selection still points at 'three'");
    }

    #[test]
    fn launch_refuses_a_host_without_an_alias() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1"), ("10.0.0.12", "")]);
        assert!(!app.validate_launch());
        assert!(
            app.validation_error.contains("needs an alias"),
            "got {:?}",
            app.validation_error
        );
    }

    #[test]
    fn launch_refuses_duplicate_host_aliases() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web"), ("10.0.0.12", "web")]);
        assert!(!app.validate_launch());
        assert!(
            app.validation_error.contains("Duplicate SSH host alias"),
            "got {:?}",
            app.validation_error
        );
    }

    #[test]
    fn launch_refuses_aliases_that_only_differ_after_the_stored_cutoff() {
        // `to_profile` stores at most 32 chars; two longer aliases sharing the
        // first 32 would persist as one keyring key, so they collide now.
        let long_a = format!("{}a", "x".repeat(32));
        let long_b = format!("{}b", "x".repeat(32));
        let mut app = app_with_hosts(&[("10.0.0.11", &long_a), ("10.0.0.12", &long_b)]);
        assert!(!app.validate_launch());
        assert!(
            app.validation_error.contains("Duplicate SSH host alias"),
            "got {:?}",
            app.validation_error
        );
    }

    #[test]
    fn a_blank_scratch_row_only_blocks_its_own_selection() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.add_host(); // blank row, selected
        assert!(!app.validate_launch());
        assert!(
            app.validation_error.contains("Host is required"),
            "got {:?}",
            app.validation_error
        );
        app.target_mode = 1; // deselect the blank row
        assert!(
            app.validate_launch(),
            "a blank scratch row must not block launching another target: {:?}",
            app.validation_error
        );
    }

    #[test]
    fn launch_accepts_a_full_host_list() {
        let hosts: Vec<(String, String)> = (1..=12)
            .map(|i| (format!("10.0.0.{i}"), format!("web-{i}")))
            .collect();
        let refs: Vec<(&str, &str)> = hosts
            .iter()
            .map(|(h, a)| (h.as_str(), a.as_str()))
            .collect();
        let mut app = app_with_hosts(&refs);
        assert!(app.validate_launch(), "got {:?}", app.validation_error);
        app.target_mode = 12;
        assert!(app.validate_launch(), "the last host must launch too");
    }

    #[test]
    fn blank_rows_are_not_persisted() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.add_host();
        let persisted = app.persistable_ssh_profiles();
        assert_eq!(persisted.len(), 1, "the scratch row must not reach settings.json");
        assert_eq!(persisted[0].alias, "web-1");
    }

    #[test]
    fn a_blank_scratch_row_never_touches_the_keyring() {
        // A migrated host can hold the positional alias `SSH2` at any index;
        // a blank row's empty alias would resolve to `ssh_target:SSH{i+1}`
        // and delete that host's password (#411).
        let mut app = app_with_hosts(&[("10.0.0.11", "SSH2")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        app.add_host(); // blank scratch row at index 1

        let ops = app.ssh_credential_ops();
        assert_eq!(ops.len(), 1, "only the persisted host may touch the keyring");
        assert_eq!(ops[0].0, "ssh_target:SSH2");
        assert_eq!(ops[0].1.as_deref(), Some("s3cret"));

        // Without the save flag the same host's key is cleared — still the
        // host's key, never the scratch row's index-derived one.
        app.ssh_slots[0].save_password = false;
        let ops = app.ssh_credential_ops();
        assert_eq!(ops, vec![("ssh_target:SSH2".to_string(), None)]);
    }

    #[test]
    fn a_password_is_saved_only_for_password_login() {
        // A host switched to key or agent login must not keep its old
        // password in the keyring (#489).
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        app.ssh_slots[0].auth = SlotAuth::Key;
        assert_eq!(app.ssh_credential_ops(), vec![("ssh_target:web-1".to_string(), None)]);
    }

    #[test]
    fn key_passphrase_ops_save_only_a_checked_key_passphrase() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.add_host(); // blank scratch row: never touches the keyring
        let slot = &mut app.ssh_slots[0];
        slot.auth = SlotAuth::Key;
        slot.passphrase = "correct horse".into();
        slot.save_passphrase = true;
        assert_eq!(
            app.key_passphrase_ops(),
            vec![("ssh_key_passphrase:web-1".to_string(), Some("correct horse".to_string()))]
        );
        // Unchecked, or another login method: the entry is cleared.
        app.ssh_slots[0].save_passphrase = false;
        assert_eq!(app.key_passphrase_ops(), vec![("ssh_key_passphrase:web-1".to_string(), None)]);
        app.ssh_slots[0].save_passphrase = true;
        app.ssh_slots[0].auth = SlotAuth::Agent;
        assert_eq!(app.key_passphrase_ops(), vec![("ssh_key_passphrase:web-1".to_string(), None)]);
    }

    #[test]
    fn the_passphrase_entry_is_the_one_the_session_reads() {
        // The session looks up `ssh_key_passphrase:<target name>`, and the
        // target name of a launcher host is its alias (#489).
        assert_eq!(
            slot_passphrase_cred_name(3, "prod-web"),
            filar_core::ssh_key_passphrase_name(&filar_core::ssh_target_display_name(3, "prod-web"))
        );
        assert_eq!(slot_passphrase_cred_name(3, "prod-web"), "ssh_key_passphrase:prod-web");
    }

    #[test]
    fn slot_debug_redacts_the_password_and_the_passphrase() {
        let mut slot = SshSlot::blank();
        slot.password = "s3cret".into();
        slot.passphrase = "correct horse".into();
        let debug = format!("{slot:?}");
        assert!(!debug.contains("s3cret") && !debug.contains("correct horse"), "{debug}");
    }

    #[test]
    fn profile_round_trip_keeps_the_login_method_and_never_a_passphrase() {
        let mut slot = SshSlot::blank();
        slot.host = "10.0.0.5".into();
        slot.alias = "web-1".into();
        slot.auth = SlotAuth::Key;
        slot.key_path = " /home/ops/.ssh/deploy ".into();
        slot.passphrase = "correct horse".into();
        slot.save_passphrase = false; // not saved → no keyring read on load
        let profile = slot.to_profile();
        let json = serde_json::to_string(&profile).unwrap();
        assert!(!json.contains("correct horse"), "{json}");
        assert!(json.contains("\"auth\":\"key\""), "{json}");
        let back = SshSlot::from_profile(&serde_json::from_str(&json).unwrap(), 0);
        assert_eq!(back.auth, SlotAuth::Key);
        assert_eq!(back.key_path, "/home/ops/.ssh/deploy");
        assert!(back.passphrase.is_empty());
    }

    #[test]
    fn a_pre_489_profile_migrates_to_password_login() {
        // Old settings files have no `auth`: Launch always built password
        // auth for them, so the migration keeps the login working.
        let old = r#"{"host":"10.0.0.11","port":"22","user":"root","alias":"web-1","save_password":false}"#;
        let profile: SshProfile = serde_json::from_str(old).unwrap();
        assert_eq!(profile.auth, None);
        let slot = SshSlot::from_profile(&profile, 0);
        assert_eq!(slot.auth, SlotAuth::Password);
        assert_eq!(slot.to_profile().auth, Some(SlotAuth::Password));
        assert_eq!(SshSlot::blank().auth, SlotAuth::Agent, "new hosts use the agent");
    }

    const KEY_PASSPHRASE: &str = "correct horse";

    /// A temp key file, removed when dropped — even when the test panics.
    struct TempKey(std::path::PathBuf);

    impl Drop for TempKey {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A throwaway ed25519 key, encrypted when `encrypted`. Deterministic
    /// seed, one bcrypt round; named by a counter so parallel tests never
    /// share a file.
    fn temp_key(encrypted: bool) -> TempKey {
        use russh::keys::ssh_key::private::Ed25519Keypair;
        use russh::keys::ssh_key::{Cipher, Kdf, LineEnding, PrivateKey};
        use std::sync::atomic::{AtomicUsize, Ordering};
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let key = PrivateKey::from(Ed25519Keypair::from_seed(&[4u8; 32]));
        let key = if encrypted {
            key.encrypt_with(
                Cipher::Aes256Ctr,
                Kdf::Bcrypt { salt: [6u8; 16].to_vec(), rounds: 1 },
                11,
                KEY_PASSPHRASE,
            )
            .expect("encrypt")
        } else {
            key
        };
        let pem = key.to_openssh(LineEnding::LF).expect("encode");
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("filar-gui-key-{}-{n}", std::process::id()));
        std::fs::write(&path, pem.as_bytes()).expect("write");
        TempKey(path)
    }

    fn app_with_key_host(key: &TempKey) -> LauncherApp {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.target_mode = 1;
        app.ssh_slots[0].auth = SlotAuth::Key;
        app.ssh_slots[0].key_path = key.0.display().to_string();
        app
    }

    #[test]
    fn launch_checks_the_saved_passphrase_of_the_selected_key() {
        let key = temp_key(true);
        let mut app = app_with_key_host(&key);
        app.ssh_slots[0].save_passphrase = true;

        let err = app.validate_selected_key().unwrap_err();
        assert!(err.contains("Enter the passphrase"), "{err}");

        app.ssh_slots[0].passphrase = "battery staple".into();
        let err = app.validate_selected_key().unwrap_err();
        assert!(err.starts_with("Wrong passphrase for SSH key"), "{err}");
        assert!(!err.contains("battery staple"), "the error must not echo it: {err}");

        app.ssh_slots[0].passphrase = KEY_PASSPHRASE.into();
        assert_eq!(app.validate_selected_key(), Ok(()));

        // Not saved: nothing to check here, the terminal asks at launch.
        app.ssh_slots[0].passphrase.clear();
        app.ssh_slots[0].save_passphrase = false;
        assert_eq!(app.validate_selected_key(), Ok(()));
    }

    #[test]
    fn launch_accepts_a_plain_key_and_refuses_a_missing_one() {
        let key = temp_key(false);
        let mut app = app_with_key_host(&key);
        app.ssh_slots[0].save_passphrase = true; // irrelevant for a plain key
        assert_eq!(app.validate_selected_key(), Ok(()));
        assert_eq!(
            app.refresh_key_probe(0),
            Some(Ok(filar_transport::KeyProtection::Plain))
        );

        let missing = key.0.with_extension("missing");
        app.ssh_slots[0].key_path = missing.display().to_string();
        let err = app.validate_selected_key().unwrap_err();
        assert!(err.contains("failed to load SSH key"), "{err}");
        assert!(matches!(app.refresh_key_probe(0), Some(Err(_))));

        // Another login method, or Local: no key to check.
        app.ssh_slots[0].auth = SlotAuth::Agent;
        assert_eq!(app.validate_selected_key(), Ok(()));
        assert_eq!(app.refresh_key_probe(0), None);
    }

    #[test]
    fn the_key_probe_sees_an_encrypted_key() {
        let key = temp_key(true);
        let mut app = app_with_key_host(&key);
        assert_eq!(
            app.refresh_key_probe(0),
            Some(Ok(filar_transport::KeyProtection::Encrypted))
        );
    }

    #[test]
    fn last_ssh_is_one_plus_the_persisted_position() {
        // Blank scratch rows are dropped from the saved list (#411), so a
        // raw slot index would preselect a different host after a restart.
        // The 1-based encoding keeps Local (0) apart from the first
        // persisted host (1).
        let mut app =
            app_with_hosts(&[("", ""), ("10.0.0.11", "web-1"), ("10.0.0.12", "web-2")]);
        assert_eq!(app.persisted_last_ssh(), 0, "Local remembers no host");
        app.target_mode = 1; // the blank scratch row itself
        assert_eq!(app.persisted_last_ssh(), 0, "a blank row has no persisted counterpart");
        app.target_mode = 2; // web-1: first persisted position
        assert_eq!(app.persisted_last_ssh(), 1);
        app.target_mode = 3; // web-2: second persisted position
        assert_eq!(app.persisted_last_ssh(), 2);
    }

    #[test]
    fn the_selected_host_survives_a_save_load_round_trip_past_a_blank_row() {
        // Both hosts sit behind a blank row; the first one must not
        // degrade to Local and the second must not shift onto the first.
        for (slot, expected_mode, alias) in [(2usize, 1usize, "web-1"), (3, 2, "web-2")] {
            let mut app =
                app_with_hosts(&[("", ""), ("10.0.0.11", "web-1"), ("10.0.0.12", "web-2")]);
            app.target_mode = slot;
            let persisted = app.persistable_ssh_profiles();
            let last_ssh = app.persisted_last_ssh();

            // The restore side of `run_launcher`, against the saved list.
            let pairs: Vec<(&str, &str)> = persisted
                .iter()
                .map(|p| (p.host.as_str(), p.alias.as_str()))
                .collect();
            let restored = app_with_hosts(&pairs);
            let target_mode = if last_ssh > 0 && last_ssh <= restored.ssh_slots.len() {
                last_ssh
            } else {
                0
            };
            assert_eq!(target_mode, expected_mode, "mode for {alias}");
            assert_eq!(
                restored.ssh_slots[target_mode - 1].alias,
                alias,
                "the same host as before the restart"
            );
        }
    }

    #[test]
    fn migration_re_encodes_last_ssh_from_a_slot_index_to_a_persisted_position() {
        // Pre-#411 `last_ssh` was the raw slot index in the padded list and
        // shared 0 with Local. [A, B, C, blank, blank] with C selected
        // (slot 2) must become position 3: two hosts precede C.
        let mut settings = Settings {
            ssh_profiles: vec![
                ssh_profile("10.0.0.11", ""),
                ssh_profile("10.0.0.12", ""),
                ssh_profile("10.0.0.13", ""),
                SshProfile::default(),
                SshProfile::default(),
            ],
            last_ssh: 2,
            ..Settings::default()
        };
        assert!(settings.apply_ssh_list_migration());
        assert_eq!(settings.last_ssh, 3);

        // A selected placeholder slot has no host to restore: Local.
        let mut settings = Settings {
            ssh_profiles: vec![ssh_profile("10.0.0.11", ""), SshProfile::default()],
            last_ssh: 1,
            ..Settings::default()
        };
        assert!(settings.apply_ssh_list_migration());
        assert_eq!(settings.last_ssh, 0);
    }

    // ── Host list import (#416) ─────────────────────────────────────

    /// A TOML host list with `count` `[[ssh_targets]]` entries.
    fn import_toml_text(count: usize) -> String {
        let mut text = String::new();
        for i in 1..=count {
            text.push_str(&format!(
                "[[ssh_targets]]\nname = \"fleet-{i:02}\"\nhost = \"192.0.2.{i}\"\nport = 2222\nuser = \"admin\"\ntags = [\"prod\", \"fleet\"]\n\n"
            ));
        }
        text
    }

    #[test]
    fn import_toml_reads_twenty_hosts_in_one_file() {
        let rows = parse_import_toml(&import_toml_text(20)).unwrap();
        assert_eq!(rows.len(), 20);
        assert_eq!(rows[0].slot.alias, "fleet-01");
        assert_eq!(rows[0].slot.host, "192.0.2.1");
        assert_eq!(rows[0].slot.port, "2222");
        assert_eq!(rows[0].slot.user, "admin");
        assert_eq!(rows[0].slot.tags, "prod, fleet");
        assert_eq!(rows[19].slot.alias, "fleet-20");
        assert_eq!(rows[19].slot.host, "192.0.2.20");
        // Secrets are never part of an import.
        assert!(rows.iter().all(|r| r.slot.password.is_empty() && !r.slot.save_password));
        // No auth table in the file — the rows carry no auth verdict (#417).
        assert!(rows.iter().all(|r| r.auth.is_none()));
    }

    #[test]
    fn import_toml_ignores_other_config_sections_and_keys() {
        let text = "[llm]\nmodel = \"glm-5.1\"\n\n\
                    [[ssh_targets]]
name = \"web-1\"
host = \"192.0.2.10\"
user = \"root\"
auth = { type = \"password\" }
host_key_policy = \"strict\"
";
        let rows = parse_import_toml(text).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].slot.port, "22", "an omitted port defaults to 22");
        assert_eq!(rows[0].slot.tags, "");
        assert_eq!(
            rows[0].auth.as_ref().map(|a| a.kind),
            Some(SlotAuth::Password),
            "the auth type is read (#417)"
        );
        assert!(
            rows[0].slot.auth == SlotAuth::Password
                && !rows[0].slot.save_password
                && rows[0].slot.password.is_empty(),
            "password auth means the connect prompt, never a stored secret"
        );
    }

    #[test]
    fn import_toml_rejects_a_file_without_targets() {
        let err = parse_import_toml("[llm]\nmodel = \"m\"\n").unwrap_err();
        assert!(err.contains("no [[ssh_targets]]"), "got {err}");
        let err = parse_import_toml("this is not toml {{{").unwrap_err();
        assert!(err.contains("not valid TOML"), "got {err}");
    }

    #[test]
    fn import_auth_must_not_take_the_inline_password() {
        // The file may carry a password (config.toml shape); it must be
        // dropped unread — secrets stay in the keyring (invariant #3).
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"192.0.2.1\"\nauth = { type = \"password\", password = \"hunter2\" }\n",
        )
        .unwrap();
        assert_eq!(rows[0].auth.as_ref().map(|a| a.kind), Some(SlotAuth::Password));
        assert!(
            rows[0].slot.password.is_empty(),
            "an inline secret must not be read"
        );
    }

    #[test]
    fn import_unknown_auth_type_is_rejected() {
        let err = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"192.0.2.1\"\nauth = { type = \"knock\" }\n",
        )
        .unwrap_err();
        assert!(err.contains("unknown auth type \"knock\""), "got {err}");
    }

    #[test]
    fn import_auth_key_keeps_the_key_path_and_export_writes_it_back() {
        // A key file path is a location, not a secret: the host keeps it,
        // and the export carries it so the round trip loses nothing (#489).
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"192.0.2.1\"\nauth = { type = \"key\", path = \" /root/.ssh/id_rsa \" }\n",
        )
        .unwrap();
        assert_eq!(
            rows[0].auth,
            Some(ImportAuth { kind: SlotAuth::Key, key_path: "/root/.ssh/id_rsa".into() })
        );
        assert_eq!(rows[0].slot.auth, SlotAuth::Key);
        assert_eq!(rows[0].slot.key_path, "/root/.ssh/id_rsa");
        let refs: Vec<&SshSlot> = rows.iter().map(|r| &r.slot).collect();
        let text = export_hosts_toml(&refs).unwrap();
        assert!(text.contains("type = \"key\""), "got {text}");
        assert!(text.contains("path = \"/root/.ssh/id_rsa\""), "got {text}");
    }

    #[test]
    fn import_agent_and_a_missing_auth_mean_agent_login() {
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"a\"\nhost = \"192.0.2.1\"\nauth = { type = \"agent\", path = \"/ignored\" }\n\n\
             [[ssh_targets]]\nname = \"b\"\nhost = \"192.0.2.2\"\n",
        )
        .unwrap();
        assert_eq!(rows[0].slot.auth, SlotAuth::Agent);
        assert!(rows[0].slot.key_path.is_empty(), "a path only belongs to key login");
        assert_eq!(rows[1].slot.auth, SlotAuth::Agent, "new hosts default to the agent");
        assert!(rows[1].auth.is_none());
    }

    #[test]
    fn import_rejects_invalid_or_duplicate_rows() {
        let err =
            parse_import_toml("[[ssh_targets]]\nname = \"\"\nhost = \"192.0.2.1\"\n").unwrap_err();
        assert!(err.contains("name must not be empty"), "got {err}");

        let err =
            parse_import_toml("[[ssh_targets]]\nname = \"web\"\nhost = \"\"\n").unwrap_err();
        assert!(err.contains("host must not be empty"), "got {err}");

        let err = parse_import_toml("[[ssh_targets]]\nname = \"web\"\nhost = \"h\"\nport = 0\n")
            .unwrap_err();
        assert!(err.contains("invalid port 0"), "got {err}");

        let duplicate = "[[ssh_targets]]\nname = \"web\"\nhost = \"192.0.2.1\"\n\n\
                         [[ssh_targets]]\nname = \"web\"\nhost = \"192.0.2.2\"\n";
        let err = parse_import_toml(duplicate).unwrap_err();
        assert!(err.contains("duplicate host name"), "got {err}");
    }

    #[test]
    fn csv_splitting_handles_quotes_and_crlf() {
        let rows = split_csv_rows("a,\"b\"\"c\",d\r\ne,f,g\r\n").unwrap();
        assert_eq!(
            rows,
            vec![
                vec!["a".to_string(), "b\"c".to_string(), "d".to_string()],
                vec!["e".to_string(), "f".to_string(), "g".to_string()],
            ]
        );
    }

    #[test]
    fn import_csv_reads_shuffled_case_insensitive_columns_and_quotes() {
        let csv = "Host,NAME,Port,User,Tags\n\
                   192.0.2.1,\"web,1\",2222,admin,prod;web\n\
                   192.0.2.2,db-1,,,\n";
        let rows = parse_import_csv(csv).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].slot.alias, "web,1");
        assert_eq!(rows[0].slot.host, "192.0.2.1");
        assert_eq!(rows[0].slot.port, "2222");
        assert_eq!(rows[0].slot.user, "admin");
        assert_eq!(rows[0].slot.tags, "prod, web");
        assert_eq!(rows[1].slot.alias, "db-1");
        assert_eq!(rows[1].slot.port, "22", "an empty port defaults to 22");
        assert_eq!(rows[1].slot.tags, "");
        // CSV has no auth column: rows default to key auth (#417).
        assert!(rows.iter().all(|r| r.auth.is_none() && !r.slot.save_password));
    }

    #[test]
    fn import_csv_rejects_bad_input_with_the_line_number() {
        let err = parse_import_csv("host,port\n192.0.2.1,22\n").unwrap_err();
        assert!(err.contains("\"name\" column"), "got {err}");

        let err = parse_import_csv("name,host,port\nweb-1,192.0.2.1,abc\n").unwrap_err();
        assert!(err.contains("line 2"), "got {err}");
        assert!(err.contains("invalid port"), "got {err}");

        let err = parse_import_csv("name,host\n\"web-1,192.0.2.1\n").unwrap_err();
        assert!(err.contains("unterminated quoted field"), "got {err}");

        let err = parse_import_csv("").unwrap_err();
        assert!(err.contains("empty CSV"), "got {err}");
    }

    #[test]
    fn import_csv_blank_lines_keep_the_file_line_number() {
        // The empty rows must not shift the number in the error message
        // (#416 review): "web-2" sits on file line 5.
        let csv = "name,host,port\n\nweb-1,192.0.2.1,22\n\nweb-2,192.0.2.2,notaport\n";
        let err = parse_import_csv(csv).unwrap_err();
        assert!(err.contains("line 5"), "got {err}");
        assert!(err.contains("invalid port"), "got {err}");
    }

    #[test]
    fn import_with_no_collisions_appends_all_rows() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        let rows =
            parse_import_toml("[[ssh_targets]]\nname = \"db-1\"\nhost = \"10.0.0.12\"\n")
                .unwrap();
        assert_eq!(
            find_import_collisions(&app.ssh_slots, &rows),
            Vec::<String>::new()
        );
        let outcome = apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Skip);
        assert_eq!(outcome.added, 1);
        assert_eq!(app.ssh_slots.len(), 2);
        assert_eq!(app.ssh_slots[1].alias, "db-1");
    }

    #[test]
    fn import_skip_keeps_the_existing_host() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        let rows =
            parse_import_toml("[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.99\"\n")
                .unwrap();
        let outcome = apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Skip);
        assert_eq!(
            outcome,
            ImportOutcome { added: 0, overwritten: 0, renamed: 0, skipped: 1, kube_skipped: 0 }
        );
        assert_eq!(app.ssh_slots.len(), 1);
        assert_eq!(app.ssh_slots[0].host, "10.0.0.11", "the existing host survives");
    }

    #[test]
    fn import_overwrite_keeps_the_password_when_the_identity_is_unchanged() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1"), ("10.0.0.12", "db-1")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.11\"\nport = 22\nuser = \"root\"\ntags = [\"prod\"]\n",
        )
        .unwrap();
        let outcome = apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Overwrite);
        assert_eq!(outcome.overwritten, 1);
        assert_eq!(app.ssh_slots.len(), 2, "no new row appears");
        let slot = &app.ssh_slots[0];
        assert_eq!(slot.tags, "prod", "tags are still overwritten");
        assert!(
            slot.save_password && slot.password == "s3cret",
            "same host/port/user: the credential still belongs here"
        );
    }

    #[test]
    fn import_overwrite_clears_the_password_when_the_identity_changes() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1"), ("10.0.0.12", "db-1")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.99\"\nport = 2222\nuser = \"admin\"\ntags = [\"prod\"]\n",
        )
        .unwrap();
        let outcome = apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Overwrite);
        assert_eq!(outcome.overwritten, 1);
        assert_eq!(app.ssh_slots.len(), 2, "no new row appears");
        let slot = &app.ssh_slots[0];
        assert_eq!(
            (slot.host.as_str(), slot.port.as_str(), slot.user.as_str()),
            ("10.0.0.99", "2222", "admin")
        );
        assert_eq!(slot.tags, "prod");
        assert!(
            !slot.save_password && slot.password.is_empty(),
            "a credential must not follow the alias to another machine"
        );
    }

    #[test]
    fn import_auth_key_switches_an_overwritten_host_to_key_auth() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.11\"\nport = 22\nuser = \"root\"\nauth = { type = \"key\" }\n",
        )
        .unwrap();
        let outcome = apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Overwrite);
        assert_eq!(outcome.overwritten, 1);
        let slot = &app.ssh_slots[0];
        assert_eq!(slot.auth, SlotAuth::Key);
        assert!(
            !slot.save_password && slot.password.is_empty(),
            "the file says key auth: the stored password must not linger"
        );
    }

    #[test]
    fn import_overwrite_drops_the_passphrase_of_another_key() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        let slot = &mut app.ssh_slots[0];
        slot.auth = SlotAuth::Key;
        slot.key_path = "/keys/old".into();
        slot.save_passphrase = true;
        slot.passphrase = "correct horse".into();
        // Same key file: the passphrase still belongs to it.
        let same = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.11\"\nuser = \"root\"\nauth = { type = \"key\", path = \"/keys/old\" }\n",
        )
        .unwrap();
        apply_import(&mut app.ssh_slots, same, CollisionPolicy::Overwrite);
        assert!(app.ssh_slots[0].save_passphrase && app.ssh_slots[0].passphrase == "correct horse");
        // Another key file: the old passphrase must not follow it.
        let other = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.11\"\nuser = \"root\"\nauth = { type = \"key\", path = \"/keys/new\" }\n",
        )
        .unwrap();
        apply_import(&mut app.ssh_slots, other, CollisionPolicy::Overwrite);
        let slot = &app.ssh_slots[0];
        assert_eq!(slot.key_path, "/keys/new");
        assert!(!slot.save_passphrase && slot.passphrase.is_empty());
    }

    #[test]
    fn import_auth_password_after_an_identity_change_keeps_the_prompt() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.99\"\nport = 2222\nuser = \"admin\"\nauth = { type = \"password\" }\n",
        )
        .unwrap();
        apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Overwrite);
        let slot = &app.ssh_slots[0];
        assert!(
            slot.auth == SlotAuth::Password && !slot.save_password && slot.password.is_empty(),
            "the new machine keeps the prompt but not the old machine's secret"
        );
    }

    #[test]
    fn import_alias_strips_control_characters() {
        // The TOML escape decodes to a raw BEL (U+0007) inside the name.
        let rows = parse_import_toml(
            "[[ssh_targets]]\nname = \"web\\u0007-1\"\nhost = \"10.0.0.9\"\n",
        )
        .unwrap();
        assert_eq!(
            rows[0].slot.alias, "web-1",
            "a control character must not reach the TUI status bar"
        );
    }

    #[test]
    fn ssh_slot_debug_redacts_the_password() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        let rendered = format!("{:?}", app.ssh_slots[0]);
        assert!(rendered.contains("<redacted>"), "got {rendered}");
        assert!(!rendered.contains("s3cret"), "got {rendered}");
    }

    #[test]
    fn import_rename_uses_the_first_free_suffix() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1"), ("10.0.0.12", "web-1-2")]);
        let rows =
            parse_import_toml("[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.99\"\n")
                .unwrap();
        let outcome = apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Rename);
        assert_eq!(outcome.renamed, 1);
        assert_eq!(app.ssh_slots.len(), 3);
        assert_eq!(app.ssh_slots[2].alias, "web-1-3", "web-1-2 was taken");
        assert_eq!(app.ssh_slots[2].host, "10.0.0.99");
    }

    #[test]
    fn import_collision_check_ignores_blank_scratch_rows() {
        let mut app = app_with_hosts(&[("", "web-1")]);
        let rows =
            parse_import_toml("[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.99\"\n")
                .unwrap();
        assert!(
            find_import_collisions(&app.ssh_slots, &rows).is_empty(),
            "blank rows are not saved hosts"
        );
        let outcome = apply_import(&mut app.ssh_slots, rows, CollisionPolicy::Skip);
        assert_eq!(outcome.added, 1);
    }

    #[test]
    fn a_broken_import_file_leaves_the_host_list_untouched() {
        let dir = std::env::temp_dir().join(format!("filar_test_import_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bad = dir.join("broken.toml");
        std::fs::write(&bad, "this is not toml {{{").unwrap();

        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        let err = stage_import(&mut app.ssh_slots, &bad).unwrap_err();
        assert!(err.contains("not valid TOML"), "got {err}");
        assert_eq!(app.ssh_slots.len(), 1, "a broken file must not touch the host list");
        assert_eq!(app.ssh_slots[0].alias, "web-1");

        // A file that cannot be read is an error, not a silent no-op.
        let err = stage_import(&mut app.ssh_slots, &dir.join("missing.csv")).unwrap_err();
        assert!(err.contains("cannot read"), "got {err}");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn staging_a_colliding_import_defers_to_the_user() {
        let dir = std::env::temp_dir().join(format!("filar_test_import2_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("hosts.toml");
        std::fs::write(&path, "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.99\"\n")
            .unwrap();

        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        match stage_import(&mut app.ssh_slots, &path).unwrap() {
            ImportStaged::NeedsDecision { rows, collisions, .. } => {
                assert_eq!(collisions, vec!["web-1".to_string()]);
                assert_eq!(rows.len(), 1);
            }
            ImportStaged::Applied(_) => panic!("a collision must not be applied silently"),
        }
        assert_eq!(app.ssh_slots.len(), 1, "nothing is applied before the choice");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_file_format_follows_the_extension() {
        let dir = std::env::temp_dir().join(format!("filar_test_import3_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        let csv = dir.join("hosts.csv");
        std::fs::write(&csv, "name,host\nweb-1,10.0.0.11\n").unwrap();
        let parsed = parse_import_file(&csv).unwrap();
        assert_eq!(parsed.rows[0].slot.alias, "web-1");

        // TOML content under a .csv name fails as CSV (explicit, not guessed).
        let lying = dir.join("lying.csv");
        std::fs::write(&lying, "[[ssh_targets]]\nname = \"w\"\nhost = \"h\"\n").unwrap();
        assert!(parse_import_file(&lying).is_err());

        let toml = dir.join("hosts.toml");
        std::fs::write(&toml, "[[ssh_targets]]\nname = \"web-1\"\nhost = \"10.0.0.11\"\n")
            .unwrap();
        assert_eq!(parse_import_file(&toml).unwrap().rows.len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn import_status_line_counts_every_decision() {
        let line = import_status_line(&ImportOutcome {
            added: 3,
            overwritten: 1,
            renamed: 2,
            skipped: 4,
            kube_skipped: 0,
        });
        assert_eq!(line, "Import: 3 added, 1 overwritten, 2 renamed, 4 skipped");
    }

    #[test]
    fn import_and_export_explicitly_report_skipped_kubernetes_clusters() {
        let parsed = parse_import_toml_details(
            "[[kube_clusters]]\nname = \"prod-eu\"\ncontext = \"kind-prod\"\n",
        )
        .unwrap();
        assert!(parsed.rows.is_empty());
        assert_eq!(parsed.kube_skipped, 1);
        let outcome = ImportOutcome {
            kube_skipped: parsed.kube_skipped,
            ..ImportOutcome::default()
        };
        assert!(
            import_status_line(&outcome).contains("1 Kubernetes cluster(s) skipped")
        );
        let line = export_status_line(2, 1, std::path::Path::new("hosts.toml"));
        assert!(line.contains("2 hosts"));
        assert!(line.contains("1 Kubernetes cluster(s) skipped"));
    }

    // ── Host list export (#417) ─────────────────────────────────────

    #[test]
    fn export_never_writes_secrets_or_key_paths() {
        let mut app = app_with_hosts(&[("10.0.0.11", "web-1")]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        let refs: Vec<&SshSlot> = app.ssh_slots.iter().collect();
        let text = export_hosts_toml(&refs).unwrap();
        assert!(!text.contains("s3cret"), "got {text}");
        assert!(!text.contains("password = "), "got {text}");
        assert!(!text.contains("path = "), "got {text}");
        // The auth *type* is the only credential-related field in the file.
        assert!(text.contains("type = \"password\""), "got {text}");
    }

    #[test]
    fn export_import_round_trip_restores_the_fleet() {
        let mut app = app_with_hosts(&[
            ("10.0.0.11", "web-1"),
            ("10.0.0.12", "db-1"),
            ("10.0.0.13", "ci-1"),
        ]);
        app.ssh_slots[0].save_password = true;
        app.ssh_slots[0].password = "s3cret".into();
        app.ssh_slots[0].port = "2222".into();
        app.ssh_slots[0].user = "admin".into();
        app.ssh_slots[0].tags = "prod, fleet".into();
        app.ssh_slots[1].auth = SlotAuth::Key;
        app.ssh_slots[1].key_path = "/home/ops/.ssh/deploy".into();
        app.ssh_slots[1].save_passphrase = true;
        app.ssh_slots[1].passphrase = "correct horse".into();
        app.ssh_slots[2].auth = SlotAuth::Agent;
        let refs: Vec<&SshSlot> = app.ssh_slots.iter().collect();
        let text = export_hosts_toml(&refs).unwrap();
        assert!(!text.contains("s3cret") && !text.contains("correct horse"), "got {text}");

        let rows = parse_import_toml(&text).expect("the export must parse as an import");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0].slot.alias, "web-1");
        assert_eq!(rows[0].slot.host, "10.0.0.11");
        assert_eq!(rows[0].slot.port, "2222");
        assert_eq!(rows[0].slot.user, "admin");
        assert_eq!(rows[0].slot.tags, "prod, fleet");
        let kind = |i: usize| rows[i].auth.as_ref().map(|a| a.kind);
        assert_eq!(kind(0), Some(SlotAuth::Password), "password auth survives the trip");
        assert_eq!(rows[1].slot.alias, "db-1");
        assert_eq!(kind(1), Some(SlotAuth::Key), "key auth survives the trip");
        assert_eq!(rows[1].slot.key_path, "/home/ops/.ssh/deploy", "and so does its path");
        assert_eq!(kind(2), Some(SlotAuth::Agent), "agent auth survives the trip");

        // A clean machine gets the very same fleet back.
        let mut clean = app_with_hosts(&[]);
        let outcome = apply_import(&mut clean.ssh_slots, rows, CollisionPolicy::Skip);
        assert_eq!(outcome.added, 3);
        assert_eq!(clean.ssh_slots.len(), 3);
        assert_eq!(clean.ssh_slots[0].host, "10.0.0.11");
        assert_eq!(clean.ssh_slots[0].auth, SlotAuth::Password);
        assert_eq!(clean.ssh_slots[1].auth, SlotAuth::Key);
        assert_eq!(clean.ssh_slots[2].auth, SlotAuth::Agent);
        assert!(
            clean.ssh_slots[0].password.is_empty() && clean.ssh_slots[1].passphrase.is_empty(),
            "the round trip restores the prompt, not the secret"
        );
    }

    #[test]
    fn export_skips_blank_scratch_rows() {
        // start_host_export passes only rows with a host to the serializer —
        // a scratch row must not reach the file (#411).
        let app = app_with_hosts(&[("10.0.0.11", "web-1"), ("", "scratch")]);
        let refs: Vec<&SshSlot> = app
            .ssh_slots
            .iter()
            .filter(|s| !s.host.trim().is_empty())
            .collect();
        let text = export_hosts_toml(&refs).unwrap();
        assert!(!text.contains("scratch"), "got {text}");
        assert!(text.contains("web-1"), "got {text}");
    }

    // ── Host groups (#418) ─────────────────────────────────────

    fn group_draft(name: &str, match_tags: &str) -> HostGroupDraft {
        HostGroupDraft {
            name: name.into(),
            match_tags: match_tags.into(),
            policy: filar_core::HostGroupPolicy::ReadOnly,
            max_parallel: "3".into(),
            per_host_timeout_secs: "30".into(),
            kube: None,
            llm_profile: String::new(),
        }
    }

    #[test]
    fn group_draft_round_trips_a_definition() {
        let group = filar_core::HostGroup {
            name: "Рабочий прод".into(),
            match_tags: vec!["work".into(), "prod".into()],
            policy: filar_core::HostGroupPolicy::ReadOnly,
            max_parallel: 5,
            per_host_timeout_secs: 45,
            kube: None,
            llm_profile: Some("glm".into()),
        };
        let draft = HostGroupDraft::from_group(&group);
        assert_eq!(draft.match_tags, "work, prod");
        assert_eq!(draft.to_group().unwrap(), group);
    }

    #[test]
    fn group_draft_preserves_kubernetes_selection() {
        let group = filar_core::HostGroup {
            name: "web-prod".into(),
            match_tags: Vec::new(),
            policy: filar_core::HostGroupPolicy::ReadOnly,
            max_parallel: 5,
            per_host_timeout_secs: 45,
            kube: Some(filar_core::KubeGroup {
                clusters: vec!["prod-eu".into(), "prod-us".into()],
                namespace: "shop".into(),
                selector: Some("app=web".into()),
            }),
            llm_profile: None,
        };
        assert_eq!(HostGroupDraft::from_group(&group).to_group().unwrap(), group);
    }

    #[test]
    fn group_draft_empty_limits_fall_back_to_defaults() {
        let mut draft = group_draft("prod", "prod");
        draft.max_parallel = String::new();
        draft.per_host_timeout_secs = String::new();
        let group = draft.to_group().unwrap();
        assert_eq!(group.max_parallel, 3);
        assert_eq!(group.per_host_timeout_secs, 30);
        assert_eq!(
            group.llm_profile, None,
            "an empty profile means the session one"
        );
    }

    #[test]
    fn group_draft_refuses_a_malformed_or_zero_limit() {
        for bad in ["0", "abc", "-1"] {
            let mut draft = group_draft("prod", "prod");
            draft.max_parallel = bad.into();
            let err = draft.to_group().unwrap_err();
            assert!(err.contains("max_parallel"), "{bad}: {err}");
            let mut draft = group_draft("prod", "prod");
            draft.per_host_timeout_secs = bad.into();
            let err = draft.to_group().unwrap_err();
            assert!(err.contains("per_host_timeout_secs"), "{bad}: {err}");
        }
        let mut draft = group_draft("prod", "prod");
        draft.name = "   ".into();
        assert!(draft.to_group().is_err(), "a named group may not lose its name");
    }

    #[test]
    fn blank_group_rows_are_not_persisted() {
        let mut app = make_app(make_meta(None, None, None, None));
        app.host_groups = vec![
            group_draft("prod", "prod"),
            // A scratch row: no name, no rule.
            group_draft("", ""),
        ];
        let groups = app.persistable_host_groups().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].name, "prod");
    }

    #[test]
    fn validate_launch_refuses_duplicate_group_names() {
        let mut app = make_app(make_meta(None, None, None, None));
        app.host_groups = vec![group_draft("prod", "prod"), group_draft("prod", "work")];
        assert!(!app.validate_launch());
        assert!(
            app.validation_error.contains("Duplicate group name"),
            "{}",
            app.validation_error
        );
    }

    #[test]
    fn group_preview_follows_tag_edits() {
        let mut app = app_with_hosts(&[("10.0.0.1", "web-1"), ("10.0.0.2", "db-1")]);
        let targets = app.launch_ssh_targets();
        // Nobody carries "prod" yet: the preview says so, weakly.
        assert!(group_preview_line("prod", &targets).contains("0 hosts"));
        // Tag web-1 in the Hosts tab — the preview recalculates.
        app.ssh_slots[0].tags = "work, prod".into();
        let targets = app.launch_ssh_targets();
        let line = group_preview_line("prod", &targets);
        assert!(line.contains("1 host(s)") && line.contains("web-1"), "{line}");
        // db-1 joins once it carries every tag of the rule...
        app.ssh_slots[1].tags = "work, prod".into();
        let targets = app.launch_ssh_targets();
        let line = group_preview_line("work, prod", &targets);
        assert!(
            line.contains("2 host(s)") && line.contains("web-1") && line.contains("db-1"),
            "{line}"
        );
        // ...and leaves again when a tag is removed.
        app.ssh_slots[0].tags = "work".into();
        let targets = app.launch_ssh_targets();
        let line = group_preview_line("work, prod", &targets);
        assert!(line.contains("1 host(s)") && line.contains("db-1"), "{line}");
    }

    #[test]
    fn group_preview_states_an_empty_rule_explicitly() {
        let app = app_with_hosts(&[("10.0.0.1", "web-1")]);
        let targets = app.launch_ssh_targets();
        let line = group_preview_line("", &targets);
        assert!(line.contains("0 hosts") && line.contains("no tags"), "{line}");
    }

    // ── Command safety scorer (#552) ─────────────────────────────────

    fn enabled_draft() -> RiskScorerDraft {
        let mut d = RiskScorerDraft::from_config(None);
        d.enabled = true;
        d.base_url = RISK_DECISIONS_BASE_URL.into();
        d.model = RISK_DECISIONS_MODEL_HINT.into();
        d
    }

    #[test]
    fn risk_draft_off_writes_no_section() {
        let d = RiskScorerDraft::from_config(None);
        assert!(!d.enabled);
        assert_eq!(d.to_config(), Ok(None));
        assert_eq!(d.to_launch(), Ok(RiskScorerLaunch::Off));
        assert_eq!(d.key_to_save(), None);
    }

    #[test]
    fn risk_draft_round_trips_a_config_section() {
        let cfg = filar_core::RiskScorerConfig {
            kind: "chat-logprobs".into(),
            base_url: "http://127.0.0.1:8080/v1".into(),
            model: "qwen".into(),
            key_env: String::new(),
            timeout_secs: 12,
            warn_below: 60,
            danger_below: 30,
        };
        let d = RiskScorerDraft::from_config(Some(&cfg));
        assert!(d.enabled && d.chat_logprobs && d.keyless);
        assert_eq!(d.to_config(), Ok(Some(cfg)), "timeout carried over untouched");
    }

    #[test]
    fn risk_draft_uses_its_own_key_name_and_never_the_key() {
        let mut d = enabled_draft();
        assert_eq!(d.key_env, filar_core::secrets::env_vars::RISK_API_KEY);
        d.key = "sk-or-SECRET\n".into();
        let cfg = d.to_config().unwrap().unwrap();
        assert_eq!(cfg.key_env, "FILAR_RISK_API_KEY");
        assert_eq!(cfg.kind, "decisions");
        // The key is stored in the OS store under key_env, sanitised...
        assert_eq!(d.key_to_save(), Some(("FILAR_RISK_API_KEY".into(), "sk-or-SECRET".into())));
        // ...and appears in neither config.toml nor pending_launch.json.
        let toml_text = toml::to_string_pretty(&filar_core::Config {
            risk_scorer: Some(cfg),
            ..Default::default()
        })
        .unwrap();
        assert!(toml_text.contains("[risk_scorer]") && !toml_text.contains("SECRET"), "{toml_text}");
        let launch = LaunchConfig {
            target: "local".into(),
            ssh: None,
            kube: None,
            kube_clusters: None,
            model: "m".into(),
            api_base_url: "http://x".into(),
            api_key: String::new(),
            session_id: None,
            temperature: String::new(),
            extra_body: String::new(),
            selected_profile: None,
            key_env: String::new(),
            save_dir: None,
            profiles: vec![],
            ssh_targets: vec![],
            arbiter_profile: None,
            host_groups: None,
            risk_scorer: Some(d.to_launch().unwrap()),
            log_sources: None,
        };
        let json = serde_json::to_string(&launch).unwrap();
        assert!(!json.contains("SECRET"), "{json}");
        let back: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.risk_scorer, launch.risk_scorer);
    }

    #[test]
    fn risk_draft_no_key_means_empty_key_env_and_nothing_to_store() {
        let mut d = enabled_draft();
        d.keyless = true;
        d.key = "typed anyway".into();
        assert_eq!(d.to_config().unwrap().unwrap().key_env, "");
        assert_eq!(d.key_to_save(), None);
        // Unticking brings the remembered name back.
        d.set_keyless(true);
        assert!(d.key.is_empty(), "ticking \"no key\" drops the typed key");
        d.set_keyless(false);
        assert_eq!(d.key_checked_for, "FILAR_RISK_API_KEY", "status read on unticking");
        assert_eq!(d.to_config().unwrap().unwrap().key_env, "FILAR_RISK_API_KEY");
    }

    #[test]
    fn risk_draft_validation() {
        let check = |f: fn(&mut RiskScorerDraft), needle: &str| {
            let mut d = enabled_draft();
            f(&mut d);
            let err = d.to_config().unwrap_err();
            assert!(err.contains(needle), "{err}");
        };
        check(|d| d.model.clear(), "model");
        check(|d| d.base_url.clear(), "address");
        check(|d| d.base_url = "openrouter.ai".into(), "http");
        check(|d| d.warn_below = "abc".into(), "warn below");
        check(|d| d.danger_below = "101".into(), "danger below");
        check(|d| { d.warn_below = "30".into(); d.danger_below = "50".into(); }, "danger_below");
        check(|d| d.key_env = "  ".into(), "key name");
    }

    #[test]
    fn a_pending_launch_from_before_552_says_nothing_about_the_scorer() {
        let json = r#"{"target":"local","ssh":null,"model":"m","api_base_url":"u","session_id":null}"#;
        let cfg: LaunchConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.risk_scorer, None);
    }

    #[test]
    fn risk_key_status_belongs_to_the_name_it_was_checked_under() {
        let mut d = enabled_draft();
        d.key_saved = true;
        d.key_checked_for = d.key_env.clone();
        assert!(d.key_saved_now());
        d.key_env = "OTHER_KEY".into();
        assert!(!d.key_saved_now(), "a renamed key is not shown as saved until checked");
        d.refresh_key_saved(); // load_secret is a no-op in tests
        assert_eq!(d.key_checked_for, "OTHER_KEY");
        assert!(!d.key_saved_now());
    }

    #[test]
    fn switching_to_chat_drops_only_the_auto_filled_decisions_address() {
        let mut d = enabled_draft();
        assert_eq!(d.base_url, RISK_DECISIONS_BASE_URL);
        d.set_chat_logprobs(true);
        assert!(d.base_url.is_empty(), "the Decisions default does not follow into chat");
        d.base_url = "https://my-proxy.example/v1".into();
        d.set_chat_logprobs(false);
        d.set_chat_logprobs(true);
        assert_eq!(d.base_url, "https://my-proxy.example/v1", "a typed address is kept");
    }

    // ── Log sources (#584) ───────────────────────────────────────────

    fn vlogs_draft() -> LogSourceDraft {
        let mut d = LogSourceDraft::blank(1);
        d.name = "vlogs-prod".into();
        d.url = "https://vlogs.example.com".into();
        d.auth = filar_core::LogAuth::Bearer;
        d.key_env = "FILAR_LOGS_VLOGS_PROD".into();
        d.key = "SECRET-LOG-KEY".into();
        d
    }

    #[test]
    fn a_log_source_draft_round_trips_without_its_key() {
        let d = vlogs_draft();
        let cfg = d.to_config().unwrap();
        assert_eq!(cfg.key_env, "FILAR_LOGS_VLOGS_PROD");
        assert_eq!(d.key_to_save(), Some(("FILAR_LOGS_VLOGS_PROD".into(), "SECRET-LOG-KEY".into())));
        let back = LogSourceDraft::from_config(&cfg, 7);
        assert_eq!(back.to_config().unwrap(), cfg);
        assert!(back.key.is_empty());

        let config = filar_core::Config {
            log_sources: vec![cfg.clone()],
            ..Default::default()
        };
        let toml_text = toml::to_string_pretty(&config).unwrap();
        assert!(toml_text.contains("[[log_sources]]") && !toml_text.contains("SECRET-LOG-KEY"), "{toml_text}");
        let launch = LaunchConfig {
            target: "local".into(),
            ssh: None,
            kube: None,
            kube_clusters: None,
            model: "m".into(),
            api_base_url: String::new(),
            api_key: String::new(),
            session_id: None,
            temperature: String::new(),
            extra_body: String::new(),
            selected_profile: None,
            key_env: String::new(),
            save_dir: None,
            profiles: vec![],
            ssh_targets: vec![],
            arbiter_profile: None,
            host_groups: None,
            risk_scorer: None,
            log_sources: Some(vec![cfg]),
        };
        let json = serde_json::to_string(&launch).unwrap();
        assert!(json.contains("vlogs-prod") && !json.contains("SECRET-LOG-KEY"), "{json}");
        let read: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(read.log_sources, launch.log_sources);
    }

    #[test]
    fn log_source_drafts_are_validated_like_the_config() {
        let mut d = vlogs_draft();
        d.url = "vlogs.example.com".into();
        assert!(d.to_config().unwrap_err().contains("url"));
        let mut d = vlogs_draft();
        d.max_lines = "many".into();
        assert!(d.to_config().unwrap_err().contains("max lines"));
        let mut d = vlogs_draft();
        d.kind = filar_core::LogSourceKind::OpenSearch;
        assert!(d.to_config().unwrap_err().contains("needs index"));
        d.index = "logs-*".into();
        d.tenant = "0:0".into();
        assert!(d.to_config().unwrap().tenant.is_none(), "a tenant field of another kind is dropped");
        let mut d = vlogs_draft();
        d.auth = filar_core::LogAuth::None;
        assert_eq!(d.to_config().unwrap().key_env, "", "no key name without a keyed auth");
        assert_eq!(d.key_to_save(), None);
    }

    #[test]
    fn launch_refuses_log_sources_linked_to_unknown_clusters_or_named_twice() {
        let mut app = make_app(make_meta(None, None, None, None));
        let mut d = vlogs_draft();
        d.clusters = "prod-eu".into();
        app.log_sources = vec![d.clone()];
        assert!(app.validate_log_sources().unwrap_err().contains("cluster prod-eu is not in the cluster list"));
        d.clusters.clear();
        let mut twin = d.clone();
        twin.id = 2;
        app.log_sources = vec![d, twin];
        assert!(app.validate_log_sources().unwrap_err().contains("used twice"));
    }

    #[test]
    fn suggested_key_names_and_check_queries() {
        assert_eq!(suggested_log_key_env("vlogs-prod"), "FILAR_LOGS_VLOGS_PROD");
        assert_eq!(suggested_log_key_env(""), "FILAR_LOGS_SOURCE");
        let loki = filar_core::LogSourceConfig {
            kind: "loki".into(),
            ..Default::default()
        };
        assert_eq!(log_check_query(&loki), r#"{namespace=~".+"}"#);
        assert_eq!(log_check_query(&vlogs_draft().to_config().unwrap()), "*");
    }

}
