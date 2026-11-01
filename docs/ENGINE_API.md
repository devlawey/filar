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
| `kube`   | Yes     | Read-only `ClusterApi` / `KubeClusterApi` and the pod shell `KubeExecutor` (requires `kube-rs`) |

Bots and mobile apps that only need SSH should disable default features:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v3.0.0", default-features = false }
```

Desktop apps (TUI/GUI) should keep `local` enabled:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v3.0.0" }
```

Consumers that need Kubernetes but not a local terminal can enable only `kube`:

```toml
filar-transport = { git = "https://github.com/devlawey/filar", default-features = false, features = ["kube"] }
```

## Read-only Kubernetes API

With the `kube` feature, `KubeClusterApi::connect(&KubeCluster)` implements
`ClusterApi`. It reads pods, events, bounded logs, workloads, rollout history,
nodes, metrics, Services, Endpoints, Ingresses, ConfigMaps and namespaces. The
public result types do not expose `kube-rs`; `CommandExecutor` is unchanged.
There are no write methods and no method can read a Kubernetes `Secret`.

The connection always uses the context named by the target, never the
kubeconfig's `current-context`. Lists return at most 500 objects and do not
silently follow continuation tokens. Log calls require both a line and a byte
limit. If a cluster-wide list is denied by namespace-scoped RBAC, reads fall
back only to namespaces explicitly configured on the target. Absent
`metrics.k8s.io` support is reported as `Unsupported`; RBAC, policy and
not-found errors are sanitized so credentials and kubeconfig contents do not
leak. ConfigMap values still require the consumer's output-redaction policy.

## Pod shell — `KubeExecutor` (#563)

With the `kube` feature, `KubeExecutor` is another `CommandExecutor`: a
persistent shell inside a pod's container, reached through the API `exec`
subresource. The trait is unchanged; everything that works on an SSH
executor (`run`, `cancel`, `set_cwd`, `current_cwd`) works here.

```rust,ignore
use filar_core::{KubeCluster, PodRef};
use filar_transport::{CommandExecutor, KubeExecutor};

# async fn example(cluster: KubeCluster) -> filar_core::Result<()> {
let target = PodRef::by_selector(&cluster.name, "shop", "app=web", Some("main"));
let exec = KubeExecutor::connect(&cluster, &target).await?;
let r = exec.run("cd /tmp && df -h .").await?;
println!("{} — {}", exec.describe(), r.stdout);
# Ok(()) }
```

- **Target.** `PodRef { cluster, namespace, pod, container }` (in
  `filar-core`): `pod` is `PodSelector::Name` or `PodSelector::Labels`. A
  selector picks the first `Running` + `Ready` pod by name. With no
  container, the only container is used, else the
  `kubectl.kubernetes.io/default-container` annotation; otherwise the error
  lists the containers. `KubeCluster::describe_pod` gives the agent-facing line.
- **Shell.** One `exec` of `/bin/sh` (`KubeExecConfig::shell`) with stdin and
  no TTY, driven by the same marker protocol as SSH, so `cd` and `export`
  persist. A container without the shell fails with a message pointing to an
  ephemeral debug container.
- **Cancel.** A second `exec` signals the shell's children (POSIX `ps`, `awk`,
  `kill`). If that cannot be confirmed the stream is torn down and a new shell
  opened; `cancel` then returns `SHELL_RECREATED_MESSAGE` (cd and variables
  are reset) — it never silently fails.
- **Reconnect.** A stream that closed **before** a command was sent is
  reopened once (`KubeExecConfig::auto_reconnect`). With a selector, a
  vanished pod is replaced by another ready one; the executor emits
  `KubeExecEvent::PodChanged` (`subscribe()`) and `take_pod_change()` returns
  the pair. A command that may have started is never retried.
- **Limits.** `pod_limits()` returns `PodLimits` read from the already-fetched
  pod spec: memory/CPU limits and requests of the container, effective
  `runAsUser` / `runAsNonRoot`, `readOnlyRootFilesystem`, dropped
  capabilities. Nothing is run or measured in the container to obtain them.
  `KubeExecEvent::Connected` carries them on every **re**connect; the first
  one is sent inside `connect()`, before any `subscribe()` can exist, so read
  `pod()`, `container()` and `pod_limits()` right after `connect()`.
