//! Configuration types and loading logic.
//!
//! Configuration is loaded from a TOML file ([`Config::load`]). Secrets such as
//! the GLM API key are **not** stored in the config file — they are read from
//! the environment via the [`crate::secrets`] module.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

// ---------------------------------------------------------------------------
// Top-level config
// ---------------------------------------------------------------------------

/// Root configuration object, deserialised from `config.toml`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Named SSH targets the agent can connect to.
    #[serde(default)]
    pub ssh_targets: Vec<SshTarget>,

    /// Default LLM-related settings (backward compatibility).
    #[serde(default)]
    pub llm: LlmConfig,

    /// Named LLM profiles for multi-LLM support (optional).
    #[serde(default)]
    pub llm_profiles: Vec<LlmProfile>,

    /// Timeout settings (seconds).
    #[serde(default)]
    pub timeouts: TimeoutConfig,

    /// Command confirmation policy.
    #[serde(default)]
    pub confirm_mode: CommandConfirmMode,

    /// Tag-bound confirmation policies from `[[tag_policies]]` (#414). A
    /// policy may only **tighten** the effective mode of targets carrying the
    /// tag: whenever any policy matches, the floor is the strictest of the
    /// global mode and all matching policies — a tag can never open more than
    /// the global mode allows. Targets with no matching policy are unaffected.
    #[serde(default)]
    pub tag_policies: Vec<TagPolicy>,

    /// Named host groups from `[[host_groups]]` (#418): a tag-intersection
    /// selection plus the policy, limits and LLM profile its members share.
    #[serde(default)]
    pub host_groups: Vec<HostGroup>,

    /// Directory where Ctrl+S session exports (`.md`) are written.
    /// `None` means the process working directory at startup.
    #[serde(default)]
    pub save_dir: Option<PathBuf>,

    /// When `true` (the default), an explicit Ctrl+S in the TUI also asks the
    /// session's LLM for a **runbook** — the session folded into a reusable
    /// procedure — written next to the export as `{stem}.runbook.md`. Set to
    /// `false` to keep Ctrl+S purely local: no network call, only the `.md`.
    /// The silent Explain-mode transcript save never generates runbooks,
    /// regardless of this setting (#401).
    #[serde(default = "default_save_runbook")]
    pub save_runbook: bool,

    /// Optional named LLM profile used as the command arbiter.
    /// When unset or invalid, the session profile is used instead.
    #[serde(default)]
    pub arbiter_profile: Option<String>,

    /// When `true`, run the independent command arbiter before each confirmation
    /// gate (`NeedsConfirmation`). Defaults to `true`; the arbiter only runs when
    /// a command actually needs confirmation (not in `Never` mode or allowlist
    /// auto-approve paths).
    #[serde(default = "default_arbiter_enabled")]
    pub arbiter_enabled: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ssh_targets: Vec::new(),
            llm: LlmConfig::default(),
            llm_profiles: Vec::new(),
            timeouts: TimeoutConfig::default(),
            confirm_mode: CommandConfirmMode::Allowlist,
            tag_policies: Vec::new(),
            host_groups: Vec::new(),
            save_dir: None,
            save_runbook: default_save_runbook(),
            arbiter_profile: None,
            arbiter_enabled: default_arbiter_enabled(),
        }
    }
}

fn default_arbiter_enabled() -> bool {
    true
}

/// Runbook generation alongside the Ctrl+S export is on by default: the
/// export is already an explicit user action, and the feature is what the
/// second file is for. Opt-out, like the arbiter (#401).
fn default_save_runbook() -> bool {
    true
}

// ---------------------------------------------------------------------------
// SSH target
// ---------------------------------------------------------------------------

/// A named SSH connection target.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTarget {
    /// Human-readable name (e.g. `"prod-web-1"`).
    pub name: String,
    /// Remote host or IP address.
    pub host: String,
    /// SSH port (default 22).
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// Remote user name.
    pub user: String,
    /// Authentication strategy.
    #[serde(default)]
    pub auth: SshAuth,

    /// Host key verification policy (default: TOFU).
    #[serde(default)]
    pub host_key_policy: HostKeyPolicy,

    /// Free-form tags (e.g. `"prod"`, `"web"`) for grouping and policies.
    /// Empty list = no tags; omitted from serialisation when empty (#413).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

fn default_ssh_port() -> u16 {
    22
}

/// SSH authentication method.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SshAuth {
    /// Use a key file from disk (e.g. `~/.ssh/id_ed25519`).
    Key {
        /// Path to the private key file.
        path: Option<PathBuf>,
    },
    /// Use the system SSH agent.
    #[default]
    Agent,
    /// Password-based authentication.
    Password {
        /// Password (optional — falls back to `SSH_PASSWORD` env var).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        password: Option<String>,
    },
}

// ---------------------------------------------------------------------------
// Host key policy
// ---------------------------------------------------------------------------

/// Host key verification policy for SSH connections.
///
/// Controls how the client handles the server's public key:
/// - [`Strict`](Self::Strict): reject unknown hosts (must be in known_hosts).
/// - [`Tofu`](Self::Tofu): trust on first use — accept, record, then verify.
/// - [`AcceptNew`](Self::AcceptNew): accept new keys without recording.
///
/// There is **no** "accept everything silently" option.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum HostKeyPolicy {
    /// Reject unknown hosts — only accept keys already in known_hosts.
    Strict,
    /// Trust on first use: accept and record new keys, reject mismatches (default).
    #[default]
    Tofu,
    /// Accept new keys without recording, reject mismatches.
    AcceptNew,
}

