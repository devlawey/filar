//! `SecretSubstitutingExecutor` — wraps a [`CommandExecutor`] to handle
//! `$FILAR_SECRET_N` substitution and output sanitisation through a
//! [`SecretProvider`].
//!
//! Before executing a command, `$FILAR_SECRET_N` placeholders are replaced
//! with actual secret values from the provider. After execution, any
//! occurrence of a secret value in stdout/stderr is masked back to its
//! placeholder name, so the LLM never sees the real secret.

use std::sync::Arc;

use filar_core::{CoreError, Result, SecretProvider};

use crate::CommandExecutor;

// ---------------------------------------------------------------------------
// SecretSubstitutingExecutor
// ---------------------------------------------------------------------------

/// [`CommandExecutor`] wrapper that handles secret variable substitution
/// and output sanitisation via a [`SecretProvider`].
///
/// **Substitution:** before passing the command to the inner executor,
/// `$FILAR_SECRET_N` placeholders are replaced with actual values from
/// the provider.
///
/// **Sanitisation:** after execution, any occurrence of a secret value in
/// stdout/stderr is replaced back with its placeholder name, so the LLM
/// never sees the real secret.
pub struct SecretSubstitutingExecutor {
    inner: Arc<dyn CommandExecutor>,
    provider: Arc<dyn SecretProvider>,
}

impl SecretSubstitutingExecutor {
    /// Create a new `SecretSubstitutingExecutor` wrapping `inner` with the
    /// given secret `provider`.
    pub fn new(inner: Arc<dyn CommandExecutor>, provider: Arc<dyn SecretProvider>) -> Self {
        Self { inner, provider }
    }
}

#[async_trait::async_trait]
impl CommandExecutor for SecretSubstitutingExecutor {
    async fn run(&self, command: &str) -> Result<crate::CommandResult> {
        // Only substitute $-prefixed secret names (e.g. $FILAR_SECRET_N).
        // This prevents non-command secrets (like the LLM API key, which is
        // stored without a $ prefix) from being injected into shell commands.
        let mut names: Vec<String> = self
            .provider
            .secret_names()
            .into_iter()
            .filter(|n| n.starts_with('$'))
            .collect();
        // Sort by descending length to prevent substring collisions:
        // $FILAR_SECRET_1 is a substring of $FILAR_SECRET_10, so the longer
        // name must be processed first.
        names.sort_unstable_by_key(|n| std::cmp::Reverse(n.len()));

        let actual_command = {
            let mut cmd = command.to_string();
            for name in &names {
                if let Ok(value) = self.provider.get(name) {
                    if !value.is_empty() {
                        cmd = cmd.replace(name, &value);
                    }
                }
            }
            cmd
        };

        // Capture the result so we can sanitise the error path too —
        // error messages may embed the substituted command with real secrets.
        let run_result = self.inner.run(&actual_command).await;
        let mut result = match run_result {
            Ok(r) => r,
            Err(e) => {
                let mut msg = e.to_string();
                for name in &names {
                    if let Ok(value) = self.provider.get(name) {
                        if !value.is_empty() {
                            msg = msg.replace(&value, name);
                        }
                    }
                }
                // Preserve the error's classification through sanitisation —
                // `ConnectionLost` must survive so the transport's reconnect
                // logic (and `is_connection_lost`) still recognises it.
                return Err(match e {
                    CoreError::ConnectionLost(_) => CoreError::ConnectionLost(msg),
                    _ => CoreError::Other(msg),
                });
            }
        };

        // Sanitise output — replace any secret value with its placeholder.
        for name in &names {
            if let Ok(value) = self.provider.get(name) {
                if !value.is_empty() {
                    result.stdout = result.stdout.replace(&value, name);
                    result.stderr = result.stderr.replace(&value, name);
                }
            }
        }

        Ok(result)
    }

