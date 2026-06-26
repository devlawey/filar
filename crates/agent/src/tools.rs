//! Tool definitions and execution for the agent.
//!
//! The agent exposes three tools to the LLM:
//! - `run_command` — execute a shell command on the current target.
//! - `read_file` — read a file's contents (wrapper around `cat`).
//! - `list_dir` — list directory contents (wrapper around `ls`).
//!
//! All tools are implemented as wrappers over shell commands to maintain the
//! **zero-install** invariant — no files are created on the remote machine.

use serde::Deserialize;
use tracing::{debug, info};

use filar_core::{CoreError, Result};
use filar_transport::CommandExecutor;

use crate::ToolDef;

// ---------------------------------------------------------------------------
// Tool names
// ---------------------------------------------------------------------------

pub const TOOL_RUN_COMMAND: &str = "run_command";
pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_LIST_DIR: &str = "list_dir";

// ---------------------------------------------------------------------------
// Tool parameter structs
// ---------------------------------------------------------------------------

/// Parameters for the `run_command` tool.
#[derive(Debug, Deserialize)]
pub struct RunCommandParams {
    /// The shell command to execute.
    pub command: String,
    /// Human-readable explanation of what the command does.
    #[serde(default)]
    pub explanation: String,
}

/// Parameters for the `read_file` tool.
#[derive(Debug, Deserialize)]
pub struct ReadFileParams {
    /// Path to the file to read.
    pub path: String,
}

/// Parameters for the `list_dir` tool.
#[derive(Debug, Deserialize)]
pub struct ListDirParams {
    /// Path to the directory to list.
    pub path: String,
}

// ---------------------------------------------------------------------------
// Tool definitions (for the LLM)
// ---------------------------------------------------------------------------

/// Return the list of tool definitions available to the LLM.
pub fn tool_definitions() -> Vec<ToolDef> {
    vec![
        ToolDef {
            name: TOOL_RUN_COMMAND.into(),
            description: "Run a shell command on the target machine and return the output. \
                Use this for system administration tasks like checking processes, \
                inspecting logs, managing services, etc."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to execute."
                    },
                    "explanation": {
                        "type": "string",
                        "description": "A brief explanation of what this command does and why."
                    }
                },
                "required": ["command"]
            }),
        },
        ToolDef {
            name: TOOL_READ_FILE.into(),
            description: "Read the contents of a file on the target machine. \
                Uses `cat` under the hood."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the file."
                    }
                },
                "required": ["path"]
            }),
        },
        ToolDef {
            name: TOOL_LIST_DIR.into(),
            description: "List the contents of a directory on the target machine. \
                Uses `ls -la` under the hood."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute or relative path to the directory."
                    }
                },
                "required": ["path"]
            }),
        },
    ]
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

/// The name of a tool and whether it requires confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    /// Executes an arbitrary shell command — requires confirmation.
    RunCommand,
    /// Reads a file via `cat` — still executes a command, confirmation depends on policy.
    ReadFile,
    /// Lists a directory via `ls` — still executes a command, confirmation depends on policy.
    ListDir,
}

/// Parsed tool call — the tool name, the shell command to execute, and an
/// optional human-readable explanation.
#[derive(Debug, Clone)]
pub struct ParsedToolCall {
    /// The original tool call ID from the LLM.
    pub id: String,
    /// Which tool was called.
    pub kind: ToolKind,
    /// The shell command that will be executed.
    pub command: String,
    /// Human-readable explanation (from the LLM or derived).
    pub explanation: String,
}

/// Parse a tool call from the LLM into a `ParsedToolCall`.
///
/// Returns an error if the tool name is unknown or the arguments are invalid.
pub fn parse_tool_call(id: &str, name: &str, arguments: &serde_json::Value) -> Result<ParsedToolCall> {
    match name {
        TOOL_RUN_COMMAND => {
            let params: RunCommandParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid run_command arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::RunCommand,
                command: params.command,
                explanation: params.explanation,
            })
        }
        TOOL_READ_FILE => {
            let params: ReadFileParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid read_file arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::ReadFile,
                command: format!("cat {}", shell_quote(&params.path)),
                explanation: format!("Read file: {}", params.path),
            })
        }
        TOOL_LIST_DIR => {
            let params: ListDirParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid list_dir arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::ListDir,
                command: format!("ls -la {}", shell_quote(&params.path)),
                explanation: format!("List directory: {}", params.path),
            })
        }
        other => Err(CoreError::Other(format!("unknown tool: {other}"))),
    }
}

