# filar Engine API — Guide for External Consumers

filar can be used as a library (not just a TUI app) to embed an SSH-command-
executing AI agent in bots, mobile apps, or other frontends. This document
describes which crates to depend on, how to configure them, and a minimal
working example.

## Crates

| Crate            | Role                                           | Required? |
|------------------|------------------------------------------------|-----------|
| `filar-core`     | Shared types, config, errors, secrets, sessions | Yes       |
| `filar-transport`| `CommandExecutor` (SSH), `SecretSubstitutingExecutor`, `ReadOnlyExecutor` | Yes  |
| `filar-agent`    | `Agent`, `AgentBuilder`, `LlmClient` trait, `OutputPreprocessor` | Yes |

> **Note:** `filar-tui`, `filar-gui`, and `filar-app` are desktop-only and
> should NOT be used as dependencies by external consumers.

## Feature flags

### `filar-transport`

| Feature  | Default | What it enables                                   |
|----------|---------|---------------------------------------------------|
| `local`  | Yes     | `LocalExecutor`, `LocalInteractive` (requires `portable-pty`) |

Bots and mobile apps that only need SSH should disable default features:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v1.0.7", default-features = false }
```

Desktop apps (TUI/GUI) should keep `local` enabled:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v1.0.7" }
```

## Cargo.toml example