// ---------------------------------------------------------------------------
// LLM config
// ---------------------------------------------------------------------------

/// LLM service configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    /// Model identifier (e.g. `"glm-5.1"`).
    pub model: String,
    /// Base URL of the API (e.g. `"https://open.bigmodel.cn/api/paas/v4"`).
    pub api_base_url: String,
    /// Maximum number of tokens to generate in a single response.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Sampling temperature (0.0–2.0). `None` = provider default.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Nucleus sampling probability (0.0–1.0, exclusive of 0). `None` = provider default.
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Arbitrary extra fields merged into the JSON request body.
    ///
    /// Keys `model`, `messages`, `tools`, `stream` are protected and
    /// ignored (with a warning) if present in `extra_body`.
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "glm-5.1".to_string(),
            api_base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            max_tokens: default_max_tokens(),
            temperature: None,
            top_p: None,
            extra_body: None,
        }
    }
}

impl LlmConfig {
    /// Validate parameter ranges.
    ///
    /// Returns an error if `temperature` or `top_p` are outside their
    /// valid ranges.
    pub fn validate(&self) -> Result<()> {
        if let Some(t) = self.temperature {
            if !(0.0..=2.0).contains(&t) {
                return Err(CoreError::Config(format!(
                    "temperature must be in [0.0, 2.0], got {t}"
                )));
            }
        }
        if let Some(p) = self.top_p {
            if !(p > 0.0 && p <= 1.0) {
                return Err(CoreError::Config(format!(
                    "top_p must be in (0.0, 1.0], got {p}"
                )));
            }
        }
        Ok(())
    }
}

/// Default `max_tokens` for `[llm]` and `[[llm_profiles]]` — 4096 tokens.
///
/// Public so that front-ends can show the value they fall back to instead of
/// repeating the literal (see #380).
pub const DEFAULT_MAX_TOKENS: u32 = 4096;

fn default_max_tokens() -> u32 {
    DEFAULT_MAX_TOKENS
}

// ---------------------------------------------------------------------------
// LLM profile (named, for multi-LLM support)
// ---------------------------------------------------------------------------

/// A named LLM profile with its own API key environment variable.
///
/// Profiles allow selecting between different LLM backends (e.g. GLM,
/// DeepSeek) at launch time via `--llm <name>`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmProfile {
    /// Human-readable name (e.g. `"glm"`, `"deepseek"`).
    pub name: String,
    /// Model identifier (e.g. `"glm-5.1"`).
    pub model: String,
    /// Base URL of the API.
    pub api_base_url: String,
    /// Maximum number of tokens to generate.
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    /// Name of the environment variable / OS credential holding the API key
    /// (default: `"GLM_API_KEY"`).
    ///
    /// An **empty** `key_env` is an explicit marker that this profile does not
    /// require a key (local / air-gapped OpenAI-compatible servers such as
    /// ollama). A non-empty `key_env` whose secret is missing is still an error.
    #[serde(default = "default_glm_key_env")]
    pub key_env: String,
    /// Sampling temperature (0.0–2.0). `None` = provider default.
    #[serde(default)]
    pub temperature: Option<f32>,
    /// Nucleus sampling probability (0.0–1.0, exclusive of 0). `None` = provider default.
    #[serde(default)]
    pub top_p: Option<f32>,
    /// Arbitrary extra fields merged into the JSON request body.
    #[serde(default)]
    pub extra_body: Option<serde_json::Value>,
    /// Prompt-token count at which the session history is compacted before the
    /// next request. `0` disables compaction for this profile.
    ///
    /// Per-profile because context windows differ: a threshold that makes
    /// sense for a 1M-token window is meaningless for 128k. This is a
    /// client-side setting and is never sent to the API.
    #[serde(default = "default_compact_at_tokens")]
    pub compact_at_tokens: u64,
}

/// Default `[[llm_profiles]].compact_at_tokens` — 200k prompt tokens.
pub const DEFAULT_COMPACT_AT_TOKENS: u64 = 200_000;

fn default_compact_at_tokens() -> u64 {
    DEFAULT_COMPACT_AT_TOKENS
}

fn default_glm_key_env() -> String {
    "GLM_API_KEY".to_string()
}

impl LlmProfile {
    /// Whether this profile requires an API key.
    ///
    /// Empty `key_env` means keyless (local / air-gapped). Non-empty means the
    /// key must be resolved from memory, OS credential store, or the env var.
    pub fn requires_api_key(&self) -> bool {
        !self.key_env.trim().is_empty()
    }
}

