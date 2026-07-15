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
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.1", default-features = false }
```

Desktop apps (TUI/GUI) should keep `local` enabled:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.1" }
```

## Cargo.toml example

```toml
[dependencies]
filar-core      = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.1" }
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.1", default-features = false }
filar-agent     = { git = "https://github.com/devlawey/filar", tag = "engine-v0.3.1" }

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

## SSH credentials (password auth)

For `SshAuth::Password`, the SSH password is resolved in this order:

1. **Explicit password on the target** — `SshAuth::Password { password: Some(..) }`.
   Recommended for bots, mobile, and other embeddings that already hold the
   credential.
2. **Your `SecretProvider`** — if the password is `None`, the transport looks up
   the logical name `"SSH_PASSWORD"` via the provider you pass to
   `SshExecutor::connect_with_provider` (or `SshInteractive::connect_with_provider`).

The transport itself **never reads environment variables** for the password. The
`SSH_PASSWORD` env-var fallback is simply the behaviour of the default
`EnvSecretProvider`, which is what the convenience constructors
(`SshExecutor::connect`, `SshInteractive::connect`) use — so TUI/desktop keep
reading `SSH_PASSWORD` from the environment, while external consumers whose env is
not a secret source are not trapped by it.

```rust,no_run
use std::sync::Arc;
use filar_core::{SshTarget, SshAuth, HostKeyPolicy, StaticSecretProvider};
use filar_transport::{SshExecutor, SshTransportConfig};

# async fn example(target: SshTarget) -> Result<(), Box<dyn std::error::Error>> {
// Option A — inject the password explicitly on the target.
let target_a = SshTarget {
    auth: SshAuth::Password { password: Some("s3cret".into()) },
    ..target.clone()
};
let exec_a = SshExecutor::connect(&target_a).await?;

// Option B — supply it through your own SecretProvider under "SSH_PASSWORD".
let secrets = Arc::new(StaticSecretProvider::new());
secrets.insert("SSH_PASSWORD", "s3cret");
let target_b = SshTarget { auth: SshAuth::Password { password: None }, ..target };
let exec_b =
    SshExecutor::connect_with_provider(&target_b, SshTransportConfig::default(), secrets).await?;
# let _ = (exec_a, exec_b);
# Ok(())
# }
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

## LLM request parameters

`LlmConfig` supports optional parameters that are sent in the API request body:

| Field | Type | Range | Default |
|-------|------|-------|---------|
| `temperature` | `Option<f32>` | [0.0, 2.0] | `None` (provider default) |
| `top_p` | `Option<f32>` | (0.0, 1.0] | `None` (provider default) |
| `extra_body` | `Option<serde_json::Value>` | JSON object; non-objects are ignored | `None` |

All fields default to `None` — without them, the request body is byte-for-byte
identical to previous versions (backward compatible).

### extra_body merge rules

`extra_body` is merged into the JSON request body **after** serializing the base
fields. Only JSON objects are merged; non-object values are ignored with a
`warn!` log. Protected keys (`model`, `messages`, `tools`, `stream`) are also
ignored with a `warn!` log and cannot be overridden via `extra_body`. All
other keys (including `max_tokens`, `temperature`, `top_p`) are inserted or
overridden.

### Config example

```toml
[llm]
model = "glm-5.2"
api_base_url = "https://open.bigmodel.cn/api/paas/v4"
max_tokens = 4096
temperature = 0.3
top_p = 0.9
[llm.extra_body]
thinking = { type = "disabled" }
```

### Provider-specific examples

- **GLM** (`thinking`): `{ "thinking": { "type": "disabled" } }`
- **OpenAI-compatible** (`reasoning_effort`): `{ "reasoning_effort": "low" }`
- **Ollama** (`options.num_ctx`): `{ "options": { "num_ctx": 8192 } }`

### Code example

```rust
use filar_core::LlmConfig;

let config = LlmConfig {
    model: "glm-5.2".into(),
    api_base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
    max_tokens: 4096,
    temperature: Some(0.3),
    top_p: None,
    extra_body: Some(serde_json::json!({ "thinking": { "type": "disabled" } })),
};
```
