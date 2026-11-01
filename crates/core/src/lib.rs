//! Core crate: shared types, error handling, configuration, and secrets.
//!
//! This crate provides the foundation for the entire workspace:
//! - [`error`]: Error types and a unified `Result` alias.
//! - [`config`]: Configuration loading from TOML files and environment variables.
//! - [`secrets`]: Secure reading of API keys and other secrets from the environment.
//! - [`fleet_checks`]: The declarative catalog of fleet checks (built-in + user).
//! - [`fleet_op`]: Fleet operations — one question, a frozen set of hosts.
//! - [`os_family`]: Which family of OS a host belongs to, for command variants.

pub mod chat;
pub mod compaction;
pub mod config;
pub mod error;
pub mod fleet_checks;
pub mod fleet_file_check;
pub mod fleet_op;
pub mod kube_pod;
pub mod log_sources;
pub mod os_family;
pub mod secrets;
pub mod session;

pub use chat::{ChatBlock, ToolInvocation, SHELL_ESCAPE_EXPLANATION};
pub use compaction::{
    compact_history, compaction_boundary, should_compact, transcript_for_summary,
    DEFAULT_KEEP_TURNS,
};
pub use config::{
    is_read_only_target, select_hosts_for_group, tag_policy_floor, target_matches_group,
    CommandConfirmMode, Config, HostGroup, HostGroupPolicy, HostKeyPolicy, KubeCluster, KubeGroup, LlmConfig,
    LlmProfile, RiskScorerConfig, RiskScorerKind, SshAuth, SshTarget, TagPolicy, TimeoutConfig,
    DEFAULT_COMMAND_TIMEOUT_SECS, DEFAULT_COMPACT_AT_TOKENS, DEFAULT_KUBE_REQUEST_TIMEOUT_SECS,
    DEFAULT_MAX_TOKENS, DEFAULT_RISK_DANGER_BELOW, DEFAULT_RISK_SCORER_TIMEOUT_SECS,
    DEFAULT_RISK_WARN_BELOW, MAX_KUBE_REQUEST_TIMEOUT_SECS, MAX_RISK_SCORER_TIMEOUT_SECS,
};
pub use error::{CoreError, Result};
pub use fleet_checks::{
    user_catalog_path, CheckSource, CommandForOs, CommandSpec, FleetCheck, FleetCheckCatalog,
    RejectedCheck, DEFAULT_COMMAND_KEY, USER_CATALOG_ENV, USER_CATALOG_FILE,
};
pub use fleet_file_check::{FileBaseline, FileProbe, FileReference};
pub use fleet_op::{
    FleetMember, FleetOperation, FleetTarget, HostHandle, HostProgress, KubePodRef, KubeScopeRef,
    OperationId,
};
pub use kube_pod::{PodLimits, PodRef, PodSelector};
pub use log_sources::{LogAuth, LogSourceConfig, LogSourceKind};
pub use os_family::{OsFamily, OS_RELEASE_COMMAND};
pub use secrets::{
    redact, redact_secrets, ssh_cred_name, ssh_key_passphrase_name, ssh_target_display_name,
    EnvSecretProvider, KeyringSecretProvider, SecretProvider, StaticSecretProvider,
};
pub use session::{
    default_base_dir, FleetSnapshot, KubePodSnapshot, KubeSessionSnapshot, ProfileUsage, Session, SessionMeta,
    SessionStore,
};