```toml
[dependencies]
filar-core      = { git = "https://github.com/devlawey/filar", tag = "engine-v1.0.7" }
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v1.0.7", default-features = false }
filar-agent     = { git = "https://github.com/devlawey/filar", tag = "engine-v1.0.7" }

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

    // 5. Build the agent. `OpenAiCompatClient` speaks the OpenAI-compatible
    //    chat/completions protocol (default endpoint: GLM). The deprecated
    //    `GlmClient` alias still works for existing consumers.
    let agent = AgentBuilder::new()
        .llm(Arc::new(filar_agent::OpenAiCompatClient::new_with_provider(
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

## Agent tools (including background jobs)

The agent exposes shell tools (`run_command`, `read_file`, `list_dir`) plus
background job tools for work that outlasts `[timeouts].command_secs`:

| Tool | Purpose |
|------|---------|
| `start_background_job` | Detach a command; returns `job_id` (+ pid when known) |
| `background_job_status` | Short poll for status/output (timeout applies to the poll only) |
| `cancel_background_job` | Stop a job by `job_id` |
| `list_background_jobs` | List jobs for the current session |

Embedders must set `AgentBuilder::session_id` (unique per tab/session) so job
state does not leak across concurrent agents. Set `AgentBuilder::is_local(true)`
for local executors (affects spawn strategy). Background jobs on SSH use
ephemeral `/tmp/filar-job-*` logs removed on completion; local jobs capture
output in memory only.

**Fleet mode.** `AgentBuilder::fleet(true)` marks an agent that works over a
group of hosts. Only `run_command` is offered: `read_file`, `list_dir` and the
background-job tools are left out of the tool set (the model never sees them),
a call to any of them is refused with an explanation instead of running, and
the system prompt says so. The same set is available as
`tools::fleet_tool_definitions(mode)`; `tools::SINGLE_HOST_TOOLS` names the
tools it drops.

A fleet agent also runs the **fleet gate** (`fleet_gate`, #435): a command on
the transport's read-only allowlist is always put to the confirmer — once, for
every host, even in modes that would auto-approve it — and anything else is
refused before the confirmer is called
(`CommandFinished { denied: true, output: fleet_gate::FLEET_WRITE_DENIED }`).
`fleet_gate::ApprovalScope` models the per-host approval a write would need;
that path is closed. The confirmer is not told the radius: a frontend knows its
own fleet (the TUI shows the group's frozen composition).

Build a fleet agent only over a group-level executor, never over one host's:
`fleet_exec::FleetExecutor::new(&operation, credentials, connector)?`
implements `CommandExecutor` for a `FleetOperation`. `credentials` is
`fleet_creds::FleetCredentials::resolve(&operation, &store)`: each host's own
secret, looked up once under `ssh_target:<name>` in `store` (never the shared
`SSH_PASSWORD`). A host without credentials gets no task, is never contacted,
is left out of `radius()` and is reported as `skipped`; credentials resolved
for another operation are refused. The connector receives each host's target
with its password already filled in — pass the transport an empty
`SecretProvider` so it cannot fall back to a shared one. An agent built with
`fleet(true)` ignores `secret_provider`: `$FILAR_SECRET_N` values are never
substituted into fleet commands. Each `run` refuses non-read-only
commands before any host is contacted, opens a fresh operation over the frozen
composition (`FleetOperation::reopen`), fans the command out under the group's
limits, and returns the summary headline plus the fold
(`FleetCheck::ad_hoc` — compared by digest, **no host output**). The
`HostConnector` you pass opens one host's executor and must wrap it in
`ReadOnlyExecutor`; a host it fails to connect is reported as "no contact" and
retried on the next `run`; so is a cached host whose connection is lost
during a `run`. `SshExecutor::cancel()` stops the running command on the
host: `SIGINT` to the shell's children through a short-lived `exec` channel
(the shell has no PTY, so a Ctrl-C byte would signal nothing), after which
the shell prints the command's marker and is ready for the next one.
`FleetExecutor::cancel()` during a `run` cancels the **operation**: queued
hosts are not started, running ones get a Ctrl-C on the host and are drained
(`fleet_run::run_on_fleet_until`), and `run` still returns the summary and fold
of what answered first, the rest as `HostState::Cancelled` (`HostRun::Cancelled`
in the report) — keep awaiting `run` after `cancel` to get it. An agent built
with `fleet(true)` does exactly that on its cancellation token: it shows the
partial result as the command's `CommandFinished`, then reports `Cancelled`,
and it applies no `command_timeout` to a fleet command (the per-host deadlines
bound it). `FleetExecutor::with_observer(observer)` hands the person-facing
`fleet_view::FleetView` of every operation (cancelled ones included) to
`observer`: groups of agreeing hosts with a sample answer (the first host's
output for a raw comparison, the compared rows for a typed one) and the hosts
without a value, with their `HostState`. It carries host output — route it to
your UI only, never into a tool result or the model's context; `run`'s own
result stays the fold. `FleetExecutor::built_from()` returns the id of
the operation it was built from; if you cache executors, reuse one only while
that id matches the fleet on screen.

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

`SshTransportConfig::command_timeout` (default 300s,
`filar_core::DEFAULT_COMMAND_TIMEOUT_SECS`) is how long `SshSession::run` waits
for the command marker. Override with
`SshTransportConfig::default().with_command_timeout(duration)`. Locally,
`LocalExecutor::with_timeout` is the equivalent (`LocalExecutor::new` uses the
same 300s default).

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

For desktop platforms, use `SessionStore::with_default_dir()` which uses
`dirs::data_dir()` (Windows `%APPDATA%`, macOS Application Support, Linux XDG).

## Chat history and compaction

A long session eventually fills the model's context window. The engine deals
with this by **compaction**: the head of the history is folded into a single
summary and the recent turns are kept verbatim. Three parts of the public API
are involved, and an embedder that ignores any of them will lose data silently
rather than loudly.

### `ChatBlock::Summary` — this one goes to the model

```rust
ChatBlock::Summary { text: String, replaced_blocks: usize }
```

`replaced_blocks` is how many blocks were folded into it — useful for a status
line, not otherwise load-bearing.

Flattening `ChatBlock`s into `ChatMessage`s is the embedder's job, and this is
where the mistake is easy to make. `ChatBlock::System` is chrome: connection
notices, error banners, anything the frontend wrote for the user rather than for
the model, and it is correct to drop it. `ChatBlock::Summary` looks like chrome
and is not. It **stands in for turns that are no longer in the history at all**,
so dropping it does not shorten the context — it erases the entire beginning of
the conversation without any error, and the model then answers as though the
session had just started.

A summary is best sent as a user-role message that says what it is, so the model
treats it as context rather than as something it said:

```rust
ChatBlock::Summary { text, .. } => Some(ChatMessage::user(
    format!("Summary of earlier turns in this session:\n{text}")
)),
```

If your match on `ChatBlock` is exhaustive, the compiler will point at this
variant when you upgrade. Do not reach for a `_ => None` arm to make it build.

### `summarise_history` — asking the model for the summary

```rust
pub async fn summarise_history(llm: &dyn LlmClient, transcript: &str) -> SummaryOutcome;