/// Execute a parsed tool call via the given executor and return the output string.
pub async fn execute_tool_call(
    call: &ParsedToolCall,
    executor: &dyn CommandExecutor,
) -> Result<String> {
    info!(tool = ?call.kind, command = %call.command, "executing tool call");
    debug!(explanation = %call.explanation, "tool explanation");

    let result = executor.run(&call.command).await?;

    let mut output = String::new();
    if !result.stdout.is_empty() {
        output.push_str(&result.stdout);
    }
    if !result.stderr.is_empty() {
        if !output.is_empty() {
            output.push('\n');
        }
        output.push_str("[stderr] ");
        output.push_str(&result.stderr);
    }

    // Append exit code if non-zero.
    if let Some(code) = result.exit_code {
        if code != 0 {
            if !output.is_empty() {
                output.push('\n');
            }
            output.push_str(&format!("[exit code: {code}]"));
        }
    }

    if output.is_empty() {
        output.push_str("(no output)");
    }

    Ok(output)
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Simple shell quoting: wraps the value in single quotes if it contains
/// any character that isn't alphanumeric, dash, underscore, dot, or slash.
fn shell_quote(value: &str) -> String {
    if value.chars().all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/') {
        value.to_string()
    } else {
        // Replace any single quotes with '\'' to break out and re-enter quoting.
        let escaped = value.replace('\'', "'\\''");
        format!("'{escaped}'")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_definitions_count() {
        let defs = tool_definitions();
        assert_eq!(defs.len(), 3);
        assert!(defs.iter().any(|d| d.name == TOOL_RUN_COMMAND));
        assert!(defs.iter().any(|d| d.name == TOOL_READ_FILE));
        assert!(defs.iter().any(|d| d.name == TOOL_LIST_DIR));
    }

    #[test]
    fn parse_run_command() {
        let args = serde_json::json!({
            "command": "ls -la /tmp",
            "explanation": "List files in /tmp"
        });
        let call = parse_tool_call("call_1", TOOL_RUN_COMMAND, &args).unwrap();
        assert_eq!(call.kind, ToolKind::RunCommand);
        assert_eq!(call.command, "ls -la /tmp");
        assert_eq!(call.explanation, "List files in /tmp");
    }

    #[test]
    fn parse_read_file() {
        let args = serde_json::json!({"path": "/etc/hostname"});
        let call = parse_tool_call("call_2", TOOL_READ_FILE, &args).unwrap();
        assert_eq!(call.kind, ToolKind::ReadFile);
        assert_eq!(call.command, "cat /etc/hostname");
    }

    #[test]
    fn parse_list_dir() {
        let args = serde_json::json!({"path": "/var/log"});
        let call = parse_tool_call("call_3", TOOL_LIST_DIR, &args).unwrap();
        assert_eq!(call.kind, ToolKind::ListDir);
        assert_eq!(call.command, "ls -la /var/log");
    }

    #[test]
    fn parse_unknown_tool() {
        let args = serde_json::json!({});
        let result = parse_tool_call("call_x", "unknown_tool", &args);
        assert!(result.is_err());
    }

    #[test]
    fn parse_invalid_args() {
        let args = serde_json::json!({"not_command": "foo"});
        let result = parse_tool_call("call_y", TOOL_RUN_COMMAND, &args);
        assert!(result.is_err());
    }

    #[test]
    fn shell_quote_simple() {
        assert_eq!(shell_quote("/etc/hostname"), "/etc/hostname");
        assert_eq!(shell_quote("file.txt"), "file.txt");
    }

    #[test]
    fn shell_quote_with_spaces() {
        assert_eq!(shell_quote("/path/with spaces"), "'/path/with spaces'");
    }

    #[test]
    fn shell_quote_with_single_quote() {
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }
}
