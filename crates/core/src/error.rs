//! Error types and unified `Result` alias for the workspace.

use thiserror::Error;

/// The top-level error type used across all crates.
///
/// Each variant maps to a distinct failure category. Crates may define
/// their own error types that convert into [`CoreError`] via `From`.
#[derive(Debug, Error)]
pub enum CoreError {
    /// Configuration file could not be read, parsed, or is missing required fields.
    #[error("config error: {0}")]
    Config(String),

    /// A required secret (e.g. `GLM_API_KEY`) was not found in the environment.
    #[error("missing secret: {0}")]
    Secret(String),

    /// I/O error (file system, network, etc.).
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    /// Generic error for cases that don't fit a more specific variant.
    #[error("{0}")]
    Other(String),
}

/// Convenience `Result` alias that defaults to [`CoreError`] as the error type.
pub type Result<T> = std::result::Result<T, CoreError>;