pub struct SummaryOutcome {
    pub usage: Option<TokenUsage>,
    pub summary: Result<String>,
}
```

The usage is returned separately from the summary because the two are owed to
different places. Whether the brief is usable decides only whether you fold the
head; the request was billed before anyone could judge it. So a reply the engine
rejects as too short still carries its `usage` back, and any per-session cost
accounting you keep should add it. `usage` is `None` when the provider reported
none, and when the call failed before a response existed — a real absence, not a
zero.

`summary` is `Err` both for transport failures and for a reply too short to be a
usable brief. Treat them the same way: leave the history alone and send the turn
on the full history. A failed summary must not cost the user their turn.

Pair it with the two helpers in `filar-core`:

```rust
let transcript = filar_core::transcript_for_summary(&blocks[..boundary]);
let outcome = filar_agent::summarise_history(llm.as_ref(), &transcript).await;
if let Ok(summary) = outcome.summary {
    let compacted = filar_core::compact_history(&blocks, boundary, &summary);
}
```

If you drive the summarising call from a task the user can cancel, guard it —
otherwise a cancelled fold keeps billing for a result you are going to discard.

### `generate_runbook` — folding a session into a procedure

```rust
pub async fn generate_runbook(llm: &dyn LlmClient, transcript: &str) -> RunbookOutcome;

pub struct RunbookOutcome {
    pub usage: Option<TokenUsage>,
    pub runbook: Result<String>,
}
```

Same shape as `summarise_history`, and for the same reason: the request is
billed before the reply can be judged, so `usage` comes back even when
`runbook` is `Err`. `Err` covers both transport failures and replies shorter
than `MIN_RUNBOOK_CHARS` (80) — treat them alike: write no document and leave
the caller's files alone.

Two differences matter when you build on it. The runbook is meant to be
*shared*, so run both the transcript you hand over and the reply, before
writing it, through `filar_core::redact_secrets(text, &provider)` — a
`$FILAR_SECRET_N` value must not reach the document through either door. And
the call carries no tool definitions: the model writing a procedure must not
be able to run anything, the same rule as the summary writer.

### `Session::folded_history` — where the folded turns live

```rust
pub struct Session {
    pub messages: Vec<ChatBlock>,        // the context: what the model is sent
    pub folded_history: Vec<ChatBlock>,  // the heads compaction folded away
    // ...
}
```

`compact_history` really does drop the head from the list it returns, so the two
fields mean different things and are not interchangeable:

- **`messages`** is the working context — what you send to the model and, in
  filar's own UI, what the feed shows.
- **`folded_history`** is every block compaction has removed, oldest first,
  appended to on each fold. It is never sent to the model.

Anything that claims to be a *record* of the session rather than a *view of its
context* must be built from both, in order:

```rust
let whole_conversation: Vec<ChatBlock> = session
    .folded_history
    .iter()
    .chain(session.messages.iter())
    .cloned()
    .collect();
```

Transcripts, exports, audit logs and anything you show a user as "the
conversation" belong in that category. Building them from `messages` alone
produces a record with a hole exactly where the beginning used to be — and the
hole appears only in sessions long enough to have been compacted, which is to
say the ones where it matters.

`folded_history` is `#[serde(default)]`, so sessions written before it existed
load with an empty archive.

## Output preprocessors — typed tables instead of raw text

Machine-format command output does not have to reach the model as raw text.
An `OutputPreprocessor` maps one command family's output to a typed table, so
aggregating code can compare rows and the model can receive a condensed
table instead of a dump:

