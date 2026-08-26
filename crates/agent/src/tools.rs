//! Tool definitions and execution for the agent.
//!
//! The agent exposes tools to the LLM:
//! - `run_command` — execute a shell command on the current target.
//! - `read_file` — read a file's contents (wrapper around `cat`).
//! - `list_dir` — list directory contents (wrapper around `ls`).
//! - `start_background_job` / `background_job_status` / `cancel_background_job` /
//!   `list_background_jobs` — long-running work without blocking tool timeouts.
//!
//! All tools are implemented as wrappers over shell commands to maintain the
//! **zero-install** invariant — no files are created on the remote machine.

use serde::Deserialize;
use tracing::{debug, info};

use filar_core::{CommandConfirmMode, CoreError, Result};
use filar_transport::CommandExecutor;

use crate::ToolDef;

// ---------------------------------------------------------------------------
// Tool names
// ---------------------------------------------------------------------------

pub const TOOL_RUN_COMMAND: &str = "run_command";
pub const TOOL_READ_FILE: &str = "read_file";
pub const TOOL_LIST_DIR: &str = "list_dir";
pub const TOOL_START_BACKGROUND_JOB: &str = "start_background_job";
pub const TOOL_BACKGROUND_JOB_STATUS: &str = "background_job_status";
pub const TOOL_CANCEL_BACKGROUND_JOB: &str = "cancel_background_job";
pub const TOOL_LIST_BACKGROUND_JOBS: &str = "list_background_jobs";

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
    /// Human-readable explanation (required in Explain mode).
    #[serde(default)]
    pub explanation: String,
}

/// Parameters for the `list_dir` tool.
#[derive(Debug, Deserialize)]
pub struct ListDirParams {
    /// Path to the directory to list.
    pub path: String,
    /// Human-readable explanation (required in Explain mode).
    #[serde(default)]
    pub explanation: String,
}

/// Parameters for `start_background_job`.
#[derive(Debug, Deserialize)]
pub struct StartBackgroundJobParams {
    pub command: String,
    #[serde(default)]
    pub explanation: String,
}

/// Parameters for `background_job_status`.
#[derive(Debug, Deserialize)]
pub struct BackgroundJobStatusParams {
    pub job_id: String,
    #[serde(default = "default_tail_lines")]
    pub tail_lines: u32,
    #[serde(default)]
    pub explanation: String,
}

fn default_tail_lines() -> u32 {
    50
}

/// Parameters for `cancel_background_job`.
#[derive(Debug, Deserialize)]
pub struct CancelBackgroundJobParams {
    pub job_id: String,
    #[serde(default)]
    pub explanation: String,
}

/// Parameters for `list_background_jobs`.
#[derive(Debug, Deserialize)]
pub struct ListBackgroundJobsParams {
    #[serde(default)]
    pub explanation: String,
}

// ---------------------------------------------------------------------------
// Tool definitions (for the LLM)
// ---------------------------------------------------------------------------

