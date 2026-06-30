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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            ssh_targets: Vec::new(),
            llm: LlmConfig::default(),
            llm_profiles: Vec::new(),
            timeouts: TimeoutConfig::default(),
            confirm_mode: CommandConfirmMode::Allowlist,
        }
    }
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
        #[serde(default)]
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
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "glm-5.1".to_string(),
            api_base_url: "https://open.bigmodel.cn/api/paas/v4".to_string(),
            max_tokens: default_max_tokens(),
        }
    }
}

fn default_max_tokens() -> u32 {
    4096
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
    /// Name of the environment variable holding the API key
    /// (default: `"GLM_API_KEY"`).
    #[serde(default = "default_glm_key_env")]
    pub key_env: String,
}

fn default_glm_key_env() -> String {
    "GLM_API_KEY".to_string()
}

impl From<&LlmProfile> for LlmConfig {
    fn from(p: &LlmProfile) -> Self {
        Self {
            model: p.model.clone(),
            api_base_url: p.api_base_url.clone(),
            max_tokens: p.max_tokens,
        }
    }
}

// ---------------------------------------------------------------------------
// Timeouts
// ---------------------------------------------------------------------------

/// Timeout configuration (all values in seconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeoutConfig {
    /// Timeout for a single command executed via the transport layer.
    #[serde(default = "default_command_timeout")]
    pub command_secs: u64,
    /// Timeout for a single LLM API call.
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
    120
}
fn default_llm_timeout() -> u64 {
    60
}
fn default_connect_timeout() -> u64 {
    15
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
        Ok(cfg)
    }

    /// Convenience: load from `config.toml` in the current directory.
    pub fn load_default() -> Result<Self> {
        Self::load("config.toml")
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
command_secs = 300
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
        assert_eq!(cfg.timeouts.command_secs, 300);
        assert_eq!(cfg.confirm_mode, CommandConfirmMode::Allowlist);
        assert_eq!(cfg.ssh_targets.len(), 2);
        assert_eq!(cfg.ssh_target("staging").unwrap().host, "10.0.0.6");
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
}