- **Denials.** A 403 from RBAC names the verb and resource
  (`нет права create на pods/exec в namespace shop (контекст prod-eu)`); one
  from an admission policy shows the policy's text
  (`отказала политика допуска: …`). A denial is never worked around. Because
  kube-rs drops the API's status text when it refuses the WebSocket upgrade,
  a refused exec is repeated once as a plain, non-upgrading `POST` to read
  that text; the API authorizes and admits it the same way and then refuses
  to upgrade, so nothing runs — but the probe can appear in API audit logs and
  admission webhooks.
  Errors carry the context and namespace, never credentials.
- **No Kubernetes resource is created or modified** by the executor, and it
  never creates ephemeral containers. Commands sent to the shell can, of
  course, change the container's files and processes — as over SSH, that is
  what the confirmation gate is for.

The exec stream uses a client without read/write timeouts (an idle shell is
normal); API reads keep the cluster's `request_timeout_secs`.

## Kubernetes output redaction

`filar_agent::KubeToolOutput::redact_for_model` is the boundary between
Kubernetes tool output and model messages, session history or exports. Structured
objects lose `metadata.managedFields` and the last-applied-configuration
annotation, and ordinary strings also pass through known-secret redaction.
Literal environment values become `«скрыто»`; `valueFrom` and `envFrom` retain
only source references. Container commands and arguments mask inline and
separate values of secret-like `--` flags and sensitive `IDENT=value`
assignments, plus URL userinfo (with or without a colon). ConfigMap keys stay
visible, while secret-like keys, sensitive assignments in multiline YAML,
`.properties` and `.env` values, credentials in embedded JSON,
credential-bearing URLs, high-entropy values and values of at least 256
characters are hidden. Binary ConfigMap payloads are always hidden.

Log output first uses `filar_core::redact_secrets`, then masks bearer tokens and
URL userinfo. `KubeCluster::reveal_configmaps` (`namespace/name`) and
`KubeCluster::reveal_env` (`namespace/pod/VARIABLE`, Pod objects only) are
narrow, explicit exceptions; revealed values carry the visible
`«раскрыто настройкой»` marker. No exception enables Kubernetes Secret reads.

`From<ConfigMapInfo>` and `From<PodLogs>` adapt the corresponding `ClusterApi`
results to this boundary. The transport remains independent of the agent's
`SecretProvider`. Embedders that construct their own model messages from
`ClusterApi` results must apply this boundary rather than serializing raw
objects or logs directly.

## Read-only Kubernetes agent tools

`filar_agent::kube_tool_definitions(mode)` returns the six cluster-tab tools:
`k8s_get`, `k8s_describe`, `k8s_logs`, `k8s_events`, `k8s_top` and
`k8s_rollout_history`. They accept only the fixed resource list exposed by
`KubeResourceKind`; there is no Secret kind and no mutation tool. Parse a model
call with `parse_kube_tool_call`, apply `kube_confirmation_decision` and show
`KubeToolCall::confirmation_context` when approval is required, then pass the
validated call to `KubeToolExecutor::execute`.

The executor is a separate engine boundary from the SSH `Agent` and does not
change `CommandExecutor` or `LlmClient`. It renders list-like results through
`PreprocessedOutput`, limits tables to 100 rows and model output to 10,000
characters, and adds explicit truncation markers. Logs accept at most 500 lines
and 1 MiB per call. Every object, event, ConfigMap and log passes through
`KubeToolOutput::redact_for_model` before the returned `KubeToolResult` can be
added to model history. Missing metrics support is reported as
`metrics-server не установлен`.

`build_kube_system_prompt` describes this read-only tool surface, including
previous-container logs after restarts and resource limits for OOMKilled
analysis. `AgentBuilder::arbiter_kube_context` and
`arbiter::KubernetesArbiterTarget` provide the same cluster, namespace and tags
to arbiter and `RiskInput` target descriptions. `ContainerInfo` also exposes
resource requests and limits for pod diagnostics.