impl From<&LlmProfile> for LlmConfig {
    fn from(p: &LlmProfile) -> Self {
        Self {
            model: p.model.clone(),
            api_base_url: p.api_base_url.clone(),
            max_tokens: p.max_tokens,
            temperature: p.temperature,
            top_p: p.top_p,
            extra_body: p.extra_body.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

/// Default `[timeouts].command_secs` — 5 minutes.
///
/// Applied to SSH marker wait and local subprocess execution. Long jobs such as
/// `du`/`find` on large trees need more than the previous 120s cap.
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 300;

/// Timeout configuration (all values in seconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Timeout for a single command executed via the transport layer.
    #[serde(default = "default_command_timeout")]
    pub command_secs: u64,
    /// Timeout for a single LLM API call.
    ///
    /// For a non-streaming call this bounds the whole request. For a streaming
    /// reply it bounds the connect phase and the pause between chunks, not the
    /// total length of the answer — a model that keeps producing tokens is
    /// never cut off.
    #[serde(default = "default_llm_timeout")]
    pub llm_secs: u64,
    /// Timeout for establishing an SSH connection.
    #[serde(default = "default_connect_timeout")]
    pub connect_secs: u64,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            command_secs: default_command_timeout(),
            llm_secs: default_llm_timeout(),
            connect_secs: default_connect_timeout(),
        }
    }
}

fn default_command_timeout() -> u64 {
    DEFAULT_COMMAND_TIMEOUT_SECS
}
fn default_llm_timeout() -> u64 {
    60
}
fn default_connect_timeout() -> u64 {
    15
}

impl TimeoutConfig {
    /// Reject values that would make every command fail immediately.
    pub fn validate(&self) -> Result<()> {
        if self.command_secs == 0 {
            return Err(CoreError::Config(
                "timeouts.command_secs must be greater than 0".into(),
            ));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Confirmation mode
// ---------------------------------------------------------------------------

/// Controls whether the agent must ask the user before executing commands.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CommandConfirmMode {
    /// Every command requires explicit user approval (safest).
    Always,
    /// Read-only commands in the allowlist are auto-approved; everything else
    /// requires confirmation (default).
    #[default]
    Allowlist,
    /// No confirmation required (dangerous — use only in trusted sandboxes).
    Never,
    /// Safe mode: every command requires confirmation AND a mandatory
    /// explanation from the model. Read-only commands are NOT auto-approved.
    /// The system prompt gets a SAFE MODE block appended.
    Explain,
}

impl CommandConfirmMode {
    /// Strictness rank used by tag-policy resolution (#414):
    /// `Explain` > `Always` > `Allowlist` > `Never`.
    pub fn strictness(self) -> u8 {
        match self {
            CommandConfirmMode::Never => 0,
            CommandConfirmMode::Allowlist => 1,
            CommandConfirmMode::Always => 2,
            CommandConfirmMode::Explain => 3,
        }
    }

    /// The stricter of the two modes (#414).
    pub fn strictest(self, other: CommandConfirmMode) -> CommandConfirmMode {
        if other.strictness() > self.strictness() {
            other
        } else {
            self
        }
    }
}

/// A tag-bound confirmation policy (#414), from `[[tag_policies]]`.
///
/// A policy may only **tighten** `confirm_mode` for targets whose
/// [`SshTarget::tags`] contain [`tag`](Self::tag) — never loosen it. When no
/// policy matches the target, no floor is applied (see [`tag_policy_floor`]).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TagPolicy {
    /// Tag name, matched exactly against `SshTarget::tags`.
    pub tag: String,
    /// Mode this tag forces (subject to the global floor).
    pub confirm_mode: CommandConfirmMode,
}

/// The confirmation-mode floor imposed by tag policies for a target with
/// `tags` (#414).
///
/// `Some(mode)` = the strictest of the global mode and every matching policy:
/// a tag can never open more than the global mode allows, and with several
/// matching policies the strictest one wins. `None` = no policy matches, so
/// there is no floor to apply.
pub fn tag_policy_floor(
    global: CommandConfirmMode,
    policies: &[TagPolicy],
    tags: &[String],
) -> Option<CommandConfirmMode> {
    let mut matching = policies
        .iter()
        .filter(|p| tags.iter().any(|t| t == &p.tag));
    let first = matching.next()?;
    Some(
        matching.fold(global.strictest(first.confirm_mode), |acc, p| {
            acc.strictest(p.confirm_mode)
        }),
    )
}

// ---------------------------------------------------------------------------
// Host groups (#418)
// ---------------------------------------------------------------------------

/// What a host group permits its members to do (#418).
///
/// Only read-only exists so far — the fleet mode is defined as "one dialogue
/// session with N executors, read-only". An unknown value in the config is a
/// parse error, not a silently loosened policy: a typo must not turn a ban
/// into permission.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum HostGroupPolicy {
    /// Members are read-only: write commands are refused by the transport.
    #[default]
    ReadOnly,
}

/// A named selection of SSH targets, from `[[host_groups]]` (#418).
///
/// The selection is a tag **intersection**: a target belongs to the group
/// when **all** of [`match_tags`](Self::match_tags) are present among its
/// [`SshTarget::tags`]. An empty `match` selects nothing — deliberately not
/// the whole fleet: an unfinished rule must never widen the blast radius.
///
/// The group carries more than the selection — a policy, a parallelism
/// limit, a per-host deadline and an optional LLM profile — which is what
/// makes it a group rather than a convenience list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HostGroup {
    /// Human-readable group name.
    pub name: String,
    /// Tags a target must **all** carry, e.g. `match = ["work", "prod"]`.
    #[serde(default, rename = "match")]
    pub match_tags: Vec<String>,
    /// Policy the group imposes on its members (default: read-only).
    #[serde(default)]
    pub policy: HostGroupPolicy,
    /// Upper bound on executors running at the same time (default: 3).
    #[serde(default = "default_max_parallel")]
    pub max_parallel: u32,
    /// Deadline for one command on one host, in seconds (default: 30).
    #[serde(default = "default_per_host_timeout_secs")]
    pub per_host_timeout_secs: u64,
    /// LLM profile for the group's dialogue; `None` = the session profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub llm_profile: Option<String>,
}