/// Return the list of tool definitions available to the LLM.
///
/// In `Explain` mode, the `explanation` field is added to `required` for all
/// tools, forcing the model to provide a justification for each command.
pub fn tool_definitions(mode: CommandConfirmMode) -> Vec<ToolDef> {
    let require_explanation = mode == CommandConfirmMode::Explain;
    let required_cmd = if require_explanation {
        serde_json::json!(["command", "explanation"])
    } else {
        serde_json::json!(["command"])
    };
    let required_path = if require_explanation {
        serde_json::json!(["path", "explanation"])
    } else {
        serde_json::json!(["path"])
    };
    let explanation_prop = || serde_json::json!({
        "type": "string",
        "description": "A brief explanation of what this command does and why."
    });

    let required_job_id = if require_explanation {
        serde_json::json!(["job_id", "explanation"])
    } else {
        serde_json::json!(["job_id"])
    };
    let required_start = if require_explanation {
        serde_json::json!(["command", "explanation"])
    } else {
        serde_json::json!(["command"])
    };
    let required_list_jobs = if require_explanation {
        serde_json::json!(["explanation"])
    } else {
        serde_json::json!([])
    };

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
                    "explanation": explanation_prop()
                },
                "required": required_cmd
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
                    },
                    "explanation": explanation_prop()
                },
                "required": required_path
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
                    },
                    "explanation": explanation_prop()
                },
                "required": required_path
            }),
        },
        ToolDef {
            name: TOOL_START_BACKGROUND_JOB.into(),
            description: "Start a long-running command in the background and return a job_id \
                immediately. Use background_job_status to poll progress with short calls; the \
                job keeps running beyond the normal command timeout. Prefer this over run_command \
                for downloads, builds, pulls, and other work that may take many minutes."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "The shell command to run in the background."
                    },
                    "explanation": explanation_prop()
                },
                "required": required_start
            }),
        },
        ToolDef {
            name: TOOL_BACKGROUND_JOB_STATUS.into(),
            description: "Poll a background job started with start_background_job. Returns \
                running/done/failed/cancelled status and recent output. This call is short — \
                it does not wait for the job to finish."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Job id returned by start_background_job."
                    },
                    "tail_lines": {
                        "type": "integer",
                        "description": "How many lines of output to include (default 50)."
                    },
                    "explanation": explanation_prop()
                },
                "required": required_job_id
            }),
        },
        ToolDef {
            name: TOOL_CANCEL_BACKGROUND_JOB.into(),
            description: "Cancel a background job by job_id. Stops the underlying process."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Job id returned by start_background_job."
                    },
                    "explanation": explanation_prop()
                },
                "required": required_job_id
            }),
        },
        ToolDef {
            name: TOOL_LIST_BACKGROUND_JOBS.into(),
            description: "List background jobs for the current session with their status."
                .into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "explanation": explanation_prop()
                },
                "required": required_list_jobs
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
    /// Start a detached background job — same confirm policy as RunCommand.
    StartBackgroundJob,
    /// Poll background job status — readonly / allowlisted.
    BackgroundJobStatus,
    /// Cancel a background job — requires confirmation.
    CancelBackgroundJob,
    /// List session background jobs — readonly / allowlisted.
    ListBackgroundJobs,
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
    /// Background job id (status / cancel tools).
    pub job_id: Option<String>,
    /// Lines of output to tail (status tool).
    pub tail_lines: Option<u32>,
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
                job_id: None,
                tail_lines: None,
            })
        }
        TOOL_READ_FILE => {
            let params: ReadFileParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid read_file arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::ReadFile,
                command: format!("cat {}", shell_quote(&params.path)),
                explanation: if params.explanation.is_empty() {
                    format!("Read file: {}", params.path)
                } else {
                    params.explanation
                },
                job_id: None,
                tail_lines: None,
            })
        }
        TOOL_LIST_DIR => {
            let params: ListDirParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid list_dir arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::ListDir,
                command: format!("ls -la {}", shell_quote(&params.path)),
                explanation: if params.explanation.is_empty() {
                    format!("List directory: {}", params.path)
                } else {
                    params.explanation
                },
                job_id: None,
                tail_lines: None,
            })
        }
        TOOL_START_BACKGROUND_JOB => {
            let params: StartBackgroundJobParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid start_background_job arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::StartBackgroundJob,
                command: params.command,
                explanation: params.explanation,
                job_id: None,
                tail_lines: None,
            })
        }
        TOOL_BACKGROUND_JOB_STATUS => {
            let params: BackgroundJobStatusParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid background_job_status arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::BackgroundJobStatus,
                command: format!("background_job_status {}", params.job_id),
                explanation: if params.explanation.is_empty() {
                    format!("Poll background job: {}", params.job_id)
                } else {
                    params.explanation
                },
                job_id: Some(params.job_id),
                tail_lines: Some(params.tail_lines),
            })
        }
        TOOL_CANCEL_BACKGROUND_JOB => {
            let params: CancelBackgroundJobParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid cancel_background_job arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::CancelBackgroundJob,
                command: crate::background::confirm_command_for_cancel(&params.job_id, None),
                explanation: if params.explanation.is_empty() {
                    format!("Cancel background job: {}", params.job_id)
                } else {
                    params.explanation
                },
                job_id: Some(params.job_id),
                tail_lines: None,
            })
        }
        TOOL_LIST_BACKGROUND_JOBS => {
            let params: ListBackgroundJobsParams = serde_json::from_value(arguments.clone())
                .map_err(|e| CoreError::Other(format!("invalid list_background_jobs arguments: {e}")))?;
            Ok(ParsedToolCall {
                id: id.to_string(),
                kind: ToolKind::ListBackgroundJobs,
                command: "list_background_jobs".into(),
                explanation: if params.explanation.is_empty() {
                    "List background jobs".into()
                } else {
                    params.explanation
                },
                job_id: None,
                tail_lines: None,
            })
        }
        other => Err(CoreError::Other(format!("unknown tool: {other}"))),
    }
}

