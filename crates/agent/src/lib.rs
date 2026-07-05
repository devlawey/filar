//! Agent crate: LLM client, agent loop, and tools.
//!
//! This crate houses:
//! - The [`LlmClient`] trait and its `GlmClient` implementation (Stage 4).
//! - The agent loop that orchestrates LLM ↔ tool execution (Stage 5).
//! - Tool definitions (`run_command`, `read_file`, `list_dir`) (Stage 5).
//! - Security layer: confirmation, destructive command detection (Stage 5).

pub mod agent;
pub mod glm;
pub mod security;
pub mod tools;

// Re-export key types for convenience.
pub use agent::{Agent, AgentBuilder};
pub use security::{CliConfirmer, CommandConfirmer, ConfirmDecision};
pub use tools::{tool_definitions, ToolKind};

use filar_core::Result;

// ---------------------------------------------------------------------------
// LlmClient trait
// ---------------------------------------------------------------------------

/// Trait abstracting an LLM backend.
///
/// The primary implementation is [`glm::GlmClient`] targeting the GLM API.
/// The trait allows swapping models without touching the agent loop.
#[async_trait::async_trait]
pub trait LlmClient: Send + Sync {
    /// Send a chat request and return the model's response.
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse>;

    /// Send a chat request with streaming, calling `on_delta` for each text
    /// chunk as it arrives. Returns the fully-assembled response.
    ///
    /// Default implementation falls back to non-streaming [`chat`][Self::chat].
    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ChatResponse> {
        // Default: no streaming, just call chat.
        let _ = on_delta; // suppress unused warning in default impl.
        self.chat(request).await
    }
}

// ---------------------------------------------------------------------------
// Request / Response types
// ---------------------------------------------------------------------------

/// A chat request containing the conversation history and available tools.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    /// Conversation messages (system / user / assistant / tool).
    pub messages: Vec<ChatMessage>,
    /// Tool definitions available to the model.
    pub tools: Vec<ToolDef>,
}

/// A single message in the conversation.
#[derive(Debug, Clone)]
pub struct ChatMessage {
    /// Role of the message sender.
    pub role: MessageRole,
    /// Text content of the message.
    pub content: String,
    /// Tool calls made by the assistant (only present on assistant messages).
    pub tool_calls: Vec<ToolCall>,
    /// ID of the tool call this message responds to (only present on tool messages).
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Create a simple text message (system or user).
    pub fn new(role: MessageRole, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Create a system message.
    pub fn system(content: impl Into<String>) -> Self {
        Self::new(MessageRole::System, content)
    }

    /// Create a user message.
    pub fn user(content: impl Into<String>) -> Self {
        Self::new(MessageRole::User, content)
    }

    /// Create an assistant text message.
    pub fn assistant(content: impl Into<String>) -> Self {
        Self::new(MessageRole::Assistant, content)
    }

    /// Create an assistant message that requests one or more tool calls.
    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: MessageRole::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
        }
    }

    /// Create a tool-result message.
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: MessageRole::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Role of a message participant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageRole {
    System,
    User,
    Assistant,
    Tool,
}

impl MessageRole {
    /// Convert to the string used by the OpenAI-compatible API.
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageRole::System => "system",
            MessageRole::User => "user",
            MessageRole::Assistant => "assistant",
            MessageRole::Tool => "tool",
        }
    }
}

/// A tool call requested by the model.
#[derive(Debug, Clone)]
pub struct ToolCall {
    /// Unique ID assigned by the model to this tool call.
    pub id: String,
    /// Name of the tool to call.
    pub name: String,
    /// Arguments as a JSON value.
    pub arguments: serde_json::Value,
}

/// Definition of a tool the model can call.
#[derive(Debug, Clone)]
pub struct ToolDef {
    /// Function / tool name.
    pub name: String,
    /// Human-readable description of what the tool does.
    pub description: String,
    /// JSON Schema describing the parameters.
    pub parameters: serde_json::Value,
}

/// The model's response — either text or one or more tool calls.
#[derive(Debug, Clone)]
pub enum ChatResponse {
    /// The model produced a text response.
    Text(String),
    /// The model wants to call one or more tools.
    ToolCalls(Vec<ToolCall>),
}

// Re-export async_trait.
pub use async_trait::async_trait;
