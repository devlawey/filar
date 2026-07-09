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
use std::sync::{Arc, RwLock};

use zeroize::Zeroize;

use crate::error::{CoreError, Result};

/// Environment variable names for secrets.
pub mod env_vars {
    /// API key for the GLM LLM service.
    pub const GLM_API_KEY: &str = "GLM_API_KEY";
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
    /// Retrieve a secret by its logical name (e.g. `"GLM_API_KEY"`,
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
/// This is the default for TUI/desktop: `GLM_API_KEY` and `FILAR_SECRET_*`
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
            .ok_or_else(|| CoreError::Secret(format!("{name} not set or empty")))
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
            .ok_or_else(|| CoreError::Secret(format!("{name} not set or empty")))
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
// Legacy convenience functions (delegates to EnvSecretProvider)
// ---------------------------------------------------------------------------

/// Retrieve the GLM API key from the environment.
///
/// Returns [`CoreError::Secret`] if the variable is unset or empty.
///
/// **Deprecated:** prefer [`SecretProvider::get`] with [`EnvSecretProvider`].
pub fn glm_api_key() -> Result<String> {
    EnvSecretProvider::new().get(env_vars::GLM_API_KEY)
}

/// Retrieve an API key from a named environment variable.
///
/// **Deprecated:** prefer [`SecretProvider::get`] with [`EnvSecretProvider`].
pub fn api_key(env_var: &str) -> Result<String> {
    EnvSecretProvider::new().get(env_var)
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
}
