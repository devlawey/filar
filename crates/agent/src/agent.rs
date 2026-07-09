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
"{transport_desc} \
You have tools to run commands, read files, and list directories. \
\
IMPORTANT: Determine the language of the user's FIRST request in this conversation, \
and write ALL of your explanations, summaries, questions, and the final answer in \
that same language. Keep this language consistent for the entire session. Do NOT \
default to any fixed language. Note: raw command output (stdout/stderr) is passed \
through as-is and must NOT be translated — only your own prose around it follows the \
user's language.\
\
Rules:\
1. Always explain what you're about to do before calling a tool.\
2. Prefer read-only commands before making changes.\
3. Be cautious with destructive commands (rm, dd, mkfs, Remove-Item, Format-Volume, etc.).\
4. If a command is denied by the user, do not retry it — try a different approach.\
5. Summarize the results concisely after each command.\
6. When the task is complete, provide a clear final answer in the user's language.\
7. If you need information from the user (e.g. a password, a choice between options), ask them directly in your text response — do not try to use interactive prompts in commands. Wait for their reply before continuing.\
8. Never put passwords or secrets directly in commands. If a password is needed, ask the user to provide it. The user can press Ctrl+P to enter the password in a secure masked input field. The password will be stored as a secret variable (e.g. $FILAR_SECRET_1) and you will be told the variable name. Use this variable directly in your commands (e.g. echo \"user:$FILAR_SECRET_1\" | chpasswd). The actual value is substituted at execution time — you never see the real password. Do not try to echo or print secret variables.\
9. NEVER run interactive commands (vim, nano, top, htop, less, man, mc, screen, tmux, ssh, etc.). These commands take over the terminal and will hang indefinitely. Instead, use non-interactive alternatives: 'cat file' instead of 'less file', 'grep -n pattern file' instead of 'vim file', 'head -n 50 file' to preview. For editing files, use 'sed' or 'tee' with heredocs.
{shell_desc}"
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

    /// Convenience: set the system prompt for local execution.
    pub fn local_mode(self) -> Self {
        self.system_prompt(build_system_prompt(true, None, cfg!(windows)))
    }

    /// Convenience: set the system prompt for SSH remote execution.
    pub fn ssh_mode(self, ssh_info: Option<&str>) -> Self {
        self.system_prompt(build_system_prompt(false, ssh_info, false))
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
        Ok(Agent {
            llm: self.llm.ok_or_else(|| CoreError::Other("LLM client not set".into()))?,
            executor,
            confirmer: self.confirmer.ok_or_else(|| CoreError::Other("confirmer not set".into()))?,
            confirm_mode: self.confirm_mode,
            max_iterations: self.max_iterations,
            max_output_chars: self.max_output_chars,
            system_prompt: self.system_prompt.unwrap_or_else(||
                build_system_prompt(false, None, cfg!(windows))
            ),
            on_text_delta: self.on_text_delta,
            event_sink: self.event_sink,
            cancellation: self.cancellation,
            confirm_timeout: self.confirm_timeout,
            command_timeout: self.command_timeout,
            secret_provider,
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

        let tool_defs = tools::tool_definitions();

        info!(prompt = %user_prompt, "agent loop started");

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

            if response.has_tool_calls() {
                let tool_calls = response.tool_calls.clone();
                info!(iteration, count = tool_calls.len(), "LLM requested tool calls");

                // Add the assistant message with tool calls (and any preamble text) to history.
                let assistant_msg = ChatMessage::assistant_with_tools(
                    &response.text,
                    tool_calls.clone(),
                );
                messages.push(assistant_msg);

                // Process each tool call.
                for tc in &tool_calls {
                    let result = self.process_tool_call(tc).await?;
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
    async fn process_tool_call(&self, tc: &ToolCall) -> Result<ChatMessage> {
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

        // Check security / confirmation.
        let decision = security::tool_needs_confirmation(
            parsed.kind,
            &parsed.command,
            self.confirm_mode,
        );

        let destructive = security::is_destructive(&parsed.command);

        // Emit CommandProposed before any confirmation logic.
        self.emit(AgentEvent::CommandProposed {
            command: parsed.command.clone(),
            explanation: parsed.explanation.clone(),
            destructive,
        });

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
                let confirm_fut = self
                    .confirmer
                    .confirm(&parsed.command, &parsed.explanation, destructive);
                let approved = if let Some(ct) = self.confirm_timeout {
                    match tokio::time::timeout(ct, with_cancellation(self.cancellation.as_ref(), confirm_fut)).await {
                        Ok(result) => result?,
                        Err(_) => {
                            info!(command = %parsed.command, "confirmation timed out");
                            self.emit(AgentEvent::CommandFinished {
                                command: parsed.command.clone(),
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
                    info!(command = %parsed.command, "command denied by user");
                    self.emit(AgentEvent::CommandFinished {
                        command: parsed.command.clone(),
                        output: String::new(),
                        denied: true,
                    });
                    return Ok(ChatMessage::tool(
                        &tc.id,
                        "Command denied by user. Try a different approach.".to_string(),
                    ));
                }
                info!(command = %parsed.command, "command approved by user");
            }
        }

        // Execute the tool, with optional cancellation and command timeout.
        let exec_fut = tools::execute_tool_call(&parsed, self.executor.as_ref());
        let output = if let Some(ct) = self.command_timeout {
            match tokio::time::timeout(ct, with_cancellation(self.cancellation.as_ref(), exec_fut)).await {
                Ok(Ok(o)) => o,
                Ok(Err(e)) if e.to_string() == "cancelled" => {
                    // Cancellation — kill the running command.
                    let _ = self.executor.cancel().await;
                    return Err(e);
                }
                Ok(Err(e)) => {
                    warn!(command = %parsed.command, error = %e, "tool execution failed");
                    self.emit(AgentEvent::CommandFinished {
                        command: parsed.command.clone(),
                        output: format!("Error: {e}"),
                        denied: false,
                    });
                    return Ok(ChatMessage::tool(
                        &tc.id,
                        format!("Error executing command: {e}"),
                    ));
                }
                Err(_) => {
                    warn!(command = %parsed.command, "command timed out");
                    let _ = self.executor.cancel().await;
                    self.emit(AgentEvent::CommandFinished {
                        command: parsed.command.clone(),
                        output: "Command timed out".to_string(),
                        denied: false,
                    });
                    return Ok(ChatMessage::tool(&tc.id, "Command timed out.".to_string()));
                }
            }
        } else {
            match with_cancellation(self.cancellation.as_ref(), exec_fut).await {
                Ok(o) => o,
                Err(e) if e.to_string() == "cancelled" => {
                    let _ = self.executor.cancel().await;
                    return Err(e);
                }
                Err(e) => {
                    warn!(command = %parsed.command, error = %e, "tool execution failed");
                    self.emit(AgentEvent::CommandFinished {
                        command: parsed.command.clone(),
                        output: format!("Error: {e}"),
                        denied: false,
                    });
                    return Ok(ChatMessage::tool(
                        &tc.id,
                        format!("Error executing command: {e}"),
                    ));
                }
            }
        };

        // Truncate output if too long.
        let truncated = self.truncate_output(&output);

        self.emit(AgentEvent::CommandFinished {
            command: parsed.command.clone(),
            output: truncated.clone(),
            denied: false,
        });

        Ok(ChatMessage::tool(&tc.id, truncated))
    }

    /// Truncate output to `max_output_chars`, appending a notice if truncated.
    fn truncate_output(&self, output: &str) -> String {
        if output.len() <= self.max_output_chars {
            return output.to_string();
        }

        let truncated = &output[..self.max_output_chars];
        format!(
            "{truncated}\n\n[... output truncated: showed {shown} of {total} characters ...]",
            shown = self.max_output_chars,
            total = output.len()
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
                "command": "sleep 999",
                "explanation": "Long sleep"
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

        let result = agent.run("run sleep", &[]).await.unwrap();
        assert_eq!(result, "Done!");

        // executor.cancel() must have been called on timeout.
        let cancel_count = *executor.cancel_count.lock().unwrap();
        assert_eq!(cancel_count, 1, "executor.cancel() should be called once on timeout");

        let received = events.lock().unwrap();
        // Expected: Started → CommandProposed → CommandFinished(output="Command timed out") → Finished
        assert!(received.len() >= 3, "expected at least 3 events, got {received:?}");
        assert!(matches!(&received[0], AgentEvent::Started),
            "first event should be Started, got {:?}", received[0]);
        assert!(matches!(&received[1], AgentEvent::CommandProposed { .. }),
            "second event should be CommandProposed, got {:?}", received[1]);
        assert!(matches!(&received[2], AgentEvent::CommandFinished { denied: false, output, .. } if output.contains("timed out")),
            "third event should be CommandFinished with timeout message, got {:?}", received[2]);
    }
}