fn default_max_parallel() -> u32 {
    3
}

fn default_per_host_timeout_secs() -> u64 {
    30
}

impl Default for HostGroup {
    fn default() -> Self {
        Self {
            name: String::new(),
            match_tags: Vec::new(),
            policy: HostGroupPolicy::default(),
            max_parallel: default_max_parallel(),
            per_host_timeout_secs: default_per_host_timeout_secs(),
            llm_profile: None,
        }
    }
}

impl HostGroup {
    /// Reject definitions that cannot mean anything (#418): an empty name or
    /// zero limits. An empty rule is deliberately allowed — it selects
    /// nobody, and the Groups tab states that explicitly.
    pub fn validate(&self) -> Result<()> {
        if self.name.trim().is_empty() {
            return Err(CoreError::Config(
                "host_groups: name must not be empty".into(),
            ));
        }
        if self.max_parallel == 0 {
            return Err(CoreError::Config(format!(
                "host_groups: \"{}\": max_parallel must be greater than 0",
                self.name
            )));
        }
        if self.per_host_timeout_secs == 0 {
            return Err(CoreError::Config(format!(
                "host_groups: \"{}\": per_host_timeout_secs must be greater than 0",
                self.name
            )));
        }
        Ok(())
    }
}

/// The targets a group selects: those carrying **all** of its
/// [`match_tags`](HostGroup::match_tags) (#418).
///
/// An empty rule selects nothing — not everything. Tag comparison is exact,
/// like [`tag_policy_floor`].
pub fn select_hosts_for_group<'a>(
    group: &HostGroup,
    targets: &'a [SshTarget],
) -> Vec<&'a SshTarget> {
    if group.match_tags.is_empty() {
        return Vec::new();
    }
    targets
        .iter()
        .filter(|t| {
            group
                .match_tags
                .iter()
                .all(|tag| t.tags.iter().any(|t| t == tag))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

impl Config {
    /// Load configuration from a TOML file at `path`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path).map_err(|e| {
            CoreError::Config(format!(
                "failed to read config file {}: {e}",
                path.display()
            ))
        })?;
        let cfg: Self = toml::from_str(&contents).map_err(|e| {
            CoreError::Config(format!(
                "failed to parse config file {}: {e}",
                path.display()
            ))
        })?;
        // Validate LLM parameter ranges.
        cfg.llm.validate()?;
        for p in &cfg.llm_profiles {
            LlmConfig::from(p).validate()?;
        }
        cfg.timeouts.validate()?;
        for group in &cfg.host_groups {
            group.validate()?;
        }
        Ok(cfg)
    }

    /// Resolve the effective arbiter profile name.
    ///
    /// When `arbiter_profile` is unset, returns `session_profile`. When set but
    /// not found in `profiles`, falls back to `session_profile` and returns a
    /// human-readable warning for the UI.
    pub fn resolve_arbiter_profile<'a>(
        &'a self,
        session_profile: &'a str,
        profiles: &'a [LlmProfile],
    ) -> (&'a str, Option<String>) {
        let requested = match &self.arbiter_profile {
            None => {
                return (session_profile, None);
            }
            Some(name) if name.trim().is_empty() => {
                return (session_profile, None);
            }
            Some(name) => name.as_str(),
        };

        if profiles.iter().any(|p| p.name == requested) {
            return (requested, None);
        }

        let warning = format!(
            "Arbiter profile '{requested}' not found — using session profile '{session_profile}' for command audits."
        );
        (session_profile, Some(warning))
    }

    /// Convenience: load from `config.toml`.
    ///
    /// Search order:
    /// 1. `FILAR_CONFIG` environment variable (explicit path)
    /// 2. `config.toml` in the current working directory (override for development)
    /// 3. `{OS data dir}/filar/config.toml` (shared system-wide config)
    /// 4. `config.toml` next to the executable
    ///
    /// Falls back to built-in defaults if no file is found anywhere.
    pub fn load_default() -> Result<Self> {
        // 1. FILAR_CONFIG env var.
        if let Ok(explicit) = std::env::var("FILAR_CONFIG") {
            let p = std::path::PathBuf::from(explicit);
            tracing::info!(path = %p.display(), "loading config from FILAR_CONFIG");
            return Self::load(&p);
        }
        // 2. Current working directory (local override for development).
        if std::path::Path::new("config.toml").exists() {
            // Warn if app-data config is also present — local file overrides.
            if let Ok(base) = crate::default_base_dir() {
                let app_config = base.join("filar").join("config.toml");
                if app_config.exists() {
                    tracing::warn!(
                        app_data = %app_config.display(),
                        "local ./config.toml overrides app-data config"
                    );
                }
            }
            tracing::info!("loading config.toml from current directory");
            return Self::load("config.toml");
        }
        // 3. App-data directory (unified config location).
        if let Ok(base) = crate::default_base_dir() {
            let app_config = base.join("filar").join("config.toml");
            if app_config.exists() {
                tracing::info!(path = %app_config.display(), "loading config from app-data dir");
                return Self::load(&app_config);
            }
        }
        // 4. Next to the executable.
        if let Ok(exe) = std::env::current_exe() {
            if let Some(exe_dir) = exe.parent() {
                let exe_config = exe_dir.join("config.toml");
                if exe_config.exists() {
                    tracing::info!(path = %exe_config.display(), "loading config from exe directory");
                    return Self::load(&exe_config);
                }
            }
        }
        tracing::info!("no config.toml found, using built-in defaults");
        Ok(Self::default())
    }

    /// Look up an SSH target by name.
    pub fn ssh_target(&self, name: &str) -> Option<&SshTarget> {
        self.ssh_targets.iter().find(|t| t.name == name)
    }

    /// Select an LLM configuration by profile name.
    ///
    /// Returns `(LlmConfig, key_env)` where `key_env` is the name of the
    /// environment variable holding the API key.
    ///
    /// - `None` → the default `[llm]` section with `"GLM_API_KEY"`.
    /// - `Some(name)` → searches `llm_profiles`; error if not found.
    pub fn select_llm(&self, name: Option<&str>) -> Result<(LlmConfig, String)> {
        match name {
            None => Ok((self.llm.clone(), default_glm_key_env())),
            Some(n) => self
                .llm_profiles
                .iter()
                .find(|p| p.name == n)
                .map(|p| (LlmConfig::from(p), p.key_env.clone()))
                .ok_or_else(|| CoreError::Config(format!("LLM profile '{n}' not found"))),
        }
    }

    /// List all available LLM profile names (including the implicit default).
    pub fn llm_profile_names(&self) -> Vec<String> {
        let mut names = vec!["default".to_string()];
        names.extend(self.llm_profiles.iter().map(|p| p.name.clone()));
        names
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config_with_explain_mode() {
        let toml = r#"
confirm_mode = "explain"

[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.confirm_mode, CommandConfirmMode::Explain);
    }

    // ── Tag policies (#414) ─────────────────────────────────────

    fn policy(tag: &str, mode: CommandConfirmMode) -> TagPolicy {
        TagPolicy { tag: tag.into(), confirm_mode: mode }
    }

    #[test]
    fn tag_policy_stricter_than_global_applies() {
        // Global allowlist + a prod policy of `always`: the policy wins.
        let policies = [policy("prod", CommandConfirmMode::Always)];
        let tags = vec!["prod".to_string()];
        assert_eq!(
            tag_policy_floor(CommandConfirmMode::Allowlist, &policies, &tags),
            Some(CommandConfirmMode::Always)
        );
    }

    #[test]
    fn tag_policy_looser_than_global_is_ignored() {
        // Global always + a `never` policy: the global mode wins — a tag can
        // never open more than the global allows.
        let policies = [policy("prod", CommandConfirmMode::Never)];
        let tags = vec!["prod".to_string()];
        assert_eq!(
            tag_policy_floor(CommandConfirmMode::Always, &policies, &tags),
            Some(CommandConfirmMode::Always)
        );
    }

    #[test]
    fn several_matching_tag_policies_pick_the_strictest() {
        let policies = [
            policy("web", CommandConfirmMode::Always),
            policy("prod", CommandConfirmMode::Explain),
            policy("db", CommandConfirmMode::Never),
        ];
        let tags = vec!["web".to_string(), "prod".to_string(), "db".to_string()];
        assert_eq!(
            tag_policy_floor(CommandConfirmMode::Allowlist, &policies, &tags),
            Some(CommandConfirmMode::Explain)
        );
    }

    #[test]
    fn no_matching_policy_has_no_floor() {
        // Targets without a matching policy keep the tab's own mode: the
        // F2 Explain toggle must stay able to move freely on them.
        let policies = [policy("prod", CommandConfirmMode::Explain)];
        let tags = vec!["test".to_string()];
        assert_eq!(
            tag_policy_floor(CommandConfirmMode::Allowlist, &policies, &tags),
            None
        );
        assert_eq!(tag_policy_floor(CommandConfirmMode::Allowlist, &[], &tags), None);
    }

    #[test]
    fn strictest_orders_modes_and_never_loosens() {
        use CommandConfirmMode::*;
        assert_eq!(Allowlist.strictest(Always), Always);
        assert_eq!(Always.strictest(Allowlist), Always);
        assert_eq!(Explain.strictest(Never), Explain);
        assert_eq!(Never.strictest(Never), Never);
        assert!(Explain.strictness() > Always.strictness());
        assert!(Always.strictness() > Allowlist.strictness());
        assert!(Allowlist.strictness() > Never.strictness());
    }

    #[test]
    fn tag_policies_parse_from_toml() {
        let toml = r#"
confirm_mode = "allowlist"

[[tag_policies]]
tag = "prod"
confirm_mode = "always"

[[tag_policies]]
tag = "db"
confirm_mode = "explain"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.tag_policies.len(), 2);
        assert_eq!(cfg.tag_policies[0].tag, "prod");
        assert_eq!(cfg.tag_policies[0].confirm_mode, CommandConfirmMode::Always);
        assert_eq!(cfg.tag_policies[1].tag, "db");
        assert_eq!(cfg.tag_policies[1].confirm_mode, CommandConfirmMode::Explain);
    }

    #[test]
    fn parse_config_arbiter_defaults() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert!(cfg.arbiter_enabled);
        assert!(cfg.arbiter_profile.is_none());
    }

    #[test]
    fn runbook_generation_is_on_by_default_and_optout_parses() {
        // A config that predates the setting gets runbooks; an explicit
        // `false` turns Ctrl+S back into a purely local save (#401).
        let minimal = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
"#;
        let cfg: Config = toml::from_str(minimal).unwrap();
        assert!(cfg.save_runbook, "runbook generation must default to on");

        // The key must sit at the document root — after a `[llm]` header it
        // would land inside that table and the setting would not parse.
        let opted_out = format!("save_runbook = false\n{minimal}");
        let cfg: Config = toml::from_str(&opted_out).unwrap();
        assert!(!cfg.save_runbook);
    }

    #[test]
    fn resolve_arbiter_profile_falls_back_when_missing() {
        let cfg = Config {
            arbiter_profile: Some("missing".into()),
            ..Config::default()
        };
        let profiles = vec![LlmProfile {
            name: "session".into(),
            model: "m".into(),
            api_base_url: "http://localhost".into(),
            key_env: String::new(),
            max_tokens: 4096,
            temperature: None,
            top_p: None,
            extra_body: None,
            compact_at_tokens: DEFAULT_COMPACT_AT_TOKENS,
        }];
        let (name, warn) = cfg.resolve_arbiter_profile("session", &profiles);
        assert_eq!(name, "session");
        assert!(warn.is_some());
    }

    #[test]
    fn parse_minimal_config() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"

