# filar Engine API — Guide for External Consumers

filar can be used as a library (not just a TUI app) to embed an SSH-command-
executing AI agent in bots, mobile apps, or other frontends. This document
describes which crates to depend on, how to configure them, and a minimal
working example.

## Crates

| Crate            | Role                                           | Required? |
|------------------|------------------------------------------------|-----------|
| `filar-core`     | Shared types, config, errors, secrets, sessions | Yes       |
| `filar-transport`| `CommandExecutor` (SSH), `SecretSubstitutingExecutor` | Yes  |
| `filar-agent`    | `Agent`, `AgentBuilder`, `LlmClient` trait     | Yes       |

> **Note:** `filar-tui`, `filar-gui`, and `filar-app` are desktop-only and
> should NOT be used as dependencies by external consumers.

## Feature flags

### `filar-transport`

| Feature  | Default | What it enables                                   |
|----------|---------|---------------------------------------------------|
| `local`  | Yes     | `LocalExecutor`, `LocalInteractive` (requires `portable-pty`) |

Bots and mobile apps that only need SSH should disable default features:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.0", default-features = false }
```

Desktop apps (TUI/GUI) should keep `local` enabled:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.0" }
```

## Cargo.toml example

```toml
[dependencies]
filar-core      = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.0" }
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.0", default-features = false }
filar-agent     = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.0" }

tokio       = { version = "1", features = ["full"] }
async-trait = "0.1"
```

## Minimal example: build an agent and receive events

```rust,no_run
use std::sync::Arc;
use std::time::Duration;

use filar_agent::{AgentBuilder, AgentEvent, ChatMessage, EventSink};
use filar_core::{SshTarget, SshAuth, HostKeyPolicy, SecretProvider, StaticSecretProvider};
use filar_transport::SshExecutor;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // 1. Configure the SSH target.
    let target = SshTarget {
        name: "my-server".into(),
        host: "10.0.0.1".into(),
        port: 22,
        user: "admin".into(),
        auth: SshAuth::Password { password: Some("secret".into()) },
        host_key_policy: HostKeyPolicy::Tofu,
    };

    // 2. Connect via SSH.
    let executor = Arc::new(SshExecutor::connect(&target).await?);

    // 3. Provide an API key via SecretProvider.
    let secrets = Arc::new(StaticSecretProvider::new());
    secrets.insert("GLM_API_KEY", "your-api-key");

    // 4. Create a simple event sink that prints events.
    struct PrintSink;
    #[async_trait::async_trait]
    impl EventSink for PrintSink {
        async fn emit(&self, event: AgentEvent) {
            println!("{event:?}");
        }
    }

    // 5. Build the agent.
    let agent = AgentBuilder::new()
        .llm(Arc::new(filar_agent::GlmClient::new_with_provider(
            &filar_core::LlmConfig {
                api_base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
                model: "glm-4-flash".into(),
                ..Default::default()
            },
            Duration::from_secs(60),
            "GLM_API_KEY",
            &*secrets,
        )?))
        .executor(executor)
        .secret_provider(secrets)
        .ssh_mode("admin@10.0.0.1:22")
        .event_sink(Arc::new(PrintSink))
        .build()?;

    // 6. Run a single turn.
    agent
        .run(&[ChatMessage::user("Show me disk usage on /var")])
        .await?;

    Ok(())
}
```

## SessionStore

`SessionStore::new(base_dir)` accepts an explicit base directory, making it
suitable for platforms where `APPDATA`/`HOME` are not available (Android, iOS):

```rust
use filar_core::SessionStore;
let store = SessionStore::new(std::path::PathBuf::from("/data/data/com.example.app"))?;
```

For desktop platforms, use `SessionStore::with_default_dir()` which reads
`APPDATA` (Windows) or `HOME` (Unix).
