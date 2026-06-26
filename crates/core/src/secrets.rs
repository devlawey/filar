//! Reading secrets from the environment.
//!
//! Secrets are **never** hardcoded. They are always read from environment
//! variables at runtime. The [`secrets`] module centralises this logic so
//! that no other part of the codebase needs to know variable names.

use crate::error::{CoreError, Result};

/// Environment variable names for secrets.
pub mod env_vars {
    /// API key for the GLM LLM service.
    pub const GLM_API_KEY: &str = "GLM_API_KEY";
}

/// Retrieve the GLM API key from the environment.
///
/// Returns [`CoreError::Secret`] if the variable is unset or empty.
pub fn glm_api_key() -> Result<String> {
    read_secret(env_vars::GLM_API_KEY)
}

/// Retrieve an API key from a named environment variable.
///
/// This is a generalisation of [`glm_api_key`] that allows different
/// LLM profiles to use different environment variables.
pub fn api_key(env_var: &str) -> Result<String> {
    read_secret(env_var)
}

/// Generic helper: read a non-empty secret from the environment.
fn read_secret(var_name: &str) -> Result<String> {
    std::env::var(var_name)
        .ok()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| CoreError::Secret(format!("{var_name} not set or empty")))
}