[[ssh_targets]]
name = "test"
host = "127.0.0.1"
port = 2222
user = "testuser"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.llm.model, "glm-5.1");
        assert_eq!(cfg.ssh_targets.len(), 1);
        assert_eq!(cfg.ssh_targets[0].port, 2222);
        assert_eq!(cfg.confirm_mode, CommandConfirmMode::Allowlist);
        assert_eq!(cfg.timeouts.command_secs, DEFAULT_COMMAND_TIMEOUT_SECS);
    }

    #[test]
    fn default_command_timeout_is_five_minutes() {
        assert_eq!(DEFAULT_COMMAND_TIMEOUT_SECS, 300);
        assert_eq!(TimeoutConfig::default().command_secs, 300);
        // Omitted `[timeouts]` still deserialises to the 300s default.
        let cfg: Config = toml::from_str(
            r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
"#,
        )
        .unwrap();
        assert_eq!(cfg.timeouts.command_secs, 300);
    }

    #[test]
    fn reject_zero_command_timeout() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"

[timeouts]
command_secs = 0
"#;
        let tmp = std::env::temp_dir().join(format!(
            "filar_config_timeout_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&tmp, toml).unwrap();
        let result = Config::load(&tmp);
        let _ = std::fs::remove_file(&tmp);
        let err = result.expect_err("Config::load should reject command_secs = 0");
        assert!(
            err.to_string().contains("command_secs"),
            "error should mention command_secs: {err}"
        );
    }

    #[test]
    fn parse_full_config() {
        let toml = r#"
confirm_mode = "allowlist"

[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
max_tokens = 8192

[timeouts]
command_secs = 90
llm_secs = 90
connect_secs = 10

[[ssh_targets]]
name = "prod"
host = "10.0.0.5"
user = "deploy"

[ssh_targets.auth]
type = "key"
path = "~/.ssh/id_ed25519"

[[ssh_targets]]
name = "staging"
host = "10.0.0.6"
user = "ubuntu"

[ssh_targets.auth]
type = "agent"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.llm.max_tokens, 8192);
        assert_eq!(cfg.timeouts.command_secs, 90);
        assert_eq!(cfg.confirm_mode, CommandConfirmMode::Allowlist);
        assert_eq!(cfg.ssh_targets.len(), 2);
        assert_eq!(cfg.ssh_target("staging").unwrap().host, "10.0.0.6");
    }

    #[test]
    fn parse_ssh_target_tags() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"

[[ssh_targets]]
name = "prod"
host = "10.0.0.5"
user = "deploy"
tags = ["prod", "web"]

[[ssh_targets]]
name = "dev"
host = "10.0.0.6"
user = "dev"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ssh_targets[0].tags, vec!["prod", "web"]);
        // Omitted tags deserialise to an empty list.
        assert!(cfg.ssh_targets[1].tags.is_empty());
    }

    #[test]
    fn ssh_target_tags_round_trip_skips_empty() {
        let mut target = SshTarget {
            name: "prod".into(),
            host: "10.0.0.5".into(),
            port: 22,
            user: "deploy".into(),
            auth: SshAuth::Agent,
            host_key_policy: HostKeyPolicy::Tofu,
            tags: vec!["prod".into()],
        };
        let json = serde_json::to_string(&target).unwrap();
        assert!(json.contains("\"tags\":[\"prod\"]"), "json: {json}");
        let back: SshTarget = serde_json::from_str(&json).unwrap();
        assert_eq!(back.tags, vec!["prod"]);

        // Empty tags are not serialised at all — old and new files stay
        // byte-compatible for untagged hosts.
        target.tags.clear();
        let json = serde_json::to_string(&target).unwrap();
        assert!(!json.contains("tags"), "json: {json}");
    }

    #[test]
    fn parse_host_key_policy() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"