    async fn cancel(&self) -> Result<()> {
        self.inner.cancel().await
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use filar_core::{EnvSecretProvider, StaticSecretProvider};
    use std::sync::Mutex;

    /// A simple mock executor that records the last command and returns a
    /// canned result.
    struct MockExecutor {
        last_command: Mutex<String>,
        stdout: String,
        stderr: String,
    }

    impl MockExecutor {
        fn new(stdout: &str) -> Self {
            Self {
                last_command: Mutex::new(String::new()),
                stdout: stdout.to_string(),
                stderr: String::new(),
            }
        }
    }

    #[async_trait::async_trait]
    impl CommandExecutor for MockExecutor {
        async fn run(&self, command: &str) -> Result<crate::CommandResult> {
            *self.last_command.lock().unwrap() = command.to_string();
            Ok(crate::CommandResult {
                stdout: self.stdout.clone(),
                stderr: self.stderr.clone(),
                exit_code: Some(0),
                duration: std::time::Duration::from_millis(1),
            })
        }
        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn substitution_with_static_provider() {
        // Set up a StaticSecretProvider with a secret.
        let provider = Arc::new(StaticSecretProvider::new());
        provider.insert("$FILAR_SECRET_1", "s3cr3t-p@ss");
        let provider_trait: Arc<dyn SecretProvider> = provider;

        // The mock returns the substituted password in stdout (simulating
        // e.g. `echo $FILAR_SECRET_1` where the shell already expanded it).
        let mock = Arc::new(MockExecutor::new("user:s3cr3t-p@ss"));
        let exec = SecretSubstitutingExecutor::new(mock.clone(), provider_trait);

        let result = exec
            .run("echo user:$FILAR_SECRET_1")
            .await
            .unwrap();

        // The inner executor received the substituted command.
        assert_eq!(*mock.last_command.lock().unwrap(), "echo user:s3cr3t-p@ss");

        // The output is sanitised — the actual secret is replaced with the
        // placeholder name.
        assert_eq!(result.stdout, "user:$FILAR_SECRET_1");
    }

    #[tokio::test]
    async fn sanitisation_masks_secret_in_stderr() {
        let static_provider = StaticSecretProvider::new();
        static_provider.insert("$FILAR_SECRET_1", "my-secret");
        let provider: Arc<dyn SecretProvider> = Arc::new(static_provider);

        let mut mock = MockExecutor::new("");
        mock.stderr = "error: auth failed for my-secret".into();
        let mock = Arc::new(mock);

        let exec = SecretSubstitutingExecutor::new(mock, provider);
        let result = exec.run("some-cmd").await.unwrap();

        // stderr is sanitised.
        assert_eq!(result.stderr, "error: auth failed for $FILAR_SECRET_1");
    }

    #[tokio::test]
    async fn no_secrets_passes_command_through() {
        let provider: Arc<dyn SecretProvider> = Arc::new(StaticSecretProvider::new());
        let mock = Arc::new(MockExecutor::new("hello"));
        let exec = SecretSubstitutingExecutor::new(mock.clone(), provider);

        let result = exec.run("echo hello").await.unwrap();

        assert_eq!(*mock.last_command.lock().unwrap(), "echo hello");
        assert_eq!(result.stdout, "hello");
    }

    #[tokio::test]
    async fn dynamic_secret_insertion_visible_to_executor() {
        let provider = Arc::new(StaticSecretProvider::new());
        let provider_trait: Arc<dyn SecretProvider> = provider.clone();

        let mock = Arc::new(MockExecutor::new("pw=mypassword"));
        let exec = SecretSubstitutingExecutor::new(mock.clone(), provider_trait);

        // Insert a secret AFTER creating the executor — it should be visible
        // because StaticSecretProvider uses shared interior mutability.
        provider.insert("$FILAR_SECRET_2", "mypassword");

        let result = exec.run("echo pw=$FILAR_SECRET_2").await.unwrap();

        assert_eq!(*mock.last_command.lock().unwrap(), "echo pw=mypassword");
        assert_eq!(result.stdout, "pw=$FILAR_SECRET_2");
    }

    #[tokio::test]
    async fn multiple_secrets_substituted_and_sanitised() {
        let provider = Arc::new(StaticSecretProvider::new());
        provider.insert("$FILAR_SECRET_1", "alice");
        provider.insert("$FILAR_SECRET_2", "bob");
        let provider_trait: Arc<dyn SecretProvider> = provider;

        // Output contains both secrets.
        let mock = Arc::new(MockExecutor::new("alice and bob are here"));
        let exec = SecretSubstitutingExecutor::new(mock.clone(), provider_trait);

        let result = exec
            .run("echo $FILAR_SECRET_1 and $FILAR_SECRET_2")
            .await
            .unwrap();

        assert_eq!(*mock.last_command.lock().unwrap(), "echo alice and bob");
        assert_eq!(result.stdout, "$FILAR_SECRET_1 and $FILAR_SECRET_2 are here");
    }

    /// A mock executor that always returns an error containing the
    /// command it received (simulating a shell error that embeds the
    /// command line).
    struct FailingMockExecutor {
        last_command: Mutex<String>,
    }

    #[async_trait::async_trait]
    impl CommandExecutor for FailingMockExecutor {
        async fn run(&self, command: &str) -> Result<crate::CommandResult> {
            *self.last_command.lock().unwrap() = command.to_string();
            Err(filar_core::CoreError::Other(format!(
                "command failed: {command}"
            )))
        }
        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn error_path_is_sanitised() {
        let provider = Arc::new(StaticSecretProvider::new());
        provider.insert("$FILAR_SECRET_1", "s3cr3t");
        let provider_trait: Arc<dyn SecretProvider> = provider;

        let mock = Arc::new(FailingMockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let exec = SecretSubstitutingExecutor::new(mock, provider_trait);

        let result = exec.run("echo $FILAR_SECRET_1").await;

        assert!(result.is_err());
        let err_msg = result.unwrap_err().to_string();
        assert!(
            !err_msg.contains("s3cr3t"),
            "secret leaked in error: {err_msg}"
        );
        assert!(
            err_msg.contains("$FILAR_SECRET_1"),
            "placeholder missing in error: {err_msg}"
        );
    }

    #[tokio::test]
    async fn substring_collision_with_10_plus_secrets() {
        let provider = Arc::new(StaticSecretProvider::new());
        provider.insert("$FILAR_SECRET_1", "one");
        provider.insert("$FILAR_SECRET_10", "ten");
        let provider_trait: Arc<dyn SecretProvider> = provider;

        let mock = Arc::new(MockExecutor::new("one and ten"));
        let exec = SecretSubstitutingExecutor::new(mock.clone(), provider_trait);

        let result = exec
            .run("echo $FILAR_SECRET_1 and $FILAR_SECRET_10")
            .await
            .unwrap();

        // Both secrets should be correctly substituted in the command.
        assert_eq!(
            *mock.last_command.lock().unwrap(),
            "echo one and ten"
        );
        // Both should be correctly masked in the output.
        assert_eq!(
            result.stdout,
            "$FILAR_SECRET_1 and $FILAR_SECRET_10"
        );
    }

    #[tokio::test]
    async fn api_key_not_substituted() {
        let provider = Arc::new(StaticSecretProvider::new());
        provider.insert("GLM_API_KEY", "sk-1234567890");
        provider.insert("$FILAR_SECRET_1", "runtime-secret");
        let provider_trait: Arc<dyn SecretProvider> = provider;

        let mock = Arc::new(MockExecutor::new(""));
        let exec = SecretSubstitutingExecutor::new(mock.clone(), provider_trait);

        let _ = exec.run("echo GLM_API_KEY").await.unwrap();

        // GLM_API_KEY is NOT substituted because it lacks the $ prefix.
        assert_eq!(
            *mock.last_command.lock().unwrap(),
            "echo GLM_API_KEY"
        );
    }

    #[tokio::test]
    async fn substitution_with_env_provider() {
        // Set up an env var that EnvSecretProvider will discover.
        std::env::set_var("FILAR_SECRET_TESTENV", "env-pw-42");
        let provider: Arc<dyn SecretProvider> = Arc::new(EnvSecretProvider::new());

        let mock = Arc::new(MockExecutor::new("auth=env-pw-42"));
        let exec = SecretSubstitutingExecutor::new(mock.clone(), provider);

        let result = exec
            .run("echo auth=$FILAR_SECRET_TESTENV")
            .await
            .unwrap();

        // The inner executor received the substituted command.
        assert_eq!(*mock.last_command.lock().unwrap(), "echo auth=env-pw-42");

        // The output is sanitised.
        assert_eq!(result.stdout, "auth=$FILAR_SECRET_TESTENV");

        std::env::remove_var("FILAR_SECRET_TESTENV");
    }
}