/// Check if a tool call has a non-empty explanation (required in Explain mode).
///
/// Returns `Some(error_message)` if the explanation is missing or empty,
/// `None` if the explanation is present (or the tool is unknown — let
/// `parse_tool_call` handle that).
pub fn check_explanation(name: &str, arguments: &serde_json::Value) -> Option<String> {
    match name {
        TOOL_RUN_COMMAND => {
            let params: RunCommandParams = serde_json::from_value(arguments.clone()).ok()?;
            if params.explanation.trim().is_empty() {
                return Some(
                    "Error: in safe mode, every command must include an `explanation` \
                     describing what it does, why it is needed now, and what it changes. \
                     Please resubmit with a meaningful explanation."
                        .into(),
                );
            }
        }
        TOOL_READ_FILE => {
            let params: ReadFileParams = serde_json::from_value(arguments.clone()).ok()?;
            if params.explanation.trim().is_empty() {
                return Some(
                    "Error: in safe mode, read_file also requires an `explanation`. \
                     Please describe why you need to read this file."
                        .into(),
                );
            }
        }
        TOOL_LIST_DIR => {
            let params: ListDirParams = serde_json::from_value(arguments.clone()).ok()?;
            if params.explanation.trim().is_empty() {
                return Some(
                    "Error: in safe mode, list_dir also requires an `explanation`. \
                     Please describe why you need to list this directory."
                        .into(),
                );
            }
        }
        TOOL_START_BACKGROUND_JOB => {
            let params: StartBackgroundJobParams = serde_json::from_value(arguments.clone()).ok()?;
            if params.explanation.trim().is_empty() {
                return Some(
                    "Error: in safe mode, start_background_job requires an `explanation`. \
                     Please describe why this long-running job is needed."
                        .into(),
                );
            }
        }
        TOOL_BACKGROUND_JOB_STATUS => {
            let params: BackgroundJobStatusParams = serde_json::from_value(arguments.clone()).ok()?;
            if params.explanation.trim().is_empty() {
                return Some(
                    "Error: in safe mode, background_job_status requires an `explanation`."
                        .into(),
                );
            }
        }
        TOOL_CANCEL_BACKGROUND_JOB => {
            let params: CancelBackgroundJobParams = serde_json::from_value(arguments.clone()).ok()?;
            if params.explanation.trim().is_empty() {
                return Some(
                    "Error: in safe mode, cancel_background_job requires an `explanation`."
                        .into(),
                );
            }
        }
        TOOL_LIST_BACKGROUND_JOBS => {
            let params: ListBackgroundJobsParams = serde_json::from_value(arguments.clone()).ok()?;
            if params.explanation.trim().is_empty() {
                return Some(
                    "Error: in safe mode, list_background_jobs requires an `explanation`."
                        .into(),
                );
            }
        }
        _ => {}
    }
    None
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
        let defs = tool_definitions(CommandConfirmMode::Allowlist);
        assert_eq!(defs.len(), 7);
        assert!(defs.iter().any(|d| d.name == TOOL_RUN_COMMAND));
        assert!(defs.iter().any(|d| d.name == TOOL_READ_FILE));
        assert!(defs.iter().any(|d| d.name == TOOL_LIST_DIR));
        assert!(defs.iter().any(|d| d.name == TOOL_START_BACKGROUND_JOB));
        assert!(defs.iter().any(|d| d.name == TOOL_BACKGROUND_JOB_STATUS));
        assert!(defs.iter().any(|d| d.name == TOOL_CANCEL_BACKGROUND_JOB));
        assert!(defs.iter().any(|d| d.name == TOOL_LIST_BACKGROUND_JOBS));
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

    // ── Explain mode tests ──────────────────────────────────────────

    #[test]
    fn tool_definitions_explain_requires_explanation() {
        let defs = tool_definitions(CommandConfirmMode::Explain);
        for def in &defs {
            let required = def.parameters["required"].as_array().unwrap();
            assert!(
                required.iter().any(|v| v.as_str() == Some("explanation")),
                "tool '{}' must require 'explanation' in Explain mode",
                def.name
            );
        }
    }

    #[test]
    fn tool_definitions_non_explain_no_explanation_required() {
        for mode in [
            CommandConfirmMode::Always,
            CommandConfirmMode::Allowlist,
            CommandConfirmMode::Never,
        ] {
            let defs = tool_definitions(mode);
            for def in &defs {
                let required = def.parameters["required"].as_array().unwrap();
                assert!(
                    !required.iter().any(|v| v.as_str() == Some("explanation")),
                    "tool '{}' must NOT require 'explanation' in {:?} mode",
                    def.name,
                    mode
                );
            }
        }
    }

    #[test]
    fn check_explanation_empty_run_command() {
        let args = serde_json::json!({"command": "ls"});
        assert!(check_explanation(TOOL_RUN_COMMAND, &args).is_some());
    }

    #[test]
    fn check_explanation_whitespace_run_command() {
        let args = serde_json::json!({"command": "ls", "explanation": "   "});
        assert!(check_explanation(TOOL_RUN_COMMAND, &args).is_some());
    }

    #[test]
    fn check_explanation_present_run_command() {
        let args = serde_json::json!({"command": "ls", "explanation": "list files"});
        assert!(check_explanation(TOOL_RUN_COMMAND, &args).is_none());
    }

    #[test]
    fn check_explanation_empty_read_file() {
        let args = serde_json::json!({"path": "/etc/hostname"});
        assert!(check_explanation(TOOL_READ_FILE, &args).is_some());
    }

    #[test]
    fn check_explanation_present_read_file() {
        let args = serde_json::json!({"path": "/etc/hostname", "explanation": "check hostname"});
        assert!(check_explanation(TOOL_READ_FILE, &args).is_none());
    }

    #[test]
    fn check_explanation_empty_list_dir() {
        let args = serde_json::json!({"path": "/var/log"});
        assert!(check_explanation(TOOL_LIST_DIR, &args).is_some());
    }

    #[test]
    fn check_explanation_unknown_tool() {
        let args = serde_json::json!({});
        assert!(check_explanation("unknown_tool", &args).is_none());
    }

    #[test]
    fn parse_read_file_with_explanation() {
        let args = serde_json::json!({"path": "/etc/hostname", "explanation": "check hostname"});
        let call = parse_tool_call("call_1", TOOL_READ_FILE, &args).unwrap();
        assert_eq!(call.explanation, "check hostname");
    }

    #[test]
    fn parse_read_file_without_explanation_falls_back() {
        let args = serde_json::json!({"path": "/etc/hostname"});
        let call = parse_tool_call("call_1", TOOL_READ_FILE, &args).unwrap();
        assert_eq!(call.explanation, "Read file: /etc/hostname");
    }

    #[test]
    fn parse_list_dir_with_explanation() {
        let args = serde_json::json!({"path": "/var/log", "explanation": "list logs"});
        let call = parse_tool_call("call_1", TOOL_LIST_DIR, &args).unwrap();
        assert_eq!(call.explanation, "list logs");
    }
}