[[ssh_targets]]
name = "prod"
host = "10.0.0.5"
user = "deploy"
host_key_policy = "strict"

[[ssh_targets]]
name = "dev"
host = "10.0.0.6"
user = "dev"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.ssh_targets[0].host_key_policy, HostKeyPolicy::Strict);
        // Default is Tofu.
        assert_eq!(cfg.ssh_targets[1].host_key_policy, HostKeyPolicy::Tofu);
    }

    #[test]
    fn parse_multi_llm_config() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"

[[llm_profiles]]
name = "deepseek"
model = "deepseek-chat"
api_base_url = "https://api.deepseek.com/v1"
max_tokens = 8192
key_env = "DEEPSEEK_API_KEY"

[[llm_profiles]]
name = "glm"
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.llm_profiles.len(), 2);

        // Default selection (no name).
        let (llm_cfg, key_env) = cfg.select_llm(None).unwrap();
        assert_eq!(llm_cfg.model, "glm-5.1");
        assert_eq!(key_env, "GLM_API_KEY");

        // Named profile.
        let (llm_cfg, key_env) = cfg.select_llm(Some("deepseek")).unwrap();
        assert_eq!(llm_cfg.model, "deepseek-chat");
        assert_eq!(llm_cfg.max_tokens, 8192);
        assert_eq!(key_env, "DEEPSEEK_API_KEY");

        // Profile with default key_env.
        let (_, key_env) = cfg.select_llm(Some("glm")).unwrap();
        assert_eq!(key_env, "GLM_API_KEY");

        // Non-existent profile.
        assert!(cfg.select_llm(Some("nonexistent")).is_err());

        // Profile names list.
        assert_eq!(cfg.llm_profile_names(), vec!["default", "deepseek", "glm"]);
    }

    #[test]
    fn parse_config_with_temperature() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