For a shell inside a pod (`KubeExecutor`, #563), `AgentBuilder::kube_pod_mode`
takes an `arbiter::KubePodTarget` (cluster, context, namespace, pod, container,
tags and `PodLimits` from `KubeExecutor::pod_limits`). It sets the pod system
prompt — the container is ephemeral, packages are not installed, `kubectl` is
probably absent, the shell persists, the known limits as facts and the rule
that the agent's processes share the container's memory limit with the
application — and the arbiter/`RiskInput` target
`Kubernetes pod <ns>/<pod> (container <c>, cluster <name>, tags: …); limits: non-root, read-only fs, memory limit 512Mi`
(unknown limits are left out). The confirmation gate is unchanged.

## Cargo.toml example

```toml
[dependencies]
filar-core      = { git = "https://github.com/devlawey/filar", tag = "engine-v3.0.0" }
filar-transport = { git = "https://github.com/devlawey/filar", tag = "engine-v3.0.0", default-features = false }
filar-agent     = { git = "https://github.com/devlawey/filar", tag = "engine-v3.0.0" }

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
    secrets.insert("FILAR_LLM_API_KEY", "your-api-key");

    // 4. Create a simple event sink that prints events.
    struct PrintSink;
    #[async_trait::async_trait]
    impl EventSink for PrintSink {
        async fn emit(&self, event: AgentEvent) {
            println!("{event:?}");
        }
    }

    // 5. Build the agent. `OpenAiCompatClient` speaks the OpenAI-compatible
    //    chat/completions protocol with any provider.
    let agent = AgentBuilder::new()
        .llm(Arc::new(filar_agent::OpenAiCompatClient::new_with_provider(
            &filar_core::LlmConfig {
                api_base_url: "https://api.deepseek.com/v1".into(),
                model: "deepseek-chat".into(),
                ..Default::default()
            },
            Duration::from_secs(60),
            "FILAR_LLM_API_KEY",
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
for local executors (affects spawn strategy). Background jobs on SSH and in
pods use an ephemeral `filar-job-*` log in the first writable directory of
`$TMPDIR`, `/tmp`, `/dev/shm` (probed once per session), removed on completion;
with no writable directory `start_background_job` refuses. Call
`background::reset_log_dir(session_id)` when a tab's target changes (reconnect,
transport swap, pod replaced) so the next job probes again. Local jobs capture
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
`fleet_creds::FleetCredentials::resolve(&operation, &store)`: each SSH host's
own secret, looked up once under `ssh_target:<name>` in `store` (never the
shared `SSH_PASSWORD`). An SSH host without credentials gets no task, is never
contacted, is left out of `radius()` and is reported as `skipped`; credentials
resolved for another operation are refused. Kubernetes members never consult
the SSH credential store and are not skipped for missing SSH credentials. The
connector receives a `FleetTarget`; an SSH target has its password already
filled in — pass the transport an empty `SecretProvider` so it cannot fall back
to a shared one. An agent built with
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
result stays the fold. `FleetExecutor::with_status(status)` reports
`fleet_view::FleetStatus` — how many hosts answered out of the fleet's
members, and whether the operation still runs — as the operation starts
(`0`), each time a host answers, and when it ends; it carries no host output.
The per-host hook underneath is `fleet_run::run_on_fleet_observed`, which
calls `on_host` as each host finishes (`run_on_fleet_until` is it with a
no-op hook). `FleetExecutor::built_from()` returns the id of
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

### Encrypted keys (`SshAuth::Key`)

An encrypted key's passphrase is taken from `SshAuth::Key { passphrase: Some(..) }`,
else from your `SecretProvider` under `"SSH_KEY_PASSPHRASE"`; with neither, the
connect fails with `… is encrypted and no passphrase was given`, and with a wrong
one with `wrong passphrase for SSH key …` — neither error carries the passphrase.
`filar_transport::fill_key_passphrase` (store, then optional env provider,
each checked by decrypting), `key_protection` and `key_passphrase_matches` let you
resolve or verify a passphrase locally before connecting; they are blocking.

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

## Fleet mode — one question, many hosts (2.0.0)

Fleet mode asks one **read-only** question of a group of hosts and hands the
model a comparison of the answers, never the answers themselves. Everything
below is additive except where the upgrade table says otherwise.

### Contract decision: no trait changed

`CommandExecutor` and `LlmClient` are unchanged. The fleet is built *on* the
existing traits rather than by widening them:

- **`filar_transport::ReadOnlyExecutor`** (#419) is a `CommandExecutor` that
  wraps another one and refuses, before anything reaches the host, every
  command outside a compiled-in allowlist of pure readers (and any shell
  construct that could smuggle another one in). `check_read_only(command)`
  is the same check as a function. The allowlist is not configurable — it is
  a security boundary, not a default.
- **`filar_agent::fleet_exec::FleetExecutor`** (#435) is a `CommandExecutor`
  over a whole fleet: one `run(command)` fans the command out, classifies
  every host and returns the summary headline plus a folded difference
  table. `cancel()` stops the whole operation (#437). The agent loop sees one
  executor, so nothing about groups leaks into it; `AgentBuilder::fleet(true)`
  switches the agent to the fleet tool set and prompt.

### The pieces

| Item | Crate | What it is |
|------|-------|------------|
| `FleetOperation` (`open`, `reopen`, `restore`) | `filar-core` | The composition: which hosts, under which group limits, frozen when the operation opens (#426). `restore` rebuilds it from saved host names, not from the tag rule (#442) |
| `FleetCheckCatalog`, `FleetCheck`, `CommandSpec` | `filar-core` | Declarative, human-authored read-only checks, built-in plus `fleet_checks.toml`, with per-OS command variants (#424, #425) |
| `FileBaseline`, `FileReference`, `FileProbe` | `filar-core` | "File against a reference" by SHA-256 — the content is never read (#440) |
| `fleet_run::run_on_fleet[_until\|_observed]` | `filar-agent` | Fan-out under `max_parallel`, a deadline per host, cancellation (#427, #437) |
| `fleet_result::{OperationResult, HostState}` | `filar-agent` | Every host's state — answered, silent, not asked, cancelled (#428) |
| `fleet_fold::fold`, `fleet_file_fold::fold_file` | `filar-agent` | The only text about an operation the model gets: groups of agreeing hosts by typed rows or digests (#430); for a file check, verdicts against the reference (#440) |
| `fleet_creds::FleetCredentials` | `filar-agent` | One host's own credentials each; a host without them drops out (#436) |
| `fleet_view::FleetView` | `filar-agent` | The person-facing aggregate with sample answers — for a UI, never the model (#438) |
| `FleetIdentifiers`, `generate_fleet_runbook` | `filar-agent` | A fleet session's runbook: a procedure for the group, with host names, addresses and accounts scrubbed in code (#443) |

### Containment is the design

Host output does not reach the model as raw text: under a typed comparison
only parsed cells are carried (escaped and length-clamped), under a raw one
only a digest. That narrows what one compromised host can say, but does not
silence it: the parsed cells are still part of the tool result, and the model
may base its next read-only command on them. What such a host cannot do is
turn that into a write or a wider fleet — every command passes the read-only
policy before fan-out, and the composition is frozen before the first command,
so no host output can widen it. Keep these properties if you build your own
fleet front end: hand the model `fold(..)` / `fold_file(..)`, and route
`FleetView` to people only.

### Persisted fleet sessions

`Session::fleet: Option<FleetSnapshot>` (`#[serde(default)]`) marks a saved
fleet dialogue and carries its group and member target identities in order
(#442, #567). Legacy `hosts: ["name", ...]` arrays still load as SSH targets.
New `targets` entries persist only SSH names or Kubernetes
cluster/namespace/pod/container identities — no addresses, usernames,
credentials or kubeconfig contents. Restore with `FleetOperation::restore`
against current `FleetTarget` values; identities it cannot find come back
separately, so you can say that the restored fleet is smaller than the saved
one.

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

## File transfer — `FileTransfer` (2.0.5)

`filar_transport::FileTransfer` is the filesystem of one side — the local disk
or an SSH host — as the file panels (Ctrl+G) and the copy engine see it. It is
a **new, separate trait**: `CommandExecutor` is unchanged, and transferring
files never goes through it. The change is additive; nothing to migrate.

| Item | What it is |
|------|------------|
| `FileTransfer` | `path_style`, `home_dir`, `list_dir`, `stat` (follows symlinks, `Ok(None)` if missing), `open_read` / `open_write` (boxed tokio `AsyncRead` / `AsyncWrite`; `open_write` is exclusive — an existing file or a symlink at the path is `AlreadyExists`, never followed; `shutdown` the writer, drop it to close), `rename` (replaces the target), `remove_file`, `create_dir`, `set_permissions` (no-op without POSIX modes) |
| `SftpFileTransfer` | An SSH host over the standard `sftp` subsystem of sshd — nothing installed remotely. `SshExecutor::open_sftp()` opens it on the executor's existing connection (the shell, its cwd and a running command are untouched); `SftpFileTransfer::connect(target, cfg, secrets)` opens a connection of its own with the same host-key check and secret sources. `close()` closes the channel, and the connection only when it owns one. A server without the subsystem fails fast with an error naming it. `rename` over an existing file: SFTPv3 has no replacing rename, so on that refusal (both ends files) the old file steps aside as `<name>.filar-old-<id>` (`sftp::OLD_SUFFIX` + a short unique id, so a copy left by a failed cleanup never blocks the next replacement), the new one takes the name, the old one is removed — or put back if taking the name fails |
| `LocalFileTransfer` | The client's own disk. Always compiled (no `local` feature needed). On Windows, `list_dir("")` lists the drives |
| `DirEntry`, `Metadata`, `EntryKind` | Listing entries: kind (`File`/`Dir`/`Symlink`/`Other`, symlinks not followed in listings), size, mtime, POSIX mode |
| `take_leftovers()` | Default method: files a side had to leave behind (an SFTP replace whose old copy could not be removed). The copy engine adds them to `CopyReport::leftover_parts`; a UI must show them |
| `PathStyle` | `Posix` (every SSH host) or `Windows`; `join`, `parent`, `file_name` on path strings. Windows: the parent of `C:\` is `""`, the virtual drive list |

### Copy engine — `copy()` (#534)

`filar_transport::copy(src, sources, dst, dst_dir, observer, cancel)` copies
the picked `CopySource`s (path + kind as listed) from one `FileTransfer` into
a directory of another — local↔host and host↔host alike. Bytes stream from the
source reader into the target writer through the client's memory; nothing is
staged on any disk.

| Rule | Behaviour |
|------|-----------|
| Atomic replace | Each file is written to `<name>.filar-part` (`PART_SUFFIX`) next to the target and renamed over it when complete. Whatever already sits under the part's name (an old part, a planted symlink) is removed first and the part is created exclusively, so bytes never land anywhere else; the writer is closed before the rename |
| Existing target | `CopyObserver::decide_overwrite` decides: `Overwrite`, `Skip`, `OverwriteAll`, `SkipAll`, `Cancel`. Never replaced silently |
| Directories | Copied recursively; an existing target directory is reused, a file in its place fails that subtree only |
| Symlinks | To a file: copied as content. To a directory: skipped with a warning (no loops). Broken: warning |
| Permissions | Mode bits travel (best effort), owners do not |
| Errors | One file failing does not stop the rest; `CopyReport::failed` lists `(path, reason)` |
| Cancel | The `CancellationToken` stops at the next chunk; the part file is removed. A part that cannot be removed (lost connection) is listed in `CopyReport::leftover_parts` — show these paths to the user |

Progress arrives as `CopyEvent`s: `Planned { files, bytes }` first, then
`FileStarted` / `Progress { done, total }` / `FileDone` / `FileSkipped` /
`Failed` / `Warning`.

Errors keep the `io::ErrorKind` (`NotFound`, `PermissionDenied`, …) inside
`CoreError::Io`, with the operation and path in the message, so a UI can tell
"no such file" from "no access".

Remote writes are allowed only on an explicit user action (AGENTS.md,
invariant 1 and its #531 exception); an embedder must not let the model drive
`open_write`, `rename` or `remove_file` on a remote side.

## Command safety rating — `RiskScorer` (2.0.7)

An embedder can have every command the agent proposes rated by a separate
*decision model*: a model that does not write text but returns a probability
distribution over the levels of an ordered scale (e.g. `typesafe/jev-1.14` on
OpenRouter). The rating is `safety_pct` 0–100, where **100 = completely
safe**. It is advisory only: it never approves, denies or delays a command,
and it is independent of the arbiter.

```rust,no_run
# use std::sync::Arc;
# use filar_agent::{AgentBuilder, build_risk_scorer};
# use filar_core::{RiskScorerConfig, StaticSecretProvider};
# fn example(builder: AgentBuilder) -> filar_core::Result<()> {
let secrets = StaticSecretProvider::new();
secrets.insert("FILAR_RISK_API_KEY", "your-openrouter-key");
let scorer = build_risk_scorer(
    &RiskScorerConfig {
        kind: "decisions".into(),
        base_url: "https://openrouter.ai/api/alpha".into(),
        model: "typesafe/jev-1.14".into(),
        key_env: "FILAR_RISK_API_KEY".into(), // "" = keyless local server
        timeout_secs: 8,
        ..Default::default()
    },
    &secrets,
)?;
let _agent = builder.risk_scorer(Some(scorer));
# Ok(()) }
```

- `RiskScorer` (trait, `filar_agent::risk`) — `score(&RiskInput) -> RiskScore`.
  `DecisionsApiScorer` speaks the Decisions API (`POST {base_url}/decisions`,
  one `score` question over `filar_agent::risk::RISK_LEVELS`). Implement the
  trait for another protocol; `LlmClient` is not involved.
- `ChatLogprobScorer` (`kind = "chat-logprobs"`, #550) turns any
  OpenAI-compatible chat model into a scorer, if the server returns
  `logprobs`: `POST {base_url}/chat/completions` with `max_tokens: 1`,
  `logprobs: true`, `top_logprobs`, and an instruction listing the same
  levels; the level distribution is read from the first token's
  `top_logprobs` (digits `0`–`4`, with or without a leading space). No
  `logprobs` in the answer → `unavailable`.
- Every command the agent proposes is rated, in every confirmation mode,
  except fleet runs and the bookkeeping tools that run nothing. Scoring runs
  in its own task and never waits on the gate or delays execution.
- The result arrives as `AgentEvent::CommandScored { proposal_id, safety_pct,
  confidence, model, cost_usd, unavailable }`. Match it to its command by
  `proposal_id` (also on `CommandProposed`, unique within the process). It may
  come after the command's `CommandFinished`, but always before `Finished` /
  `Error`, and never after `Cancelled`.
- Any failure (timeout, HTTP error, unexpected response) is `unavailable:
  true`; the command is unaffected. The key is never part of an error.
- The decision model sees the command, the agent's explanation and the target
  description — the same data the session's LLM already has.

## Upgrading to the next engine release (unreleased)

| Change | Breaks | What to do |
|--------|--------|------------|
| `KubeExecutor`, `KubeExecConfig`, `KubeExecEvent` under the `kube` feature; `PodRef`, `PodSelector`, `PodLimits` and `KubeCluster::describe_pod` in `filar-core` (#563) | Nothing — additive; `CommandExecutor` and `LlmClient` are unchanged | Use `KubeExecutor` wherever a `CommandExecutor` is expected for a pod shell |
| `AgentBuilder::kube_pod_mode` / `arbiter_kube_pod_context`, `KubePodTarget`, new field `KubernetesArbiterTarget::pod` (#564) | Struct literals of `KubernetesArbiterTarget` need `pod` (`None` for a cluster tab); `KubernetesArbiterTarget::new` is unchanged | Prefer `KubernetesArbiterTarget::new` / `::pod`, or add `pod: None` |
| `filar_core::KubePodSnapshot`, new field `KubeSessionSnapshot::pod` (#565) | Struct literals of `KubeSessionSnapshot` need `pod` (`None` for a cluster tab); saved sessions without it deserialize as cluster tabs | Add `pod: None`, or `Some(KubePodSnapshot { pod, container })` for a pod tab |
| `KubeInteractive`, `KubeExecutor::open_terminal` under the `kube` feature (#577) | Nothing — additive; `InteractiveTerminal` is unchanged | Use `open_terminal(cols, rows)` for an interactive TTY shell in the executor's pod |
| `ClusterApi::pod_http_get`, `PodHttpRequest`/`PodHttpResponse`, `ContainerPortInfo`, new fields `ContainerInfo::ports` and `KubeCluster::diagnostic_ports`, tool `pod_http_get` (#578) | Struct literals of `ContainerInfo` need `ports`; `KubeCluster` literals without `..Default::default()` need `diagnostic_ports`; other `ClusterApi` implementations get a default `Unavailable` | Add `ports: Vec::new()` / `diagnostic_ports: Vec::new()`; implement `pod_http_get` only if your API can port-forward |

## Upgrading to `engine-v3.0.0`

| Change | Breaks | What to do |
|--------|--------|------------|
| `KubeCluster`, `Config::kube_clusters`, `Config::kube_cluster(name)`, `KubeCluster::describe(namespace)`, `DEFAULT_KUBE_REQUEST_TIMEOUT_SECS`, `MAX_KUBE_REQUEST_TIMEOUT_SECS` (#562) | Nothing in the API — additive (struct literals of `Config` use `..Default::default()`) | Opt in: `[[kube_clusters]]` names a kubeconfig context plus namespaces |
| `Config::load` rejects duplicate target names — across `[[ssh_targets]]` and `[[kube_clusters]]`, **including two SSH targets with the same name** (#562) | Existing **configurations**, not code: a config with duplicate SSH target names loaded before and now fails at startup, even without any `[[kube_clusters]]` | Rename duplicated targets so every name is unique before upgrading |
| `ClusterApi`, `KubeClusterApi`, read-only cluster result types and the default `kube` transport feature (#572) | Dependency footprint for consumers using default features; no Rust API break | Disable default features for SSH-only/mobile builds, or enable only `kube` for cluster access |
| `KubeToolOutput`, `redact_logs`, redaction markers and `KubeCluster::reveal_env` (#573) | Nothing — additive (struct literals of `KubeCluster` use `..Default::default()`) | Pass Kubernetes objects/logs through `KubeToolOutput::redact_for_model` before giving them to a model |
| `KubeToolExecutor`, six `k8s_*` tool definitions, typed calls/results, cluster prompt and Kubernetes arbiter context (#574) | `ContainerInfo` and `ArbiterContext` struct literals need the new resource-map / `kubernetes` fields; `CommandExecutor` and `LlmClient` are unchanged | Use `..` where practical, or populate the new fields; route cluster model output through `KubeToolExecutor` |
| `FleetTarget::{Ssh, KubePod, KubeCluster}` and secret-free fleet snapshots (#567) | `FleetMember::target()` returns `&FleetTarget`; `HostConnector` and `FleetOperation::restore` use `FleetTarget`; `FleetSnapshot::hosts` is replaced by `targets`; target/member names and credential participant lists are owned `String` labels | Match the target variant in connectors and restore code; owned labels include Kubernetes scope; legacy session `hosts` arrays still deserialize as SSH targets |
| `KubeFleet` and its typed approval/report API (#568) | Exhaustive matches on `CoreError`, `HostRun` and `HostState` need the new Kubernetes variants; the fleet API itself is additive | Handle access denial, disappearance/unavailability and typed text answers; use one `KubeFleetApproval` per exact frozen-fleet call |
| `KubeAgent::new_fleet`, `KubeFleetObserver`, `KubeFleet::from_operation` and `KubeFleetReport::{view,status}` (#569) | Nothing — additive | Opt in to present frozen Kubernetes fleet results and restore exact membership without rediscovery |

`CommandExecutor` and `LlmClient` are unchanged.

## Upgrading to `engine-v2.0.7`

| Change | Breaks | What to do |
|--------|--------|------------|
| `AgentEvent::CommandProposed::proposal_id: u64` added (#549) | Constructing the event by hand; patterns without `..` | Add `..` to patterns; keep the id if you show safety ratings |
| `AgentEvent::CommandScored` added (#549) | Nothing — `AgentEvent` is `#[non_exhaustive]` | Handle it to show the rating next to the command |
| `RiskScorerConfig::warn_below` / `danger_below` (display thresholds) and `impl Default for RiskScorerConfig` (#551) | Struct literals without `..Default::default()` | Add `..Default::default()` |
| `ChatLogprobScorer`, `RiskScorerKind::ChatLogprobs` (#550); `RiskScorerKind` is `#[non_exhaustive]` | Exhaustive matches on `RiskScorerKind` | Add a `_` arm |
| `RiskScorer`, `RiskInput`, `RiskScore`, `DecisionsApiScorer`, `build_risk_scorer`, `AgentBuilder::risk_scorer`; `RiskScorerConfig`, `RiskScorerKind`, `Config::risk_scorer` (#549) | Nothing — additive (struct literals of `Config` use `..Default::default()`) | Opt in; see the safety-rating section |

`CommandExecutor` and `LlmClient` are unchanged.

## Upgrading to `engine-v2.0.5`

Nothing breaks: `CommandExecutor`, `LlmClient` and the existing traits are
unchanged — file transfer is a new, separate trait.

| Change | Breaks | What to do |
|--------|--------|------------|
| `FileTransfer`, `LocalFileTransfer`, `SftpFileTransfer`, `copy()`, `CopyObserver`, `CopyReport`, `CopyEvent`, `CopySource`, `OverwriteDecision`, `EntryKind`, `Metadata`, `DirEntry`, `PathStyle`, `PART_SUFFIX`, `OLD_SUFFIX`, `SshExecutor::open_sftp()` added (#532–#534) | Nothing — additive | Opt in where a client moves files between the local disk and SSH hosts; remote writes only on an explicit user action (see the file-transfer section and the #531 exception) |

## Upgrading to `engine-v2.0.4`

| Change | Breaks | What to do |
|--------|--------|------------|
| `ChatBlock::Command::tool: Option<ToolInvocation>` added (#525) | Struct literals and exhaustive patterns of `ChatBlock::Command` | `tool: None` where you build one by hand (or the `ToolInvocation` from `CommandProposed`); add `..` to patterns. Old session files load unchanged (`#[serde(default)]`) |
| `AgentEvent::CommandProposed::tool: ToolInvocation` added (#525) | Constructing the event by hand; patterns without `..` | Keep the tool on the chat block you create for the command |
| `ToolInvocation`, `SHELL_ESCAPE_EXPLANATION` exported from `filar-core` (#525) | Nothing — additive | — |

When you rebuild the model's history from your own chat blocks, replay a
command as what it was: an assistant message with `tool_calls` (the turn's
preamble text as its content) followed by one `ChatMessage::tool` result per
call — never as assistant text such as `Command: …\nOutput: …`. Models
imitate the history: commands written as text teach them to "run" commands by
typing them, with invented output and no confirmation (#525). The TUI's
`history_to_messages` is the reference. The agent loop additionally flags an
answer that writes a command and its output as text, and follows up once on
an answer that ends by announcing an action without a tool call.

## Upgrading to `engine-v2.0.3`

| Change | Breaks | What to do |
|--------|--------|------------|
| `ViewDropped::exit_code` and `ViewDropped::reason` added (#513) | Struct literals constructing `ViewDropped` | `exit_code: None, reason: None` where you build one by hand; reading an existing value is unaffected |
| `fleet_fold::Failure` and `DroppedHost::failure()`/`handle()` added (#513) | Nothing — additive | Opt in to show a failed host's exit code and first error line |

## Upgrading to `engine-v2.0.2`

| Change | Breaks | What to do |
|--------|--------|------------|
| `ComparedValue::Text` added (#505) | Exhaustive matches on `ComparedValue` | Handle the variant: a short plain-text answer (up to 10 lines / 1 KB) of a raw-text check, shown to the agent line by line |
| `InteractiveTerminal::foreground_cwd` (#499), `fleet_view::looks_binary` (#502) added | Nothing — additive | `foreground_cwd` defaults to `None`; override where the terminal can report the foreground process's directory |

## Upgrading to `engine-v2.0.0`

| Change | Breaks | What to do |
|--------|--------|------------|
| `Session::fleet: Option<FleetSnapshot>` added | Struct literals constructing `Session` | `fleet: None` for an ordinary session. Deserialised sessions are unaffected |
| `SessionMeta::fleet_group: Option<String>` added | Struct literals constructing `SessionMeta` | `fleet_group: None` |
| Fleet modules, `ReadOnlyExecutor`, `FleetOperation`, fleet checks, fleet runbook | Nothing — additive | Opt in where you need a fleet |
| `SshAuth::Key` gains `passphrase: Option<String>` (#480) | Struct literals and exhaustive patterns of `SshAuth::Key` | `SshAuth::Key { path, passphrase: None }`; patterns `SshAuth::Key { path, .. }`. Never (de)serialised |
| `MissingCredentials::NoPassphrase` added (#480); `AgentUnsupported` removed (#479) | Exhaustive matches on `MissingCredentials` | Handle the new reason; drop the removed one |

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

By default the key is read from `FILAR_LLM_API_KEY` (the former name
`GLM_API_KEY` is still read as a fallback). A profile can override this with
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
