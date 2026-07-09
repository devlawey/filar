//! Core crate: shared types, error handling, configuration, and secrets.
//!
//! This crate provides the foundation for the entire workspace:
//! - [`error`]: Error types and a unified `Result` alias.
//! - [`config`]: Configuration loading from TOML files and environment variables.
//! - [`secrets`]: Secure reading of API keys and other secrets from the environment.

pub mod chat;
pub mod config;
pub mod error;
pub mod secrets;
pub mod session;

pub use chat::ChatBlock;
pub use config::{Config, SshTarget, SshAuth, LlmConfig, LlmProfile, CommandConfirmMode, TimeoutConfig, HostKeyPolicy};
pub use error::{CoreError, Result};
pub use secrets::{EnvSecretProvider, SecretProvider, StaticSecretProvider};
pub use session::{default_base_dir, Session, SessionMeta, SessionStore};
