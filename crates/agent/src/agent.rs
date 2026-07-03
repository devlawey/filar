//! Agent loop: orchestrates LLM ↔ tool execution with safety checks.
//!
//! The [`Agent`] struct ties together an [`LlmClient`], a [`CommandExecutor`],
//! and a [`CommandConfirmer`] to implement the core agent loop:
//!
//! ```text
//! user prompt → LLM → (tool call?) → confirm → execute → result → LLM → … → final answer
//! ```

use std::sync::Arc;

use tracing::{info, warn};

use filar_core::{CommandConfirmMode, CoreError, Result};
use filar_transport::CommandExecutor;

use crate::{
    security::{self, CommandConfirmer, ConfirmDecision},
    tools::{self},
    ChatMessage, ChatRequest, ChatResponse, LlmClient, MessageRole, ToolCall,
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
        Ok(Agent {
            llm: self.llm.ok_or_else(|| CoreError::Other("LLM client not set".into()))?,
            executor: self.executor.ok_or_else(|| CoreError::Other("executor not set".into()))?,
            confirmer: self.confirmer.ok_or_else(|| CoreError::Other("confirmer not set".into()))?,
            confirm_mode: self.confirm_mode,
            max_iterations: self.max_iterations,
            max_output_chars: self.max_output_chars,
            system_prompt: self.system_prompt.unwrap_or_else(||
                build_system_prompt(false, None, cfg!(windows))
            ),
        })
    }
}

impl Default for AgentBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl Agent {
    /// Create a new builder for configuring an agent.
    pub fn builder() -> AgentBuilder {
        AgentBuilder::new()
    }

    /// Run the agent loop with a user prompt and optional conversation history.
    ///
    /// Returns the final text response from the LLM, or an error if the
    /// loop exceeds the maximum iterations or encounters a failure.
    pub async fn run(&self, user_prompt: &str, history: &[ChatMessage]) -> Result<String> {
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

            let response = self.llm.chat(&request).await?;

            match response {
                ChatResponse::Text(text) => {
                    info!(iteration, "agent produced final text response");
                    return Ok(text);
                }
                ChatResponse::ToolCalls(tool_calls) => {
                    info!(iteration, count = tool_calls.len(), "LLM requested tool calls");

                    // Add the assistant message with tool calls to history.
                    let assistant_msg = ChatMessage {
                        role: MessageRole::Assistant,
                        content: String::new(),
                        tool_calls: tool_calls.clone(),
                        tool_call_id: None,
                    };
                    messages.push(assistant_msg);

                    // Process each tool call.
                    for tc in &tool_calls {
                        let result = self.process_tool_call(tc).await?;
                        messages.push(result);
                    }
                }
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

        match decision {
            ConfirmDecision::Blocked(reason) => {
                warn!(command = %parsed.command, reason = %reason, "command blocked by security");
                return Ok(ChatMessage::tool(
                    &tc.id,
                    format!("Error: command blocked by security policy: {reason}"),
                ));
            }
            ConfirmDecision::AutoApproved => {
                info!(command = %parsed.command, "command auto-approved");
            }
            ConfirmDecision::NeedsConfirmation => {
                let approved = self
                    .confirmer
                    .confirm(&parsed.command, &parsed.explanation, destructive)
                    .await?;

                if !approved {
                    info!(command = %parsed.command, "command denied by user");
                    return Ok(ChatMessage::tool(
                        &tc.id,
                        "Command denied by user. Try a different approach.".to_string(),
                    ));
                }
                info!(command = %parsed.command, "command approved by user");
            }
        }

        // Execute the tool.
        let output = match tools::execute_tool_call(&parsed, self.executor.as_ref()).await {
            Ok(o) => o,
            Err(e) => {
                warn!(command = %parsed.command, error = %e, "tool execution failed");
                return Ok(ChatMessage::tool(
                    &tc.id,
                    format!("Error executing command: {e}"),
                ));
            }
        };

        // Truncate output if too long.
        let truncated = self.truncate_output(&output);

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
                Ok(ChatResponse::Text("No more responses.".into()))
            }
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
        let llm = Arc::new(MockLlm::new(vec![ChatResponse::Text("Hello!".into())]));
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
        let tool_call = ChatResponse::ToolCalls(vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "echo hello",
                "explanation": "Print hello"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::Text("Done! The output was: output of: echo hello".into()),
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
        let tool_call = ChatResponse::ToolCalls(vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "rm -rf /tmp",
                "explanation": "Delete temp files"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::Text("Okay, I won't delete anything.".into()),
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
        let tool_call = ChatResponse::ToolCalls(vec![ToolCall {
            id: "call_1".into(),
            name: "run_command".into(),
            arguments: serde_json::json!({
                "command": "ls -la",
                "explanation": "List files"
            }),
        }]);

        let llm = Arc::new(MockLlm::new(vec![
            tool_call,
            ChatResponse::Text("Files listed.".into()),
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
        let tool_call = ChatResponse::ToolCalls(vec![ToolCall {
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
}
