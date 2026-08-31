//! Core crate: shared types, error handling, configuration, and secrets.
//!
//! This crate provides the foundation for the entire workspace:
//! - [`error`]: Error types and a unified `Result` alias.
//! - [`config`]: Configuration loading from TOML files and environment variables.
//! - [`secrets`]: Secure reading of API keys and other secrets from the environment.

pub mod chat;
pub mod compaction;
pub mod config;
pub mod error;
pub mod secrets;
pub mod session;

pub use chat::ChatBlock;
pub use compaction::{compaction_boundary, should_compact, DEFAULT_KEEP_TURNS};
pub use config::{
    Config, SshTarget, SshAuth, LlmConfig, LlmProfile, CommandConfirmMode, TimeoutConfig,
    HostKeyPolicy, DEFAULT_COMMAND_TIMEOUT_SECS, DEFAULT_COMPACT_AT_TOKENS,
};
pub use error::{CoreError, Result};
pub use secrets::{
    ssh_cred_name, ssh_target_display_name, EnvSecretProvider, KeyringSecretProvider,
    SecretProvider, StaticSecretProvider, redact,
};
pub use session::{default_base_dir, ProfileUsage, Session, SessionMeta, SessionStore};
