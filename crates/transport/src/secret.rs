//! `SecretSubstitutingExecutor` — wraps a [`CommandExecutor`] to handle
//! `$FILAR_SECRET_N` substitution and output sanitisation through a
//! [`SecretProvider`].
//!
//! Before executing a command, `$FILAR_SECRET_N` placeholders are replaced
//! with actual secret values from the provider. After execution, any
//! occurrence of a secret value in stdout/stderr is masked back to its
//! placeholder name, so the LLM never sees the real secret.

use std::sync::Arc;

use filar_core::{Result, SecretProvider};

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
        // Substitute secret placeholders with actual values.
        let names = self.provider.secret_names();
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

        let mut result = self.inner.run(&actual_command).await?;

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