temperature = 0.3
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.llm.temperature, Some(0.3));
        assert_eq!(cfg.llm.top_p, None);
        assert_eq!(cfg.llm.extra_body, None);
    }

    #[test]
    fn parse_config_with_extra_body() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
temperature = 0.5
top_p = 0.9
[llm.extra_body]
thinking = { type = "disabled" }
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        assert_eq!(cfg.llm.temperature, Some(0.5));
        assert_eq!(cfg.llm.top_p, Some(0.9));
        assert!(cfg.llm.extra_body.is_some());
        assert_eq!(cfg.llm.extra_body.as_ref().unwrap()["thinking"]["type"], "disabled");
    }

    #[test]
    fn validate_temperature_out_of_range() {
        let cfg = LlmConfig {
            model: "test".into(),
            api_base_url: "http://localhost".into(),
            max_tokens: 4096,
            temperature: Some(3.0),
            top_p: None,
            extra_body: None,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_top_p_zero() {
        let cfg = LlmConfig {
            model: "test".into(),
            api_base_url: "http://localhost".into(),
            max_tokens: 4096,
            temperature: None,
            top_p: Some(0.0),
            extra_body: None,
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_valid_params() {
        let cfg = LlmConfig {
            model: "test".into(),
            api_base_url: "http://localhost".into(),
            max_tokens: 4096,
            temperature: Some(1.5),
            top_p: Some(0.7),
            extra_body: None,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn profile_carries_params() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
[[llm_profiles]]
name = "local"
model = "llama3"
api_base_url = "http://localhost:11434/v1"
temperature = 0.2
[llm_profiles.extra_body]
options = { num_ctx = 8192 }
"#;
        let cfg: Config = toml::from_str(toml).unwrap();
        let (llm_cfg, _) = cfg.select_llm(Some("local")).unwrap();
        assert_eq!(llm_cfg.temperature, Some(0.2));
        assert!(llm_cfg.extra_body.is_some());
    }

    #[test]
    fn config_load_rejects_out_of_range_temperature() {
        let toml = r#"
[llm]
model = "glm-5.1"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
temperature = 5.0
"#;
        let tmp = std::env::temp_dir().join(format!(
            "filar_config_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&tmp, toml).unwrap();
        let result = Config::load(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_err(), "Config::load should reject temperature=5.0");
    }

    // ── Host groups (#418) ─────────────────────────────────────

    fn group(name: &str, tags: &[&str]) -> HostGroup {
        HostGroup {
            name: name.into(),
            match_tags: tags.iter().map(|t| t.to_string()).collect(),
            ..HostGroup::default()
        }
    }

    fn target_with_tags(name: &str, tags: &[&str]) -> SshTarget {
        SshTarget {
            name: name.into(),
            host: "10.0.0.1".into(),
            port: 22,
            user: "root".into(),
            auth: SshAuth::default(),
            host_key_policy: HostKeyPolicy::default(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
        }
    }

    #[test]
    fn host_group_round_trip() {
        // The exact shape from #418.
        let text = r#"
[[host_groups]]
name = "Рабочий прод"
match = ["work", "prod"]
policy = "read-only"
max_parallel = 3
per_host_timeout_secs = 30
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        let g = &cfg.host_groups[0];
        assert_eq!(g.name, "Рабочий прод");
        assert_eq!(g.match_tags, ["work", "prod"]);
        assert_eq!(g.policy, HostGroupPolicy::ReadOnly);
        assert_eq!(g.max_parallel, 3);
        assert_eq!(g.per_host_timeout_secs, 30);
        assert_eq!(g.llm_profile, None);

        // Round trip: serialize and parse back — the definition survives.
        let serialized = toml::to_string(&cfg).unwrap();
        let back: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(back.host_groups, cfg.host_groups);
        assert!(
            !serialized.contains("llm_profile ="),
            "an unset profile must stay out of the file"
        );
    }

    #[test]
    fn host_group_defaults_fill_missing_fields() {
        let text = r#"
[[host_groups]]
name = "prod"
match = ["prod"]
"#;
        let cfg: Config = toml::from_str(text).unwrap();
        let g = &cfg.host_groups[0];
        assert_eq!(
            g.policy,
            HostGroupPolicy::ReadOnly,
            "read-only is the only policy and the default"
        );
        assert_eq!(g.max_parallel, 3);
        assert_eq!(g.per_host_timeout_secs, 30);
        assert_eq!(g.llm_profile, None);
    }

    #[test]
    fn host_group_unknown_policy_is_rejected() {
        let text = r#"
[[host_groups]]
name = "prod"
match = ["prod"]
policy = "read-write"
"#;
        assert!(
            toml::from_str::<Config>(text).is_err(),
            "a policy typo must fail the parse, not silently loosen the ban"
        );
    }

    #[test]
    fn host_group_selects_the_tag_intersection() {
        let targets = [
            target_with_tags("both", &["work", "prod"]),
            target_with_tags("work-only", &["work"]),
            target_with_tags("prod-only", &["prod"]),
            target_with_tags("none", &[]),
        ];
        let selected: Vec<&str> =
            select_hosts_for_group(&group("prod", &["work", "prod"]), &targets)
                .iter()
                .map(|t| t.name.as_str())
                .collect();
        assert_eq!(selected, ["both"], "only the host carrying every tag matches");
    }

    #[test]
    fn host_group_empty_match_selects_nothing() {
        let targets = [
            target_with_tags("a", &["prod"]),
            target_with_tags("b", &[]),
        ];
        // Not "the whole fleet": an unfinished rule must not widen the list.
        assert!(select_hosts_for_group(&group("everything?", &[]), &targets).is_empty());
    }

    #[test]
    fn host_group_with_no_matches_selects_nothing() {
        let targets = [target_with_tags("a", &["dev"])];
        assert!(select_hosts_for_group(&group("prod", &["prod"]), &targets).is_empty());
    }

    #[test]
    fn host_group_validation_rejects_empty_name_and_zero_limits() {
        assert!(HostGroup {
            name: "  ".into(),
            ..HostGroup::default()
        }
        .validate()
        .is_err());
        assert!(HostGroup {
            name: "prod".into(),
            max_parallel: 0,
            ..HostGroup::default()
        }
        .validate()
        .is_err());
        assert!(HostGroup {
            name: "prod".into(),
            per_host_timeout_secs: 0,
            ..HostGroup::default()
        }
        .validate()
        .is_err());
        // An empty rule is legal — it selects nobody, and the UI says so.
        assert!(HostGroup {
            name: "prod".into(),
            ..HostGroup::default()
        }
        .validate()
        .is_ok());
    }

    #[test]
    fn config_load_rejects_a_zero_group_limit() {
        let toml = r#"
[[host_groups]]
name = "prod"
match = ["prod"]
max_parallel = 0
"#;
        let tmp = std::env::temp_dir().join(format!(
            "filar_config_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::write(&tmp, toml).unwrap();
        let result = Config::load(&tmp);
        let _ = std::fs::remove_file(&tmp);
        assert!(result.is_err(), "Config::load should reject max_parallel = 0");
    }

    /// The only test in this crate that touches `FILAR_CONFIG`.
    ///
    /// `cargo test` runs tests as threads of one process and environment
    /// variables are per-process, so two tests setting this variable race each
    /// other regardless of what their guards do. Keep it that way: a second
    /// test that reads or writes `FILAR_CONFIG` (or calls `load_default`, which
    /// reads it) must not be added without putting both behind a shared lock.
    #[test]
    fn load_default_prefers_filar_config_env() {
        let dir = std::env::temp_dir().join(format!("filar_cfg_test_{}", std::process::id()));
        let path = dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, "[llm]\nmodel = \"env-model\"\napi_base_url = \"https://test.example.com\"\n").unwrap();

        let _dir_guard = DirGuard(dir);
        let old_val = std::env::var("FILAR_CONFIG").ok();
        std::env::set_var("FILAR_CONFIG", path.as_os_str());
        let _env_guard = EnvGuard { key: "FILAR_CONFIG", old: old_val };

        let cfg = Config::load_default().unwrap();
        assert_eq!(cfg.llm.model, "env-model", "FILAR_CONFIG must take priority");
    }

    struct EnvGuard { key: &'static str, old: Option<String> }
    impl Drop for EnvGuard { fn drop(&mut self) { match &self.old { Some(v) => std::env::set_var(self.key, v), None => std::env::remove_var(self.key), } } }
    struct DirGuard(std::path::PathBuf);
    impl Drop for DirGuard { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }
}
