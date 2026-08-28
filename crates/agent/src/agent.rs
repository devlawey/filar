//! Agent loop: orchestrates LLM ↔ tool execution with safety checks.
//!
//! The [`Agent`] struct ties together an [`LlmClient`], a [`CommandExecutor`],
//! and a [`CommandConfirmer`] to implement the core agent loop:
//!
//! ```text
//! user prompt → LLM → (tool call?) → confirm → execute → result → LLM → … → final answer
//! ```

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use filar_core::{CommandConfirmMode, CoreError, Result, SecretProvider};
use filar_transport::{CommandExecutor, SecretSubstitutingExecutor};

use crate::{
    arbiter::{self, ArbiterContext, ARBITER_TIMEOUT_SECS, HISTORY_TAIL_EXCHANGES},
    events::{AgentEvent, EventSink},
    security::{self, CommandConfirmer, ConfirmDecision},
    tools::{self},
    ChatMessage, ChatRequest, LlmClient, ToolCall,
};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Default maximum number of agent loop iterations (anti-runaway).
const DEFAULT_MAX_ITERATIONS: usize = 50;

/// Default maximum output length (in characters) before truncation.
const DEFAULT_MAX_OUTPUT_CHARS: usize = 10_000;

/// Maximum number of retries for missing explanation in Explain mode.
const MAX_MISSING_EXPLANATION_RETRIES: u32 = 2;

/// System prompt block appended when `CommandConfirmMode::Explain` is active.
const SAFE_MODE_PROMPT: &str = r#"

SAFE MODE IS ACTIVE.

Every command you propose must include an `explanation` that lets the user decide
whether to approve it, without having to reconstruct your reasoning. In 1–3 sentences,
each explanation must cover:

1. What the command does — in plain language, not a restatement of its flags.
2. Why it is needed right now — the link to the user's task or to what you just
   observed. This is the most important part.
3. What it changes — say that it only reads state, or name exactly what it creates,
   modifies, restarts, or deletes.

For anything that modifies state, also state the blast radius and how to undo it — or
say plainly that it cannot be undone.