```rust
pub trait OutputPreprocessor: Send + Sync {
    fn name(&self) -> &'static str;
    fn matches(&self, command: &str) -> bool;
    fn preprocess(&self, command: &str, output: &str)
        -> Result<PreprocessedOutput, PreprocessError>;
}

pub enum PreprocessOutcome {
    Structured { preprocessor: &'static str, table: PreprocessedOutput },
    Raw { reason: RawFallback }, // NoPreprocessor | Unparseable(PreprocessError)
}

let registry = filar_agent::PreprocessorRegistry::default(); // built-ins
match registry.preprocess(&command, &output) {
    filar_agent::PreprocessOutcome::Structured { table, .. } => { /* use the table */ }
    filar_agent::PreprocessOutcome::Raw { .. } => { /* send the output as before */ }
}
```

Two rules define an implementation. It is a **pure function of its
arguments** — no network, no filesystem, no environment — so it can run on
every command without side effects. And it **fails closed**: when the output
does not look exactly like the expected format (truncated, another locale,
another `df` flavour), it returns `Err` and the caller gets `Raw` — a wrong
table is worse than raw text. `PreprocessedOutput` is validated at
construction (one field per column), and neither method panics on arbitrary
input.

Register your own preprocessors with `register()`; claimants are consulted
in registration order and the first that claims the command and parses its
output wins. The built-in set (`with_builtins()`, the `Default`) is
registered first.

## Upgrading to `engine-v1.0.6`

Three changes to the public API. All three fail loudly at compile time except
the third, which is the one worth reading twice.

| Change | Breaks | What to do |
|--------|--------|------------|
| `ChatBlock::Summary` added | Exhaustive matches on `ChatBlock` | Send it to the model. See above — a `_ => None` arm here is a silent data loss |
| `summarise_history` returns `SummaryOutcome` instead of `Result<String>` | Direct callers | Match on `outcome.summary`; add `outcome.usage` to your cost accounting |
| `Session::folded_history` added | Struct literals constructing `Session` | `Vec::new()` for a fresh session. Build transcripts from it plus `messages` |

The first two stop the build. The third stops the build only if you construct
`Session` with a struct literal — if you deserialize sessions, it compiles and
runs, and your transcripts quietly lose the folded head.



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

## Using a local or third-party OpenAI-compatible model

`OpenAiCompatClient` is provider-agnostic: point `api_base_url` at any
OpenAI-compatible endpoint and supply the API key it expects. No code changes
are needed — only the config.

A local model served by [Ollama](https://ollama.com/) or LM Studio exposes an
OpenAI-compatible API at `http://localhost:11434/v1` (Ollama) or
`http://localhost:1234/v1` (LM Studio). Local servers usually do not check the
key, but filar requires a non-empty value — pass any placeholder:

```toml
[llm]
model = "llama3.1"
api_base_url = "http://localhost:11434/v1"
max_tokens = 4096
temperature = 0.3
```

```rust
use filar_core::LlmConfig;

let config = LlmConfig {
    model: "llama3.1".into(),
    api_base_url: "http://localhost:11434/v1".into(),
    max_tokens: 4096,
    temperature: Some(0.3),
    ..Default::default()
};
// Prefer a [[llm_profiles]] entry with key_env = "" (keyless); do not send a dummy key.
```

### Choosing the API key environment variable

By default the key is read from `GLM_API_KEY`. A profile can override this with
`key_env` so each provider uses its own variable.

**Empty `key_env`** means the profile is keyless (local / air-gapped servers):
no key is resolved and the HTTP client does not send `Authorization`.

```toml
[[llm_profiles]]
name = "ollama"
model = "llama3.1"
api_base_url = "http://localhost:11434/v1"
key_env = ""                    # keyless — no Authorization header
temperature = 0.3
```

```toml
[[llm_profiles]]
name = "local-with-dummy"
model = "llama3.1"
api_base_url = "http://localhost:11434/v1"
key_env = "OLLAMA_KEY"          # non-empty: key required (env / keyring)
temperature = 0.3
```

Select it at launch with `--llm ollama` (or `Config::select_llm(Some("ollama"))`).