Rules:
- Never write an explanation that merely restates the command ("runs df -h to show
  disk usage"). If it would not help someone decide, it is not good enough.
- Distinguish what you know from what you assume. Say "assuming this service is managed
  by systemd" rather than asserting it.
- Prefer several small commands, each separately explainable, over one compound command
  whose combined effect is hard to describe.
- If the user rejects a command, do not resubmit the same command with a reworded
  explanation. Propose a different approach or ask a clarifying question.
"#;

/// Build the system prompt based on execution context.
///
/// - `is_local`: true when executing commands on the local machine.
/// - `ssh_info`: optional human-readable description of the SSH target
///   (e.g. "user@host:port") for remote sessions.
/// - `is_windows`: true when running on a Windows host (affects shell and commands).
fn build_system_prompt(is_local: bool, ssh_info: Option<&str>, is_windows: bool) -> String {
    let transport_desc = if is_local {
        if is_windows {
            "You are a system administration assistant operating on the LOCAL Windows machine. \
             Commands are executed directly on this computer via PowerShell, not over a network. \
             Use Windows-compatible PowerShell commands. For example: use Get-ComputerInfo instead of uname, \
             Get-ChildItem instead of ls, Get-Content instead of cat, Select-String instead of grep. \
             PowerShell aliases like ls, cat, cp are available but use cmdlet syntax for best results."
                .to_string()
        } else {
            "You are a system administration assistant operating on the LOCAL machine. \
             Commands are executed directly on this computer, not over a network."
                .to_string()
        }
    } else {
        match ssh_info {
            Some(info) => format!(
                "You are a system administration assistant operating a REMOTE machine via SSH ({info}). \
                 Commands are executed on the remote host over an SSH connection."
            ),
            None => "You are a system administration assistant operating a REMOTE machine via SSH. \
                     Commands are executed on the remote host over an SSH connection.".to_string(),
        }
    };

    let shell_desc = if is_local {
        if is_windows {
            "You are running on Windows with PowerShell. \
             Each command runs in a separate process — shell state (cwd, env) does NOT persist between calls. \
             Use absolute paths or chain commands with semicolons if needed."
        } else {
            "You are running on a POSIX shell. \
             Each command runs in a separate process — shell state (cwd, env) does NOT persist between calls. \
             Use absolute paths or chain commands with && or ; if needed."
        }
    } else {
        // SSH: persistent channel — state persists between commands.
        "You are running on a persistent POSIX shell session over SSH. \
         Shell state (cwd, env) DOES persist between calls: your `cd`, exported variables \
         and environment carry over to subsequent commands. Prefer using this (e.g. \
         `cd /var/log` then `ls`)."
    };

    format!(
        r#"{transport_desc} You have tools to run commands, read files, and list directories. IMPORTANT: Determine the language of the user's FIRST request in this conversation, and write ALL of your explanations, summaries, questions, and the final answer in that same language. Keep this language consistent for the entire session. Do NOT default to any fixed language. Note: raw command output (stdout/stderr) is passed through as-is and must NOT be translated — only your own prose around it follows the user's language.

Rules:
1. Always explain what you're about to do before calling a tool.
2. Prefer read-only commands before making changes.
3. Be cautious with destructive commands (rm, dd, mkfs, Remove-Item, Format-Volume, etc.).
4. If a command is denied by the user, do not retry it — try a different approach.
5. Summarize the results concisely after each command.
6. When the task is complete, provide a clear final answer in the user's language.
7. If you need information from the user (e.g. a password, a choice between options), ask them directly in your text response — do not try to use interactive prompts in commands. Wait for their reply before continuing.
8. Never put passwords or secrets directly in commands. If a password is needed, ask the user to provide it via Ctrl+P (secure masked input). The password is stored as $FILAR_SECRET_N and you are told the variable name — use that placeholder in commands (substituted at execution; you never see the real value). Do not echo or print secret variables. Never run bare `sudo`/`su`/`doas` that would prompt on a TTY — agent commands have no interactive password UI. After the user provides a secret, use a non-interactive form such as `printf '%s\n' "$FILAR_SECRET_1" | sudo -S <command>` (POSIX) or an equivalent that reads the password from stdin. NEVER combine such a secret pipe with a `<<EOF` heredoc on the same command: the heredoc replaces the last pipeline command's stdin, so sudo tries the heredoc lines as the password. To write a file via sudo, first write it to a temp path without sudo, then pipe the secret to `sudo -S cp` of that temp file.
9. NEVER run interactive commands (vim, nano, top, htop, less, man, mc, screen, tmux, ssh, etc.). These commands take over the terminal and will hang indefinitely. Instead, use non-interactive alternatives: 'cat file' instead of 'less file', 'grep -n pattern file' instead of 'vim file', 'head -n 50 file' to preview. For editing files, use 'sed' or 'tee' with heredocs.
10. NEVER use long wall-clock waits (`sleep N`, `Start-Sleep`, etc.) to poll progress. Every tool command shares a hard timeout (`[timeouts].command_secs`). A `sleep` near or above that timeout will fail. For downloads, pulls, builds, or other long jobs: use `start_background_job` then poll with `background_job_status` (short calls; timeout applies to each poll, not job lifetime). Cancel with `cancel_background_job`; list jobs with `list_background_jobs`. For live interactive progress, ask the user to use Ctrl+T (interactive terminal) instead of blocking the agent tool call.
{shell_desc}"#
    )
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// The agent that orchestrates LLM calls and tool execution.
pub struct Agent {
    llm: Arc<dyn LlmClient>,
    executor: Arc<dyn CommandExecutor>,
    confirmer: Arc<dyn CommandConfirmer>,
    confirm_mode: CommandConfirmMode,
    max_iterations: usize,
    max_output_chars: usize,
    system_prompt: String,
    /// Optional callback invoked for each text delta during LLM streaming.
    on_text_delta: Option<Arc<dyn Fn(String) + Send + Sync>>,
    /// Optional event sink for emitting AgentEvents to frontends.
    event_sink: Option<EventSink>,
    /// Optional cancellation token for user-initiated cancellation.
    cancellation: Option<CancellationToken>,
    /// Optional timeout for command confirmation.
    confirm_timeout: Option<Duration>,
    /// Optional timeout for command execution.
    command_timeout: Option<Duration>,
    /// Optional secret provider (stored for potential future use).
    /// The executor is already wrapped in `SecretSubstitutingExecutor`
    /// during `build()` if this is set.
    #[allow(dead_code)]
    secret_provider: Option<Arc<dyn SecretProvider>>,
    /// Optional LLM client for the independent command arbiter.
    arbiter_llm: Option<Arc<dyn LlmClient>>,
    /// When false, skip arbiter audits even if `arbiter_llm` is set.
    arbiter_enabled: bool,
    /// Execution context passed to the arbiter (local vs remote).
    arbiter_context: ArbiterContext,
    /// Display name of the arbiter profile (for events / TUI).
    arbiter_model_name: String,
    /// Session id (tab) — scopes background jobs.
    session_id: String,
    /// True when commands run on the local machine (vs SSH).
    is_local: bool,
}

/// Builder for [`Agent`].
pub struct AgentBuilder {
    llm: Option<Arc<dyn LlmClient>>,
    executor: Option<Arc<dyn CommandExecutor>>,
    confirmer: Option<Arc<dyn CommandConfirmer>>,
    confirm_mode: CommandConfirmMode,
    max_iterations: usize,
    max_output_chars: usize,
    system_prompt: Option<String>,
    on_text_delta: Option<Arc<dyn Fn(String) + Send + Sync>>,
    event_sink: Option<EventSink>,
    cancellation: Option<CancellationToken>,
    confirm_timeout: Option<Duration>,
    command_timeout: Option<Duration>,
    secret_provider: Option<Arc<dyn SecretProvider>>,
    arbiter_llm: Option<Arc<dyn LlmClient>>,
    arbiter_enabled: bool,
    arbiter_context: Option<ArbiterContext>,
    arbiter_model_name: Option<String>,
    session_id: Option<String>,
    is_local: bool,
}

impl AgentBuilder {
    /// Create a new builder.
    pub fn new() -> Self {
        Self {
            llm: None,
            executor: None,
            confirmer: None,
            confirm_mode: CommandConfirmMode::Always,
            max_iterations: DEFAULT_MAX_ITERATIONS,
            max_output_chars: DEFAULT_MAX_OUTPUT_CHARS,
            system_prompt: None,
            on_text_delta: None,
            event_sink: None,
            cancellation: None,
            confirm_timeout: None,
            command_timeout: None,
            secret_provider: None,
            arbiter_llm: None,
            arbiter_enabled: true,
            arbiter_context: None,
            arbiter_model_name: None,
            session_id: None,
            is_local: false,
        }
    }

    /// Set the LLM client.
    pub fn llm(mut self, llm: Arc<dyn LlmClient>) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Set the command executor.
    pub fn executor(mut self, executor: Arc<dyn CommandExecutor>) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Set the command confirmer.
    pub fn confirmer(mut self, confirmer: Arc<dyn CommandConfirmer>) -> Self {
        self.confirmer = Some(confirmer);
        self
    }

    /// Set the confirmation mode.
    pub fn confirm_mode(mut self, mode: CommandConfirmMode) -> Self {
        self.confirm_mode = mode;
        self
    }

    /// Set the maximum number of loop iterations.
    pub fn max_iterations(mut self, n: usize) -> Self {
        self.max_iterations = n;
        self
    }

    /// Set the maximum output length in characters before truncation.
    pub fn max_output_chars(mut self, n: usize) -> Self {
        self.max_output_chars = n;
        self
    }

    /// Set a custom system prompt. If not set, a default SSH prompt is used.
    pub fn system_prompt(mut self, prompt: impl Into<String>) -> Self {
        self.system_prompt = Some(prompt.into());
        self
    }

    /// Set the text-delta callback for LLM streaming.
    pub fn on_text_delta(mut self, cb: Arc<dyn Fn(String) + Send + Sync>) -> Self {
        self.on_text_delta = Some(cb);
        self
    }

    /// Set the event sink for emitting [`AgentEvent`]s to a frontend.
    ///
    /// If set, the agent emits events at key points in the processing loop:
    /// [`AgentEvent::Started`], [`AgentEvent::TextDelta`],
    /// [`AgentEvent::CommandProposed`], [`AgentEvent::CommandFinished`],
    /// [`AgentEvent::Finished`], [`AgentEvent::Error`], and
    /// [`AgentEvent::Cancelled`].
    pub fn event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Set a cancellation token for user-initiated cancellation.
    ///
    /// When the token is cancelled, the agent loop aborts at the next
    /// `tokio::select!` checkpoint (LLM request or command execution) and
    /// emits [`AgentEvent::Cancelled`]. If not set, the agent runs to
    /// completion (TUI behavior — eternal wait).
    pub fn cancellation(mut self, token: CancellationToken) -> Self {
        self.cancellation = Some(token);
        self
    }

    /// Set a timeout for command confirmation.
    ///
    /// If the confirmer does not respond within `duration`, the command is
    /// treated as denied. Default: no timeout (eternal wait — TUI behavior).
    pub fn confirm_timeout(mut self, duration: Duration) -> Self {
        self.confirm_timeout = Some(duration);
        self
    }

    /// Set a timeout for command execution.
    ///
    /// If the command does not complete within `duration`, it is aborted and
    /// a timeout error is returned to the LLM. Default: no timeout (current
    /// behavior — relies on transport-level timeouts).
    pub fn command_timeout(mut self, duration: Duration) -> Self {
        self.command_timeout = Some(duration);
        self
    }

    /// Set a secret provider for command substitution and output sanitisation.
    ///
    /// When set, the executor is wrapped in [`SecretSubstitutingExecutor`] during
    /// [`build`](Self::build). `$FILAR_SECRET_N` placeholders in commands are
    /// replaced with actual values from the provider before execution, and
    /// secret values in command output are masked back to placeholders.
    pub fn secret_provider(mut self, provider: Arc<dyn SecretProvider>) -> Self {
        self.secret_provider = Some(provider);
        self
    }

    /// Set the LLM client used for independent command audits.
    pub fn arbiter_llm(mut self, llm: Option<Arc<dyn LlmClient>>) -> Self {
        self.arbiter_llm = llm;
        self
    }

    /// Enable or disable the command arbiter (default: enabled).
    pub fn arbiter_enabled(mut self, enabled: bool) -> Self {
        self.arbiter_enabled = enabled;
        self
    }

    /// Set execution context for arbiter prompts (local vs SSH target).
    pub fn arbiter_context(mut self, ctx: ArbiterContext) -> Self {
        self.arbiter_context = Some(ctx);
        self
    }

    /// Set the display name of the arbiter profile (shown in the TUI).
    pub fn arbiter_model_name(mut self, name: impl Into<String>) -> Self {
        self.arbiter_model_name = Some(name.into());
        self
    }

    /// Set the session id used to scope background jobs (typically the tab id).
    pub fn session_id(mut self, id: impl Into<String>) -> Self {
        self.session_id = Some(id.into());
        self
    }

    /// Mark whether the executor targets the local machine (affects background spawn).
    pub fn is_local(mut self, is_local: bool) -> Self {
        self.is_local = is_local;
        self
    }

    /// Convenience: set arbiter context for local execution.
    pub fn arbiter_local_context(self) -> Self {
        self.arbiter_context(ArbiterContext {
            is_local: true,
            ssh_info: None,
        })
    }

    /// Convenience: set arbiter context for SSH remote execution.
    pub fn arbiter_ssh_context(self, ssh_info: Option<String>) -> Self {
        self.arbiter_context(ArbiterContext {
            is_local: false,
            ssh_info,
        })
    }

    /// Convenience: set the system prompt for local execution.
    pub fn local_mode(self) -> Self {
        self.system_prompt(build_system_prompt(true, None, cfg!(windows)))
            .is_local(true)
    }

    /// Convenience: set the system prompt for SSH remote execution.
    pub fn ssh_mode(self, ssh_info: Option<&str>) -> Self {
        self.system_prompt(build_system_prompt(false, ssh_info, false))
            .is_local(false)
    }

    /// Build the agent.
    pub fn build(self) -> Result<Agent> {
        let executor = self.executor.ok_or_else(|| CoreError::Other("executor not set".into()))?;
        // Wrap the executor in SecretSubstitutingExecutor if a provider is set.
        let secret_provider = self.secret_provider;
        let executor: Arc<dyn CommandExecutor> = match &secret_provider {
            Some(provider) => Arc::new(SecretSubstitutingExecutor::new(executor, provider.clone())),
            None => executor,
        };
        let mut system_prompt = self.system_prompt.unwrap_or_else(||
            build_system_prompt(false, None, cfg!(windows))
        );
        // Append SAFE MODE block in Explain mode.
        if self.confirm_mode == CommandConfirmMode::Explain {
            system_prompt.push('\n');
            system_prompt.push_str(SAFE_MODE_PROMPT);
        }
        Ok(Agent {
            llm: self.llm.ok_or_else(|| CoreError::Other("LLM client not set".into()))?,
            executor,
            confirmer: self.confirmer.ok_or_else(|| CoreError::Other("confirmer not set".into()))?,
            confirm_mode: self.confirm_mode,
            max_iterations: self.max_iterations,
            max_output_chars: self.max_output_chars,
            system_prompt,
            on_text_delta: self.on_text_delta,
            event_sink: self.event_sink,
            cancellation: self.cancellation,
            confirm_timeout: self.confirm_timeout,
            command_timeout: self.command_timeout,
            secret_provider,
            arbiter_llm: self.arbiter_llm,
            arbiter_enabled: self.arbiter_enabled,
            arbiter_context: self.arbiter_context.unwrap_or(ArbiterContext {
                is_local: false,
                ssh_info: None,
            }),
            arbiter_model_name: self
                .arbiter_model_name
                .unwrap_or_else(|| "session profile".into()),
            session_id: self
                .session_id
                .unwrap_or_else(|| "default".into()),
            is_local: self.is_local,
        })
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Run a future with cancellation support.
///
/// If a cancellation token is set, wraps the future in `tokio::select!`
/// with `token.cancelled()`. Returns `Err("cancelled")` if cancelled.
async fn with_cancellation<F, T>(
    cancellation: Option<&CancellationToken>,
    future: F,
) -> Result<T>
where
    F: std::future::Future<Output = Result<T>>,
{
    match cancellation {
        Some(token) => {
            tokio::select! {
                result = future => result,
                _ = token.cancelled() => Err(CoreError::Other("cancelled".into())),
            }
        }
        None => future.await,
    }
}

impl Agent {
    /// Create a new builder for configuring an agent.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    /// Emit an event to the sink (if set).
    fn emit(&self, event: AgentEvent) {
        if let Some(ref sink) = self.event_sink {
            sink(event);
        }
    }

    /// Run the independent command arbiter when enabled. Never blocks confirmation.
    async fn run_command_audit(
        &self,
        command: &str,
        explanation: &str,
        destructive: bool,
        conversation: &[ChatMessage],
    ) {
        if !self.arbiter_enabled || self.confirm_mode == CommandConfirmMode::Never {
            return;
        }
        let Some(ref arbiter_llm) = self.arbiter_llm else {
            return;
        };

        let target_desc = arbiter::target_description(&self.arbiter_context);
        let history_tail =
            arbiter::history_tail_from_messages(conversation, HISTORY_TAIL_EXCHANGES);
        let messages = arbiter::build_audit_messages(
            command,
            explanation,
            destructive,
            &target_desc,
            &history_tail,
        );

        let audit = arbiter::run_audit(
            arbiter_llm.as_ref(),
            messages,
            Duration::from_secs(ARBITER_TIMEOUT_SECS),
            self.cancellation.as_ref(),
        )
        .await;

        // `arbiter_model_name` is the arbiter *profile* name (set in runner from
        // `LlmProfile.name`), despite the historical field name. Confirm overlay
        // compares it to the session profile name (#360).
        self.emit(AgentEvent::CommandAudited {
            verdict: audit.verdict.label().to_string(),
            reason: audit.reason.clone(),
            arbiter_model: Some(self.arbiter_model_name.clone()),
            unavailable: audit.unavailable,
        });

        if audit.tokens_in > 0 || audit.tokens_out > 0 {
            self.emit(AgentEvent::TokenUsage {
                tokens_in: audit.tokens_in,
                tokens_out: audit.tokens_out,
                cost: audit.cost,
                model: audit.model,
                arbiter: true,
            });
        }
    }

    /// Check whether the cancellation token has been triggered.
    fn is_cancelled(&self) -> bool {
        match &self.cancellation {
            Some(token) => token.is_cancelled(),
            None => false,
        }
    }

    /// Run the agent loop with a user prompt and optional conversation history.
    ///
    /// Returns the final text response from the LLM, or an error if the
    /// loop exceeds the maximum iterations or encounters a failure.
    ///
    /// Events emitted via the [`EventSink`] (if set):
    /// [`AgentEvent::Started`] → [`AgentEvent::TextDelta`] (streaming) →
    /// [`AgentEvent::CommandProposed`] / [`AgentEvent::CommandFinished`] (tool calls) →
    /// [`AgentEvent::Finished`] (success) or [`AgentEvent::Error`] (failure).
    pub async fn run(&self, user_prompt: &str, history: &[ChatMessage]) -> Result<String> {
        self.emit(AgentEvent::Started);
        match self.run_loop(user_prompt, history).await {
            Ok(text) => {
                self.emit(AgentEvent::Finished(text.clone()));
                Ok(text)
            }
            Err(e) => {
                if self.is_cancelled() {
                    self.emit(AgentEvent::Cancelled);
                } else {
                    self.emit(AgentEvent::Error(e.to_string()));
                }
                Err(e)
            }
        }
    }

    /// Inner agent loop — does NOT emit `Started`/`Finished`/`Error` events.
    /// The caller ([`run`](Self::run)) wraps this to emit those events.
    async fn run_loop(&self, user_prompt: &str, history: &[ChatMessage]) -> Result<String> {
        // Build initial message history: system prompt + prior context + new user message.
        let mut messages: Vec<ChatMessage> = vec![ChatMessage::system(&self.system_prompt)];
        messages.extend_from_slice(history);
        messages.push(ChatMessage::user(user_prompt));

        let tool_defs = tools::tool_definitions(self.confirm_mode);

        info!(prompt = %user_prompt, "agent loop started");

        let mut missing_explanation_count: u32 = 0;

        for iteration in 0..self.max_iterations {
            info!(iteration, "sending request to LLM");

            let request = ChatRequest {
                messages: messages.clone(),
                tools: tool_defs.clone(),
            };

            // Use streaming if either callback is set, otherwise fall back to non-streaming.
            // Both on_text_delta and event_sink can fire simultaneously.
            let response = if self.on_text_delta.is_some() || self.event_sink.is_some() {
                let cb = self.on_text_delta.clone();
                let sink = self.event_sink.clone();
                let callback = move |delta: String| {
                    if let Some(ref cb) = cb {
                        cb(delta.clone());
                    }
                    if let Some(ref sink) = sink {
                        sink(AgentEvent::TextDelta(delta));
                    }
                };
                with_cancellation(self.cancellation.as_ref(), self.llm.chat_stream(&request, &callback)).await?
            } else {
                with_cancellation(self.cancellation.as_ref(), self.llm.chat(&request)).await?
            };

            // Emit token usage if the provider reported it.
            if let Some(ref u) = response.usage {
                self.emit(AgentEvent::TokenUsage {
                    tokens_in: u.prompt_tokens.unwrap_or(0),
                    tokens_out: u.completion_tokens.unwrap_or(0),
                    cost: u.cost,
                    model: response.model.clone(),
                    arbiter: false,
                });
            }

            if response.has_tool_calls() {
                let tool_calls = response.tool_calls.clone();
                info!(iteration, count = tool_calls.len(), "LLM requested tool calls");

                // Add the assistant message with tool calls (and any preamble text) to history.
                let assistant_msg = ChatMessage::assistant_with_tools(
                    &response.text,
                    tool_calls.clone(),
                );
                messages.push(assistant_msg);

                // In Explain mode, validate ALL explanations before executing any.
                // If any tool call lacks an explanation, reject the entire batch
                // and let the model retry — no partial execution in safe mode.
                if self.confirm_mode == CommandConfirmMode::Explain {
                    let mut errors: Vec<(String, String)> = Vec::new();
                    for tc in &tool_calls {
                        if let Some(err) = tools::check_explanation(&tc.name, &tc.arguments) {
                            errors.push((tc.id.clone(), err));
                        }
                    }
                    if !errors.is_empty() {
                        missing_explanation_count += 1;
                        if missing_explanation_count > MAX_MISSING_EXPLANATION_RETRIES {
                            warn!(count = missing_explanation_count, "explanation retries exhausted");
                            return Err(CoreError::Other(
                                "Agent repeatedly proposed commands without the required explanation. Stopping.".into()
                            ));
                        }
                        for (id, err) in errors {
                            warn!(tool = %id, "missing explanation in safe mode");
                            messages.push(ChatMessage::tool(id, err));
                        }
                        // Skip execution for this entire batch — let the model retry.
                        continue;
                    }
                    // All tool calls have valid explanations — reset the counter.
                    missing_explanation_count = 0;
                }

                // Process each tool call.
                for tc in &tool_calls {
                    let result = self.process_tool_call(tc, &messages).await?;
                    messages.push(result);
                }
            } else {
                info!(iteration, "agent produced final text response");
                return Ok(response.text);
            }
        }

        // Exceeded max iterations.
        warn!(max_iterations = self.max_iterations, "agent loop exceeded max iterations");
        Err(CoreError::Other(format!(
            "agent loop exceeded maximum iterations ({})",
            self.max_iterations
        )))
    }

    /// Process a single tool call: parse, confirm, execute, and return the
    /// tool result message.
    ///
    /// Emits [`AgentEvent::CommandProposed`] before confirmation and
    /// [`AgentEvent::CommandFinished`] after execution (or denial).
    async fn process_tool_call(
        &self,
        tc: &ToolCall,
        conversation: &[ChatMessage],
    ) -> Result<ChatMessage> {
        // Parse the tool call.
        let parsed = match tools::parse_tool_call(&tc.id, &tc.name, &tc.arguments) {
            Ok(p) => p,
            Err(e) => {
                warn!(tool = %tc.name, error = %e, "failed to parse tool call");
                return Ok(ChatMessage::tool(
                    &tc.id,
                    format!("Error: failed to parse tool call: {e}"),
                ));
            }
        };

        info!(tool = ?parsed.kind, command = %parsed.command, "processing tool call");

        let display_command = match parsed.kind {
            tools::ToolKind::StartBackgroundJob => {
                crate::background::confirm_command_for_start(&parsed.command)
            }
            tools::ToolKind::CancelBackgroundJob => parsed
                .job_id
                .as_ref()
                .map(|id| crate::background::confirm_command_for_cancel(id, None))
                .unwrap_or_else(|| parsed.command.clone()),
            _ => parsed.command.clone(),
        };

        // Check security / confirmation.
        let decision = security::tool_needs_confirmation(
            parsed.kind,
            &parsed.command,
            self.confirm_mode,
        );

        let destructive = security::is_destructive(&parsed.command)
            || matches!(parsed.kind, tools::ToolKind::CancelBackgroundJob);

        // Emit CommandProposed before any confirmation logic.
        self.emit(AgentEvent::CommandProposed {
            command: display_command.clone(),
            explanation: parsed.explanation.clone(),
            destructive,
        });

        // Reject long sleep/wait patterns before confirm/execute — they only
        // burn the command timeout and never finish usefully (#323).
        if matches!(parsed.kind, tools::ToolKind::RunCommand) {
            if let Some(msg) = crate::long_wait::reject_long_wait(&parsed.command) {
                warn!(command = %parsed.command, "long wait rejected by policy");
                self.emit(AgentEvent::CommandFinished {
                    command: parsed.command.clone(),
                    output: msg.clone(),
                    denied: false,
                });
                return Ok(ChatMessage::tool(&tc.id, msg));
            }
        }

        match decision {
            ConfirmDecision::Blocked(reason) => {
                warn!(command = %parsed.command, reason = %reason, "command blocked by security");
                // No CommandFinished event for blocked commands: blocked is not a
                // user denial, and the TUI should not show a command block for it.
                // The block reason is sent back to the LLM as tool context.
                return Ok(ChatMessage::tool(
                    &tc.id,
                    format!("Error: command blocked by security policy: {reason}"),
                ));
            }
            ConfirmDecision::AutoApproved => {
                info!(command = %parsed.command, "command auto-approved");
            }
            ConfirmDecision::NeedsConfirmation => {
                self.run_command_audit(
                    &parsed.command,
                    &parsed.explanation,
                    destructive,
                    conversation,
                )
                .await;

                let confirm_fut = self
                    .confirmer
                    .confirm(&display_command, &parsed.explanation, destructive);
                let approved = if let Some(ct) = self.confirm_timeout {
                    match tokio::time::timeout(ct, with_cancellation(self.cancellation.as_ref(), confirm_fut)).await {
                        Ok(result) => result?,
                        Err(_) => {
                            info!(command = %display_command, "confirmation timed out");
                            self.emit(AgentEvent::CommandFinished {
                                command: display_command.clone(),
                                output: "Confirmation timed out".to_string(),
                                denied: true,
                            });
                            return Ok(ChatMessage::tool(
                                &tc.id,
                                "Command confirmation timed out. Treating as denied.".to_string(),
                            ));
                        }
                    }
                } else {
                    with_cancellation(self.cancellation.as_ref(), confirm_fut).await?
                };

                if !approved {
                    info!(command = %display_command, "command denied by user");
                    self.emit(AgentEvent::CommandFinished {
                        command: display_command.clone(),
                        output: String::new(),
                        denied: true,
                    });
                    return Ok(ChatMessage::tool(
                        &tc.id,
                        "Command denied by user. Try a different approach.".to_string(),
                    ));
                }
                info!(command = %display_command, "command approved by user");
            }
        }

        // Execute the tool, with optional cancellation and command timeout.
        let exec_fut = self.execute_parsed_tool(&parsed);
        let output = if let Some(ct) = self.command_timeout {
            match tokio::time::timeout(ct, with_cancellation(self.cancellation.as_ref(), exec_fut)).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) if e.to_string() == "cancelled" => {
                    // Cancellation — kill the running command (foreground only).
                    if !matches!(
                        parsed.kind,
                        tools::ToolKind::StartBackgroundJob
                            | tools::ToolKind::BackgroundJobStatus
                            | tools::ToolKind::CancelBackgroundJob
                            | tools::ToolKind::ListBackgroundJobs
                    ) {
                        let _ = self.executor.cancel().await;
                    }
                    return Err(e);
                }
                Ok(Err(e)) => {
                    warn!(command = %display_command, error = %e, "tool execution failed");
                    let detail = e.to_string();
                    let mut output = if detail.to_ascii_lowercase().contains("timed out") {
                        crate::long_wait::enrich_timeout_message(&format!("Error: {detail}"))
                    } else {
                        format!("Error: {detail}")
                    };
                    output = crate::password_prompt::enrich_password_prompt_message_for_command(
                        &parsed.command, &output,
                    );
                    self.emit(AgentEvent::CommandFinished {
                        command: display_command.clone(),
                        output: output.clone(),
                        denied: false,
                    });
                    return Ok(ChatMessage::tool(&tc.id, output));
                }
                Err(_) => {
                    warn!(command = %display_command, "command timed out");
                    if !matches!(
                        parsed.kind,
                        tools::ToolKind::StartBackgroundJob
                            | tools::ToolKind::BackgroundJobStatus
                            | tools::ToolKind::CancelBackgroundJob
                            | tools::ToolKind::ListBackgroundJobs
                    ) {
                        let _ = self.executor.cancel().await;
                    }
                    let output = crate::long_wait::enrich_timeout_message("Command timed out.");
                    self.emit(AgentEvent::CommandFinished {
                        command: display_command.clone(),
                        output: output.clone(),
                        denied: false,
                    });
                    return Ok(ChatMessage::tool(&tc.id, output));
                }
            }
        } else {
            match with_cancellation(self.cancellation.as_ref(), exec_fut).await {
                Ok(o) => o,
                Err(e) if e.to_string() == "cancelled" => {
                    if !matches!(
                        parsed.kind,
                        tools::ToolKind::StartBackgroundJob
                            | tools::ToolKind::BackgroundJobStatus
                            | tools::ToolKind::CancelBackgroundJob
                            | tools::ToolKind::ListBackgroundJobs
                    ) {
                        let _ = self.executor.cancel().await;
                    }
                    return Err(e);
                }
                Err(e) => {
                    warn!(command = %display_command, error = %e, "tool execution failed");
                    let detail = e.to_string();
                    let mut output = if detail.to_ascii_lowercase().contains("timed out") {
                        crate::long_wait::enrich_timeout_message(&format!("Error: {detail}"))
                    } else {
                        format!("Error: {detail}")
                    };
                    output = crate::password_prompt::enrich_password_prompt_message_for_command(
                        &parsed.command, &output,
                    );
                    self.emit(AgentEvent::CommandFinished {
                        command: display_command.clone(),
                        output: output.clone(),
                        denied: false,
                    });
                    return Ok(ChatMessage::tool(&tc.id, output));
                }
            }
        };

        // Truncate output if too long; enrich password/TTY failures for the LLM.
        let enriched = crate::password_prompt::enrich_password_prompt_message_for_command(
            &parsed.command, &output,
        );
        let truncated = self.truncate_output(&enriched);

        self.emit(AgentEvent::CommandFinished {
            command: display_command,
            output: truncated.clone(),
            denied: false,
        });

        Ok(ChatMessage::tool(&tc.id, truncated))
    }

    async fn execute_parsed_tool(&self, parsed: &tools::ParsedToolCall) -> Result<String> {
        match parsed.kind {
            tools::ToolKind::StartBackgroundJob => {
                crate::background::start_job(
                    &self.session_id,
                    &parsed.command,
                    self.is_local,
                    self.executor.as_ref(),
                )
                .await
            }
            tools::ToolKind::BackgroundJobStatus => {
                let job_id = parsed
                    .job_id
                    .as_deref()
                    .ok_or_else(|| CoreError::Other("missing job_id".into()))?;
                let tail = parsed.tail_lines.unwrap_or(50);
                crate::background::job_status(
                    &self.session_id,
                    job_id,
                    tail,
                    self.is_local,
                    self.executor.as_ref(),
                )
                .await
            }
            tools::ToolKind::CancelBackgroundJob => {
                let job_id = parsed
                    .job_id
                    .as_deref()
                    .ok_or_else(|| CoreError::Other("missing job_id".into()))?;
                crate::background::cancel_job(
                    &self.session_id,
                    job_id,
                    self.is_local,
                    self.executor.as_ref(),
                )
                .await
            }
            tools::ToolKind::ListBackgroundJobs => {
                Ok(crate::background::list_jobs(&self.session_id)?)
            }
            _ => tools::execute_tool_call(parsed, self.executor.as_ref()).await,
        }
    }

    /// Truncate output to `max_output_chars`, appending a notice if truncated.
    fn truncate_output(&self, output: &str) -> String {
        let total_chars = output.chars().count();
        if total_chars <= self.max_output_chars {
            return output.to_string();
        }

        // Truncate by characters, not bytes — slicing at `max_output_chars`
        // bytes could land mid-UTF-8-char and panic (#260).
        let truncated: String = output.chars().take(self.max_output_chars).collect();
        format!(
            "{truncated}\n\n[... output truncated: showed {shown} of {total} characters ...]",
            shown = self.max_output_chars,
            total = total_chars
        )
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatResponse;
    use filar_transport::CommandResult;
    use std::time::Duration;

    // ── Mock LLM client ──────────────────────────────────────────────────

    struct MockLlm {
        responses: Vec<ChatResponse>,
        call_count: std::sync::Mutex<usize>,
    }

    impl MockLlm {
        fn new(responses: Vec<ChatResponse>) -> Self {
            Self {
                responses,
                call_count: std::sync::Mutex::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for MockLlm {
        async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse> {
            let mut count = self.call_count.lock().unwrap();
            let idx = *count;
            *count += 1;
            if idx < self.responses.len() {
                Ok(self.responses[idx].clone())
            } else {
                Ok(ChatResponse::text("No more responses."))
            }
        }
    }

    // ── Mock streaming LLM client ─────────────────────────────────────

    /// Mock LLM that implements `chat_stream` — calls `on_delta` for each
    /// text chunk before returning the assembled response.
    struct MockStreamingLlm {
        /// Text chunks to emit via `on_delta`.
        deltas: Vec<String>,
        /// Final response to return from `chat_stream` / `chat`.
        final_response: ChatResponse,
    }

    #[async_trait::async_trait]
    impl LlmClient for MockStreamingLlm {
        async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse> {
            Ok(self.final_response.clone())
        }

        async fn chat_stream(
            &self,
            _request: &ChatRequest,
            on_delta: &(dyn Fn(String) + Send + Sync),
        ) -> Result<ChatResponse> {
            for d in &self.deltas {
                on_delta(d.clone());
            }
            Ok(self.final_response.clone())
        }
    }

    // ── Mock executor ────────────────────────────────────────────────────

    struct MockExecutor {
        last_command: std::sync::Mutex<String>,
    }

    #[async_trait::async_trait]
    impl CommandExecutor for MockExecutor {
        async fn run(&self, command: &str) -> Result<CommandResult> {
            *self.last_command.lock().unwrap() = command.to_string();
            Ok(CommandResult {
                stdout: format!("output of: {command}"),
                stderr: String::new(),
                exit_code: Some(0),
                duration: Duration::from_millis(10),
                cwd: None,
            })
        }

        async fn cancel(&self) -> Result<()> {
            Ok(())
        }
    }

    // ── Mock confirmer ───────────────────────────────────────────────────

    struct MockConfirmer {
        approve: bool,
    }

    #[async_trait::async_trait]
    impl CommandConfirmer for MockConfirmer {
        async fn confirm(&self, _command: &str, _explanation: &str, _destructive: bool) -> Result<bool> {
            Ok(self.approve)
        }
    }

    // ── Tests ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn agent_text_response() {
        let llm = Arc::new(MockLlm::new(vec![ChatResponse::text("Hello!")]));
        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .build()
            .unwrap();

        let result = agent.run("say hello", &[]).await.unwrap();
        assert_eq!(result, "Hello!");
    }

    #[tokio::test]
    async fn agent_tool_call_then_text() {
        // First response: tool call. Second response: text.
        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "echo hello",
                "explanation": "Print hello"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Done! The output was: output of: echo hello"),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Always)
            .build()
            .unwrap();

        let result = agent.run("say hello via command", &[]).await.unwrap();
        assert!(result.contains("Done!"));
    }

    #[tokio::test]
    async fn agent_tool_call_denied() {
        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "rm -rf /tmp",
                "explanation": "Delete temp files"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Okay, I won't delete anything."),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: false }); // Deny!

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Always)
            .build()
            .unwrap();

        let result = agent.run("delete temp files", &[]).await.unwrap();
        assert!(result.contains("Okay"));
    }

    #[tokio::test]
    async fn agent_tool_call_auto_approved() {
        // In Allowlist mode, read-only commands are auto-approved (no confirmer call).
        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "ls -la",
                "explanation": "List files"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Files listed."),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });
        // Confirmer that always denies — but it should never be called.
        let confirmer = Arc::new(MockConfirmer { approve: false });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Allowlist)
            .build()
            .unwrap();

        let result = agent.run("list files", &[]).await.unwrap();
        assert!(result.contains("Files listed"));
    }

    #[tokio::test]
    async fn agent_substitutes_secret_inserted_after_build() {
        // #364 regression: a Ctrl+P secret registered AFTER the agent is
        // built must be substituted into tool commands (heredoc included),
        // and the real value must never reach the LLM-visible output.
        let heredoc_cmd = "printf '%s\n' \"$FILAR_SECRET_1\" | sudo -S tee /tmp/f <<'EOF'\n<x/>\nEOF";
        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": heredoc_cmd,
                "explanation": "Write file with sudo"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Done."),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });
        let provider = Arc::new(filar_core::StaticSecretProvider::new());

        let finished: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink_events = finished.clone();
        let sink: EventSink = Arc::new(move |event| {
            if let AgentEvent::CommandFinished { output, .. } = event {
                sink_events.lock().unwrap().push(output);
            }
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor.clone())
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Always)
            .secret_provider(provider.clone() as Arc<dyn filar_core::SecretProvider>)
            .event_sink(sink)
            .build()
            .unwrap();

        // Secret appears AFTER build — like a real Ctrl+P during a session.
        provider.insert("$FILAR_SECRET_1", "hunter2");

        let result = agent.run("write the file", &[]).await.unwrap();
        assert!(result.contains("Done."));

        // The inner executor received the substituted command.
        let executed = executor.last_command.lock().unwrap().clone();
        assert!(
            executed.contains("\"hunter2\""),
            "secret not substituted in executed command: {executed}"
        );
        assert!(
            !executed.contains("$FILAR_SECRET_1"),
            "placeholder not substituted: {executed}"
        );

        // The LLM-visible output is sanitised — no real secret.
        let outputs = finished.lock().unwrap();
        assert_eq!(outputs.len(), 1);
        assert!(
            !outputs[0].contains("hunter2"),
            "secret leaked into tool output: {}",
            outputs[0]
        );
        assert!(outputs[0].contains("$FILAR_SECRET_1"));
    }

    #[tokio::test]
    async fn agent_max_iterations() {
        // Always return a tool call — never produce text.
        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "echo loop",
                "explanation": "Looping"
            }),
        }]);

        // Need enough responses for all iterations.
        let responses: Vec<ChatResponse> = (0..20).map(|_| tool_call.clone()).collect();
        let llm = Arc::new(MockLlm::new(responses));

        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Never)
            .max_iterations(3)
            .build()
            .unwrap();

        let result = agent.run("loop forever", &[]).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("maximum iterations"));
    }

    #[test]
    fn truncate_short_output() {
        let agent = Agent::builder()
            .llm(Arc::new(MockLlm::new(vec![])))
            .executor(Arc::new(MockExecutor {
                last_command: std::sync::Mutex::new(String::new()),
            }))
            .confirmer(Arc::new(MockConfirmer { approve: true }))
            .max_output_chars(100)
            .build()
            .unwrap();

        let output = "short output";
        assert_eq!(agent.truncate_output(output), output);
    }

    #[test]
    fn truncate_long_output() {
        let agent = Agent::builder()
            .llm(Arc::new(MockLlm::new(vec![])))
            .executor(Arc::new(MockExecutor {
                last_command: std::sync::Mutex::new(String::new()),
            }))
            .confirmer(Arc::new(MockConfirmer { approve: true }))
            .max_output_chars(10)
            .build()
            .unwrap();

        let output = "0123456789ABCDEF"; // 16 chars
        let truncated = agent.truncate_output(output);
        assert!(truncated.starts_with("0123456789"));
        assert!(truncated.contains("truncated"));
        assert!(truncated.contains("16"));
    }

    #[test]
    fn truncate_output_multibyte_no_panic() {
        let agent = Agent::builder()
            .llm(Arc::new(MockLlm::new(vec![])))
            .executor(Arc::new(MockExecutor {
                last_command: std::sync::Mutex::new(String::new()),
            }))
            .confirmer(Arc::new(MockConfirmer { approve: true }))
            .max_output_chars(10)
            .build()
            .unwrap();

        // Cyrillic: 2 bytes per char. Byte 10 lands inside a char,
        // which previously panicked at `&output[..10]`.
        let output = "абвгдеёжзийк"; // 12 chars, 24 bytes
        let truncated = agent.truncate_output(output);
        assert!(
            truncated.starts_with("абвгдеёжзи"),
            "must keep first 10 chars, got: {truncated}"
        );
        assert!(truncated.contains("truncated"));
        assert!(truncated.contains("12"));
    }

    #[test]
    fn ssh_prompt_states_persistence() {
        // SSH mode: prompt should mention persistence.
        let prompt = build_system_prompt(false, None, false);
        assert!(
            prompt.contains("DOES persist") || prompt.contains("carry over"),
            "SSH prompt should mention shell state persistence, got: {prompt}"
        );
        assert!(
            !prompt.contains("does NOT persist"),
            "SSH prompt should NOT say state does not persist"
        );
    }

    #[test]
    fn local_prompt_states_no_persistence() {
        // Local mode: prompt should say state does NOT persist.
        let prompt = build_system_prompt(true, None, false);
        assert!(
            prompt.contains("does NOT persist"),
            "Local prompt should mention state does NOT persist, got: {prompt}"
        );
    }

    #[test]
    fn prompt_mirrors_user_language() {
        // The prompt must NOT hardcode Russian as the response language.
        let prompt = build_system_prompt(true, None, false);
        assert!(
            !prompt.contains("Russian"),
            "Prompt should not hardcode Russian, got: {prompt}"
        );
        // The prompt must instruct the model to mirror the user's language.
        assert!(
            prompt.contains("user's") && prompt.contains("same language"),
            "Prompt should mention mirroring the user's language, got: {prompt}"
        );
        // Raw command output must not be translated.
        assert!(
            prompt.contains("must NOT be translated"),
            "Prompt should state that command output is not translated, got: {prompt}"
        );
    }

    // ── Event sink tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn event_sink_sequence_tool_call() {
        // DoD test: mock-LLM with one tool call → sink receives
        // Started → CommandProposed → CommandFinished → Finished.
        use std::sync::Mutex;

        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "echo hello",
                "explanation": "Print hello"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Done!"),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });

        // Collect events via an EventSink.
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Never)
            .event_sink(sink)
            .build()
            .unwrap();

        let result = agent.run("say hello", &[]).await.unwrap();
        assert_eq!(result, "Done!");

        let received = events.lock().unwrap();
        assert_eq!(received.len(), 4, "expected 4 events, got {received:?}");

        // Verify the event sequence.
        assert!(matches!(&received[0], AgentEvent::Started), "first event should be Started, got {:?}", received[0]);
        assert!(matches!(&received[1], AgentEvent::CommandProposed { command, .. } if command == "echo hello"),
            "second event should be CommandProposed, got {:?}", received[1]);
        assert!(matches!(&received[2], AgentEvent::CommandFinished { command, denied, .. } if command == "echo hello" && !denied),
            "third event should be CommandFinished (not denied), got {:?}", received[2]);
        assert!(matches!(&received[3], AgentEvent::Finished(text) if text == "Done!"),
            "fourth event should be Finished, got {:?}", received[3]);
    }

    #[tokio::test]
    async fn event_sink_denied_command() {
        // When a command is denied, sink should receive CommandFinished with denied=true.
        use std::sync::Mutex;

        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "rm -rf /tmp",
                "explanation": "Delete temp files"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Okay, I won't delete anything."),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: false }); // Deny!

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Always)
            .event_sink(sink)
            .build()
            .unwrap();

        let _ = agent.run("delete temp files", &[]).await.unwrap();

        let received = events.lock().unwrap();
        // Started → CommandProposed → CommandFinished(denied=true) → Finished
        assert_eq!(received.len(), 4, "expected 4 events, got {received:?}");
        assert!(matches!(&received[2], AgentEvent::CommandFinished { denied: true, .. }),
            "third event should be CommandFinished with denied=true, got {:?}", received[2]);
    }

    #[tokio::test]
    async fn event_sink_streaming_text_delta() {
        // DoD 2: Mock-LLM with streaming → sink receives TextDelta before Finished.
        use std::sync::Mutex;

        let llm = Arc::new(MockStreamingLlm {
            deltas: vec!["Hello".into(), " world".into(), "!".into()],
            final_response: ChatResponse::text("Hello world!"),
        });

        let executor = Arc::new(MockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Never)
            .event_sink(sink)
            .build()
            .unwrap();

        let result = agent.run("say hello", &[]).await.unwrap();
        assert_eq!(result, "Hello world!");

        let received = events.lock().unwrap();
        // Expected: Started → TextDelta("Hello") → TextDelta(" world") → TextDelta("!") → Finished
        assert_eq!(received.len(), 5, "expected 5 events, got {received:?}");
        assert!(matches!(&received[0], AgentEvent::Started),
            "first event should be Started, got {:?}", received[0]);
        assert!(matches!(&received[1], AgentEvent::TextDelta(s) if s == "Hello"),
            "second event should be TextDelta, got {:?}", received[1]);
        assert!(matches!(&received[2], AgentEvent::TextDelta(s) if s == " world"),
            "third event should be TextDelta, got {:?}", received[2]);
        assert!(matches!(&received[3], AgentEvent::TextDelta(s) if s == "!"),
            "fourth event should be TextDelta, got {:?}", received[3]);
        assert!(matches!(&received[4], AgentEvent::Finished(text) if text == "Hello world!"),
            "last event should be Finished, got {:?}", received[4]);
    }

    #[tokio::test]
    async fn cancellation_emits_cancelled_event() {
        // DoD test: agent with CancellationToken — triggering it mid-run
        // emits Started → Cancelled and returns an error.
        use std::sync::Mutex;

        // Mock LLM that hangs forever — simulates a long LLM request.
        struct HangingLlm;
        #[async_trait::async_trait]
        impl LlmClient for HangingLlm {
            async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        let llm = Arc::new(HangingLlm);
        let executor = Arc::new(MockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let token = CancellationToken::new();
        let token_clone = token.clone();

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .event_sink(sink)
            .cancellation(token)
            .build()
            .unwrap();

        // Spawn the agent run in a task.
        let handle = tokio::spawn(async move {
            agent.run("test", &[]).await
        });

        // Give the agent a moment to start, then cancel.
        tokio::time::sleep(Duration::from_millis(50)).await;
        token_clone.cancel();

        // The agent should return an error (cancelled).
        let result = handle.await.unwrap();
        assert!(result.is_err(), "agent should return error on cancellation");

        let received = events.lock().unwrap();
        // Expected: Started → Cancelled
        assert_eq!(received.len(), 2, "expected 2 events, got {received:?}");
        assert!(matches!(&received[0], AgentEvent::Started),
            "first event should be Started, got {:?}", received[0]);
        assert!(matches!(&received[1], AgentEvent::Cancelled),
            "second event should be Cancelled, got {:?}", received[1]);
    }

    #[tokio::test]
    async fn confirm_timeout_treats_as_denied() {
        // DoD test: agent with confirm_timeout — confirmer that never
        // responds → timeout fires, command treated as denied.
        use std::sync::Mutex;

        // Confirmer that hangs forever — never responds.
        struct HangingConfirmer;
        #[async_trait::async_trait]
        impl CommandConfirmer for HangingConfirmer {
            async fn confirm(&self, _: &str, _: &str, _: bool) -> Result<bool> {
                std::future::pending::<()>().await;
                unreachable!()
            }
        }

        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "echo hello",
                "explanation": "Print hello"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Done!"),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let confirmer = Arc::new(HangingConfirmer);

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Always)
            .event_sink(sink)
            .confirm_timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let result = agent.run("say hello", &[]).await.unwrap();
        assert_eq!(result, "Done!");

        let received = events.lock().unwrap();
        // Expected: Started → CommandProposed → CommandFinished(denied=true, "timed out") → Finished
        assert!(received.len() >= 3, "expected at least 3 events, got {received:?}");
        assert!(matches!(&received[0], AgentEvent::Started),
            "first event should be Started, got {:?}", received[0]);
        assert!(matches!(&received[1], AgentEvent::CommandProposed { .. }),
            "second event should be CommandProposed, got {:?}", received[1]);
        assert!(matches!(&received[2], AgentEvent::CommandFinished { denied: true, output, .. } if output.contains("timed out")),
            "third event should be CommandFinished with denied=true and timeout message, got {:?}", received[2]);
    }

    #[tokio::test]
    async fn command_timeout_cancels_executor() {
        // DoD test: agent with command_timeout — executor that hangs forever
        // → timeout fires, executor.cancel() is called, agent continues.
        use std::sync::Mutex;

        /// Executor that hangs forever in `run()` and tracks `cancel()` calls.
        struct HangingExecutor {
            cancel_count: Mutex<usize>,
        }

        #[async_trait::async_trait]
        impl CommandExecutor for HangingExecutor {
            async fn run(&self, _command: &str) -> Result<CommandResult> {
                std::future::pending::<()>().await;
                unreachable!()
            }

            async fn cancel(&self) -> Result<()> {
                *self.cancel_count.lock().unwrap() += 1;
                Ok(())
            }
        }

        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                // Must not be a long `sleep` — those are rejected by long_wait
                // policy (#323) before the executor runs.
                "command": "make all",
                "explanation": "Hang until agent timeout"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Done!"),
        ]));

        let executor = Arc::new(HangingExecutor {
            cancel_count: Mutex::new(0),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor.clone())
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Never)
            .event_sink(sink)
            .command_timeout(Duration::from_millis(100))
            .build()
            .unwrap();

        let result = agent.run("run make", &[]).await.unwrap();
        assert_eq!(result, "Done!");

        // executor.cancel() must have been called on timeout.
        let cancel_count = *executor.cancel_count.lock().unwrap();
        assert_eq!(cancel_count, 1, "executor.cancel() should be called once on timeout");

        let received = events.lock().unwrap();
        // Expected: Started → CommandProposed → CommandFinished(output contains timed out) → Finished
        assert!(received.len() >= 3, "expected at least 3 events, got {received:?}");
        assert!(matches!(&received[0], AgentEvent::Started),
            "first event should be Started, got {:?}", received[0]);
        assert!(matches!(&received[1], AgentEvent::CommandProposed { .. }),
            "second event should be CommandProposed, got {:?}", received[1]);
        assert!(matches!(&received[2], AgentEvent::CommandFinished { denied: false, output, .. } if output.contains("timed out")),
            "third event should be CommandFinished with timeout message, got {:?}", received[2]);
        assert!(matches!(&received[2], AgentEvent::CommandFinished { output, .. } if output.contains("Ctrl+T")),
            "timeout message should include long-wait guidance, got {:?}", received[2]);
    }

    #[tokio::test]
    async fn long_wait_sleep_rejected_without_executor() {
        use std::sync::Mutex;

        struct CountingExecutor {
            runs: Mutex<usize>,
        }

        #[async_trait::async_trait]
        impl CommandExecutor for CountingExecutor {
            async fn run(&self, _command: &str) -> Result<CommandResult> {
                *self.runs.lock().unwrap() += 1;
                Ok(CommandResult {
                    stdout: "should not run".into(),
                    stderr: String::new(),
                    exit_code: Some(0),
                    duration: Duration::from_millis(1),
                    cwd: None,
                })
            }

            async fn cancel(&self) -> Result<()> {
                Ok(())
            }
        }

        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "sleep 120 && tail /tmp/x.log",
                "explanation": "Wait for pull"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("Switched to background poll."),
        ]));
        let executor = Arc::new(CountingExecutor {
            runs: Mutex::new(0),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor.clone())
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Never)
            .event_sink(sink)
            .build()
            .unwrap();

        let result = agent.run("wait for pull", &[]).await.unwrap();
        assert!(result.contains("background") || result.contains("Switched"));
        assert_eq!(
            *executor.runs.lock().unwrap(),
            0,
            "long sleep must not reach the executor"
        );

        let received = events.lock().unwrap();
        assert!(
            received.iter().any(|e| matches!(
                e,
                AgentEvent::CommandFinished { denied: false, output, .. }
                    if output.contains("refused") && output.contains("120")
            )),
            "expected CommandFinished with long-wait refusal, got {received:?}"
        );
    }

    #[test]
    fn system_prompt_matches_eval_snapshot() {
        // The eval harness (eval/prompts/agent-system.txt) must test filar's
        // real system prompt. This snapshot is the canonical SSH/POSIX remote
        // variant — filar's primary scenario (build_system_prompt(false, None,
        // false)). If the prompt in code changes, update the eval snapshot to
        // match; this test fails on drift.
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../eval/prompts/agent-system.txt");
        let snapshot = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("failed to read {}: {e}", path.display()));
        let expected = build_system_prompt(false, None, false);
        assert_eq!(
            snapshot.trim_end(),
            expected.trim_end(),
            "eval/prompts/agent-system.txt is out of sync with build_system_prompt(false, None, false)"
        );
    }

    // ── Explain mode agent-level tests ─────────────────────────────────

    #[test]
    fn build_explain_mode_appends_safe_mode_prompt() {
        let agent = Agent::builder()
            .llm(Arc::new(MockLlm::new(vec![])))
            .executor(Arc::new(MockExecutor {
                last_command: std::sync::Mutex::new(String::new()),
            }))
            .confirmer(Arc::new(MockConfirmer { approve: true }))
            .confirm_mode(CommandConfirmMode::Explain)
            .build()
            .unwrap();

        assert!(
            agent.system_prompt.contains("SAFE MODE IS ACTIVE"),
            "system prompt must contain SAFE MODE block in Explain mode"
        );
    }

    #[test]
    fn build_non_explain_mode_no_safe_mode_prompt() {
        for mode in [
            CommandConfirmMode::Always,
            CommandConfirmMode::Allowlist,
            CommandConfirmMode::Never,
        ] {
            let agent = Agent::builder()
                .llm(Arc::new(MockLlm::new(vec![])))
                .executor(Arc::new(MockExecutor {
                    last_command: std::sync::Mutex::new(String::new()),
                }))
                .confirmer(Arc::new(MockConfirmer { approve: true }))
                .confirm_mode(mode)
                .build()
                .unwrap();

            assert!(
                !agent.system_prompt.contains("SAFE MODE IS ACTIVE"),
                "system prompt must NOT contain SAFE MODE block in {:?} mode",
                mode
            );
        }
    }

    #[tokio::test]
    async fn explain_mode_rejects_missing_explanation() {
        // Tool call without explanation — should be rejected, executor not called.
        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({"command": "ls"}),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::text("OK, let me explain first."),
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor.clone())
            .confirmer(Arc::new(MockConfirmer { approve: true }))
            .confirm_mode(CommandConfirmMode::Explain)
            .build()
            .unwrap();

        let result = agent.run("list files", &[]).await.unwrap();
        assert!(result.contains("explain"));
        // Executor must NOT have been called — no command was executed.
        assert!(
            executor.last_command.lock().unwrap().is_empty(),
            "executor must not be called when explanation is missing"
        );
    }

    #[tokio::test]
    async fn explain_mode_stops_after_retry_limit() {
        // Repeatedly send tool calls without explanation.
        let tool_call = ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({"command": "ls"}),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call.clone(),
            tool_call.clone(),
            tool_call,
        ]));

        let executor = Arc::new(MockExecutor {
            last_command: std::sync::Mutex::new(String::new()),
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor)
            .confirmer(Arc::new(MockConfirmer { approve: true }))
            .confirm_mode(CommandConfirmMode::Explain)
            .max_iterations(10)
            .build()
            .unwrap();

        let result = agent.run("list files", &[]).await;
        assert!(result.is_err(), "agent should stop after retry limit");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("explanation"),
            "error should mention missing explanation, got: {err}"
        );
    }

    // ── Command arbiter tests (#353) ────────────────────────────────────

    struct StaticArbiterLlm {
        text: String,
    }

    #[async_trait::async_trait]
    impl LlmClient for StaticArbiterLlm {
        async fn chat(&self, _request: &ChatRequest) -> Result<ChatResponse> {
            Ok(ChatResponse::text(self.text.clone()))
        }
    }

    fn tool_call_echo() -> ChatResponse {
        ChatResponse::tool_calls("", vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "echo hello",
                "explanation": "Print hello"
            }),
        }])
    }

    #[tokio::test]
    async fn arbiter_unavailable_does_not_block_confirm() {
        use std::sync::Mutex;

        let llm = Arc::new(MockLlm::new(vec![
            tool_call_echo(),
            ChatResponse::text("Done."),
        ]));
        let executor = Arc::new(MockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });
        let arbiter = Arc::new(StaticArbiterLlm {
            text: "not valid json".into(),
        });

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor.clone())
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Always)
            .event_sink(sink)
            .arbiter_llm(Some(arbiter))
            .arbiter_enabled(true)
            .arbiter_local_context()
            .build()
            .unwrap();

        let _ = agent.run("say hello", &[]).await.unwrap();
        assert_eq!(
            *executor.last_command.lock().unwrap(),
            "echo hello",
            "command must execute when arbiter is unavailable"
        );

        let received = events.lock().unwrap();
        assert!(
            received.iter().any(|e| matches!(
                e,
                AgentEvent::CommandAudited { unavailable: true, .. }
            )),
            "expected CommandAudited unavailable, got {received:?}"
        );
        assert!(
            received.iter().any(|e| matches!(
                e,
                AgentEvent::CommandFinished { denied: false, .. }
            )),
            "command must not be auto-denied"
        );
    }

    #[tokio::test]
    async fn arbiter_mismatch_does_not_auto_deny() {
        use std::sync::Mutex;

        let llm = Arc::new(MockLlm::new(vec![
            tool_call_echo(),
            ChatResponse::text("Done."),
        ]));
        let executor = Arc::new(MockExecutor {
            last_command: Mutex::new(String::new()),
        });
        let confirmer = Arc::new(MockConfirmer { approve: true });
        let arbiter = Arc::new(StaticArbiterLlm {
            text: r#"{"verdict":"MISMATCH","reason":"Command prints hello but explanation claims goodbye."}"#
                .into(),
        });

        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let events_clone = events.clone();
        let sink: EventSink = Arc::new(move |event: AgentEvent| {
            events_clone.lock().unwrap().push(event);
        });

        let agent = Agent::builder()
            .llm(llm)
            .executor(executor.clone())
            .confirmer(confirmer)
            .confirm_mode(CommandConfirmMode::Always)
            .event_sink(sink)
            .arbiter_llm(Some(arbiter))
            .arbiter_enabled(true)
            .arbiter_local_context()
            .build()
            .unwrap();

        let _ = agent.run("say hello", &[]).await.unwrap();
        assert_eq!(
            *executor.last_command.lock().unwrap(),
            "echo hello",
            "MISMATCH verdict must not auto-deny the command"
        );

        let received = events.lock().unwrap();
        assert!(
            received.iter().any(|e| matches!(
                e,
                AgentEvent::CommandAudited { verdict, unavailable: false, .. }
                    if verdict == "MISMATCH"
            )),
            "expected MISMATCH CommandAudited, got {received:?}"
        );
    }
}
