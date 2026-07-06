//! GLM LLM client — OpenAI-compatible `chat/completions` API.
//!
//! Implements [`crate::LlmClient`] using `reqwest` to talk to the GLM
//! platform (e.g. `open.bigmodel.cn`). The API is OpenAI-compatible, so the
//! request/response shapes mirror the standard `chat/completions` format with
//! tool/function calling support.
//!
//! # Features
//! - Configurable model, base URL, and max tokens (from [`filar_core::LlmConfig`]).
//! - API key read from the `GLM_API_KEY` environment variable.
//! - Retries with exponential backoff on transient failures (5xx, 429, network).
//! - Request timeout.
//! - Tool calling (function calling) support.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use filar_core::{secrets, CoreError, LlmConfig, Result};

use crate::{ChatMessage, ChatRequest, ChatResponse, LlmClient, ToolCall};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Default number of retry attempts for transient failures.
const DEFAULT_MAX_RETRIES: u32 = 3;

/// Base delay for exponential backoff (doubled each retry).
const DEFAULT_BACKOFF_BASE: Duration = Duration::from_millis(500);

// ---------------------------------------------------------------------------
// GlmClient
// ---------------------------------------------------------------------------

/// [`LlmClient`] implementation backed by the GLM (OpenAI-compatible) API.
pub struct GlmClient {
    http: reqwest::Client,
    api_base_url: String,
    model: String,
    max_tokens: u32,
    api_key: String,
    timeout: Duration,
    max_retries: u32,
    backoff_base: Duration,
}

impl GlmClient {
    /// Create a new `GlmClient` from the given LLM config.
    ///
    /// The API key is read from the `GLM_API_KEY` environment variable.
    pub fn new(config: &LlmConfig, timeout: Duration) -> Result<Self> {
        Self::new_with_key(config, timeout, &secrets::glm_api_key()?)
    }

    /// Create a new `GlmClient` with an explicit API key (useful for testing).
    pub fn new_with_key(config: &LlmConfig, timeout: Duration, api_key: &str) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| CoreError::Other(format!("failed to build HTTP client: {e}")))?;

        Ok(Self {
            http,
            api_base_url: config.api_base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            api_key: api_key.to_string(),
            timeout,
            max_retries: DEFAULT_MAX_RETRIES,
            backoff_base: DEFAULT_BACKOFF_BASE,
        })
    }

    /// Override the maximum number of retry attempts.
    #[allow(dead_code)]
    pub fn with_max_retries(mut self, retries: u32) -> Self {
        self.max_retries = retries;
        self
    }

    /// Override the base backoff delay.
    #[allow(dead_code)]
    pub fn with_backoff_base(mut self, base: Duration) -> Self {
        self.backoff_base = base;
        self
    }

    /// Build the full API endpoint URL.
    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.api_base_url)
    }
}

#[async_trait::async_trait]
impl LlmClient for GlmClient {
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let api_request = ApiRequest::from_chat_request(request, &self.model, self.max_tokens);
        let body = serde_json::to_value(&api_request)
            .map_err(|e| CoreError::Other(format!("failed to serialize request: {e}")))?;

        debug!(model = %self.model, "sending chat request to GLM API");

        // Retry loop with exponential backoff.
        let mut last_error: Option<ApiError> = None;
        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let delay = self.backoff_base * 2u32.pow(attempt - 1);
                warn!(attempt, delay_ms = delay.as_millis(), "retrying after transient error");
                tokio::time::sleep(delay).await;
            }

            match self.send_request(&body).await {
                Ok(response) => {
                    debug!("GLM API request succeeded");
                    return response.try_into_chat_response();
                }
                Err(e) if e.is_retryable() => {
                    warn!(attempt, error = %e, "transient error, will retry");
                    last_error = Some(e);
                    continue;
                }
                Err(e) => {
                    return Err(e.into_core_error());
                }
            }
        }

        Err(last_error
            .map(|e| e.into_core_error())
            .unwrap_or_else(|| CoreError::Other("exhausted retries".into())))
    }

    async fn chat_stream(
        &self,
        request: &ChatRequest,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> Result<ChatResponse> {
        let mut api_request =
            ApiRequest::from_chat_request(request, &self.model, self.max_tokens);
        api_request.stream = Some(true);
        let body = serde_json::to_value(&api_request)
            .map_err(|e| CoreError::Other(format!("failed to serialize request: {e}")))?;

        debug!(model = %self.model, "sending streaming chat request to GLM API");

        // Retry loop for the initial connection only (not mid-stream).
        let response = {
            let mut last_error: Option<ApiError> = None;
            let mut resp: Option<reqwest::Response> = None;
            for attempt in 0..=self.max_retries {
                if attempt > 0 {
                    let delay = self.backoff_base * 2u32.pow(attempt - 1);
                    warn!(attempt, delay_ms = delay.as_millis(), "retrying after transient error");
                    tokio::time::sleep(delay).await;
                }
                match self.send_stream_request(&body).await {
                    Ok(r) => {
                        resp = Some(r);
                        break;
                    }
                    Err(e) if e.is_retryable() => {
                        warn!(attempt, error = %e, "transient error, will retry");
                        last_error = Some(e);
                        continue;
                    }
                    Err(e) => {
                        return Err(e.into_core_error());
                    }
                }
            }
            match resp {
                Some(r) => r,
                None => {
                    return Err(last_error
                        .map(|e| e.into_core_error())
                        .unwrap_or_else(|| CoreError::Other("exhausted retries".into())))
                }
            }
        };

        // Parse the SSE stream.
        // Buffer raw bytes and decode only complete lines to avoid
        // corrupting multi-byte UTF-8 characters split across chunks.
        use futures::StreamExt;
        let mut stream = response.bytes_stream();
        let mut state = SseState::new();
        let mut raw_buffer: Vec<u8> = Vec::new();

        loop {
            match stream.next().await {
                Some(Ok(chunk)) => {
                    raw_buffer.extend_from_slice(&chunk);
                    while let Some(pos) = raw_buffer.iter().position(|&b| b == b'\n') {
                        let line: String =
                            String::from_utf8_lossy(&raw_buffer[..=pos]).into_owned();
                        raw_buffer = raw_buffer[pos + 1..].to_vec();
                        let deltas = state.process_chunk(&line);
                        for d in deltas {
                            on_delta(d);
                        }
                    }
                }
                Some(Err(e)) => {
                    return Err(CoreError::Other(format!("stream error: {e}")));
                }
                None => {
                    debug!("stream ended");
                    return state.into_response();
                }
            }
            if state.done {
                return state.into_response();
            }
        }
    }
}

impl GlmClient {
    /// Send a single request to the API and return the raw response.
    async fn send_request(&self, body: &serde_json::Value) -> std::result::Result<ApiResponse, ApiError> {
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ApiError::Timeout(self.timeout)
                } else if e.is_connect() {
                    ApiError::Connect(e.to_string())
                } else {
                    ApiError::Network(e.to_string())
                }
            })?;

        let status = response.status();

        if status.is_success() {
            // Capture the body as text first, then parse as JSON.
            // This way if parsing fails, we can include the actual response
            // body in the error message for debugging.
            let body_text = response.text().await.unwrap_or_default();
            debug!(status = %status, body_len = body_text.len(), "GLM API success response");
            let api_response: ApiResponse = serde_json::from_str(&body_text)
                .map_err(|e| {
                    let preview = if body_text.len() > 500 {
                        format!("{}...", &body_text[..500])
                    } else {
                        body_text.clone()
                    };
                    warn!(error = %e, body = %preview, "failed to parse API response");
                    ApiError::Parse(format!("{e}. Response body: {preview}"))
                })?;
            Ok(api_response)
        } else {
            let status_code = status.as_u16();
            let body_text = response.text().await.unwrap_or_default();
            info!(status_code, body = %body_text, "GLM API returned error status");
            match status_code {
                401 | 403 => Err(ApiError::Auth(format!("HTTP {status_code}: {body_text}"))),
                429 => Err(ApiError::RateLimit(body_text)),
                500..=599 => Err(ApiError::Server(status_code, body_text)),
                _ => Err(ApiError::Client(status_code, body_text)),
            }
        }
    }

    /// Send a streaming request — returns the raw HTTP response for SSE parsing.
    async fn send_stream_request(
        &self,
        body: &serde_json::Value,
    ) -> std::result::Result<reqwest::Response, ApiError> {
        let response = self
            .http
            .post(self.endpoint())
            .bearer_auth(&self.api_key)
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ApiError::Timeout(self.timeout)
                } else if e.is_connect() {
                    ApiError::Connect(e.to_string())
                } else {
                    ApiError::Network(e.to_string())
                }
            })?;

        let status = response.status();
        if status.is_success() {
            debug!(status = %status, "GLM streaming API connection established");
            Ok(response)
        } else {
            let status_code = status.as_u16();
            let body_text = response.text().await.unwrap_or_default();
            info!(status_code, body = %body_text, "GLM API returned error status");
            match status_code {
                401 | 403 => Err(ApiError::Auth(format!("HTTP {status_code}: {body_text}"))),
                429 => Err(ApiError::RateLimit(body_text)),
                500..=599 => Err(ApiError::Server(status_code, body_text)),
                _ => Err(ApiError::Client(status_code, body_text)),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Internal error for retry logic
// ---------------------------------------------------------------------------

/// Internal error type that tracks retryability.
enum ApiError {
    /// Network / connection failure.
    Connect(String),
    /// General network error.
    Network(String),
    /// Request timed out.
    Timeout(Duration),
    /// Authentication error (401/403) — not retryable.
    Auth(String),
    /// Rate limited (429) — retryable.
    RateLimit(String),
    /// Server error (5xx) — retryable.
    Server(u16, String),
    /// Other client error (4xx) — not retryable.
    Client(u16, String),
    /// Failed to parse the response body.
    Parse(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Connect(msg) => write!(f, "connection error: {msg}"),
            ApiError::Network(msg) => write!(f, "network error: {msg}"),
            ApiError::Timeout(d) => write!(f, "request timed out after {d:?}"),
            ApiError::Auth(msg) => write!(f, "authentication error: {msg}"),
            ApiError::RateLimit(msg) => write!(f, "rate limited: {msg}"),
            ApiError::Server(code, msg) => write!(f, "server error {code}: {msg}"),
            ApiError::Client(code, msg) => write!(f, "client error {code}: {msg}"),
            ApiError::Parse(msg) => write!(f, "failed to parse API response: {msg}"),
        }
    }
}

impl ApiError {
    /// Whether this error is worth retrying.
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            ApiError::Connect(_) | ApiError::Network(_) | ApiError::Timeout(_) | ApiError::RateLimit(_) | ApiError::Server(_, _)
        )
    }

    /// Convert to a [`CoreError`] for the final result.
    fn into_core_error(self) -> CoreError {
        match self {
            ApiError::Connect(msg) => CoreError::Other(format!("connection error: {msg}")),
            ApiError::Network(msg) => CoreError::Other(format!("network error: {msg}")),
            ApiError::Timeout(d) => CoreError::Other(format!("request timed out after {d:?}")),
            ApiError::Auth(msg) => CoreError::Other(format!("authentication error: {msg}")),
            ApiError::RateLimit(msg) => CoreError::Other(format!("rate limited: {msg}")),
            ApiError::Server(code, msg) => CoreError::Other(format!("server error {code}: {msg}")),
            ApiError::Client(code, msg) => CoreError::Other(format!("client error {code}: {msg}")),
            ApiError::Parse(msg) => CoreError::Other(format!("failed to parse API response: {msg}")),
        }
    }
}

// ---------------------------------------------------------------------------
// API request / response structs (OpenAI-compatible)
// ---------------------------------------------------------------------------

/// Top-level API request body.
#[derive(Serialize)]
struct ApiRequest {
    model: String,
    messages: Vec<ApiMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<ApiTool>,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    stream: Option<bool>,
}

impl ApiRequest {
    fn from_chat_request(req: &ChatRequest, model: &str, max_tokens: u32) -> Self {
        let messages = req.messages.iter().map(ApiMessage::from).collect();
        let tools = req.tools.iter().map(ApiTool::from).collect();
        Self {
            model: model.to_string(),
            messages,
            tools,
            max_tokens,
            stream: None,
        }
    }
}

/// A message in the API request.
#[derive(Serialize)]
struct ApiMessage {
    role: &'static str,
    content: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ApiToolCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

impl From<&ChatMessage> for ApiMessage {
    fn from(msg: &ChatMessage) -> Self {
        let tool_calls: Vec<ApiToolCall> = msg
            .tool_calls
            .iter()
            .map(ApiToolCall::from)
            .collect();
        Self {
            role: msg.role.as_str(),
            content: msg.content.clone(),
            tool_calls,
            tool_call_id: msg.tool_call_id.clone(),
        }
    }
}

/// A tool definition in the API request.
#[derive(Serialize)]
struct ApiTool {
    #[serde(rename = "type")]
    tool_type: &'static str,
    function: ApiToolFunction,
}

/// Function metadata inside a tool definition.
#[derive(Serialize)]
struct ApiToolFunction {
    name: String,
    description: String,
    parameters: serde_json::Value,
}

impl From<&crate::ToolDef> for ApiTool {
    fn from(def: &crate::ToolDef) -> Self {
        Self {
            tool_type: "function",
            function: ApiToolFunction {
                name: def.name.clone(),
                description: def.description.clone(),
                parameters: def.parameters.clone(),
            },
        }
    }
}

/// A tool call in an assistant message (request side).
#[derive(Serialize)]
struct ApiToolCall {
    id: String,
    #[serde(rename = "type")]
    call_type: &'static str,
    function: ApiToolCallFunction,
}

impl From<&ToolCall> for ApiToolCall {
    fn from(tc: &ToolCall) -> Self {
        Self {
            id: tc.id.clone(),
            call_type: "function",
            function: ApiToolCallFunction {
                name: tc.name.clone(),
                arguments: tc.arguments.to_string(),
            },
        }
    }
}

#[derive(Serialize)]
struct ApiToolCallFunction {
    name: String,
    arguments: String,
}

// ── Response structs ─────────────────────────────────────────────────────

/// Top-level API response body.
#[derive(Deserialize)]
struct ApiResponse {
    choices: Vec<ApiChoice>,
}

#[derive(Deserialize)]
struct ApiChoice {
    message: ApiChoiceMessage,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct ApiChoiceMessage {
    #[allow(dead_code)]
    role: String,
    content: Option<String>,
    tool_calls: Option<Vec<ApiToolCallResponse>>,
}

#[derive(Deserialize)]
struct ApiToolCallResponse {
    id: String,
    function: ApiToolCallResponseFunction,
}

#[derive(Deserialize)]
struct ApiToolCallResponseFunction {
    name: String,
    arguments: String,
}

impl ApiResponse {
    /// Convert the parsed API response into a [`ChatResponse`].
    fn try_into_chat_response(self) -> Result<ChatResponse> {
        let choice = self
            .choices
            .into_iter()
            .next()
            .ok_or_else(|| CoreError::Other("API returned no choices".into()))?;

        // If the model returned tool calls, parse them.
        if let Some(tool_calls) = choice.message.tool_calls {
            if !tool_calls.is_empty() {
                let parsed: Vec<ToolCall> = tool_calls
                    .into_iter()
                    .map(|tc| {
                        let arguments = serde_json::from_str(&tc.function.arguments)
                            .unwrap_or(serde_json::Value::Null);
                        ToolCall {
                            id: tc.id,
                            name: tc.function.name,
                            arguments,
                        }
                    })
                    .collect();
                return Ok(ChatResponse::ToolCalls(parsed));
            }
        }

        // Otherwise, return the text content.
        let text = choice.message.content.unwrap_or_default();
        Ok(ChatResponse::Text(text))
    }
}

// ---------------------------------------------------------------------------
// SSE streaming types
// ---------------------------------------------------------------------------

/// A single SSE event parsed from a `data: {...}` line.
#[derive(Deserialize)]
struct SseEvent {
    #[serde(default)]
    choices: Vec<SseChoice>,
}

#[derive(Deserialize)]
struct SseChoice {
    delta: SseDelta,
    #[allow(dead_code)]
    finish_reason: Option<String>,
}

#[derive(Deserialize)]
struct SseDelta {
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<SseToolCallDelta>>,
}

#[derive(Deserialize)]
struct SseToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<SseToolCallFunctionDelta>,
}

#[derive(Deserialize)]
struct SseToolCallFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

/// Accumulated tool call from streaming deltas.
#[derive(Default)]
struct StreamToolCall {
    id: String,
    name: String,
    arguments: String,
}

/// Stateful SSE parser — accumulates text and tool_calls across chunked data.
///
/// Designed for unit-testing: feed it chunked SSE data via [`process_chunk`](Self::process_chunk),
/// then call [`into_response`](Self::into_response) to get the final [`ChatResponse`].
struct SseState {
    buffer: String,
    full_text: String,
    tool_calls: BTreeMap<usize, StreamToolCall>,
    done: bool,
}

impl SseState {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            full_text: String::new(),
            tool_calls: BTreeMap::new(),
            done: false,
        }
    }

    /// Process a chunk of SSE data. Returns new text content deltas.
    fn process_chunk(&mut self, chunk: &str) -> Vec<String> {
        let mut new_deltas = Vec::new();
        self.buffer.push_str(chunk);
        while let Some(pos) = self.buffer.find('\n') {
            let line = self.buffer[..pos].trim_end_matches('\r').to_string();
            self.buffer = self.buffer[pos + 1..].to_string();

            if line.is_empty() {
                continue;
            }
            let Some(data) = line.strip_prefix("data: ") else {
                continue;
            };

            if data.trim() == "[DONE]" {
                self.done = true;
                continue;
            }

            match serde_json::from_str::<SseEvent>(data) {
                Ok(event) => {
                    if let Some(choice) = event.choices.into_iter().next() {
                        if let Some(content) = choice.delta.content {
                            if !content.is_empty() {
                                new_deltas.push(content.clone());
                                self.full_text.push_str(&content);
                            }
                        }
                        if let Some(tc_deltas) = choice.delta.tool_calls {
                            for tc in tc_deltas {
                                let entry =
                                    self.tool_calls.entry(tc.index).or_default();
                                if let Some(id) = tc.id {
                                    entry.id = id;
                                }
                                if let Some(func) = tc.function {
                                    if let Some(name) = func.name {
                                        entry.name = name;
                                    }
                                    if let Some(args) = func.arguments {
                                        entry.arguments.push_str(&args);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    warn!(error = %e, line = %data, "failed to parse SSE event, skipping");
                }
            }
        }
        new_deltas
    }

    /// Build the final [`ChatResponse`] from accumulated state.
    fn into_response(self) -> Result<ChatResponse> {
        if !self.tool_calls.is_empty() {
            let calls: Vec<ToolCall> = self
                .tool_calls
                .values()
                .map(|tc| {
                    let arguments = serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                        warn!(error = %e, id = %tc.id, name = %tc.name, "failed to parse accumulated tool call arguments");
                        serde_json::Value::Null
                    });
                    ToolCall {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        arguments,
                    }
                })
                .collect();
            Ok(ChatResponse::ToolCalls(calls))
        } else {
            Ok(ChatResponse::Text(self.full_text))
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ChatMessage, MessageRole, ToolDef};

    #[test]
    fn serialize_simple_request() {
        let req = ChatRequest {
            messages: vec![
                ChatMessage::system("You are helpful."),
                ChatMessage::user("Hello"),
            ],
            tools: vec![],
        };
        let api = ApiRequest::from_chat_request(&req, "glm-5.1", 4096);
        let json = serde_json::to_value(&api).unwrap();

        assert_eq!(json["model"], "glm-5.1");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][0]["content"], "You are helpful.");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "Hello");
        // No tools → "tools" key should be absent.
        assert!(json.get("tools").is_none());
    }

    #[test]
    fn serialize_request_with_tools() {
        let req = ChatRequest {
            messages: vec![ChatMessage::user("list files")],
            tools: vec![ToolDef {
                name: "run_command".into(),
                description: "Run a shell command".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": { "type": "string" }
                    },
                    "required": ["command"]
                }),
            }],
        };
        let api = ApiRequest::from_chat_request(&req, "glm-5.1", 4096);
        let json = serde_json::to_value(&api).unwrap();

        assert_eq!(json["tools"][0]["type"], "function");
        assert_eq!(json["tools"][0]["function"]["name"], "run_command");
        assert_eq!(
            json["tools"][0]["function"]["description"],
            "Run a shell command"
        );
    }

    #[test]
    fn deserialize_text_response() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "Hello! How can I help?"
                },
                "finish_reason": "stop"
            }]
        });
        let resp: ApiResponse = serde_json::from_value(raw).unwrap();
        let result = resp.try_into_chat_response().unwrap();
        match result {
            ChatResponse::Text(text) => assert_eq!(text, "Hello! How can I help?"),
            _ => panic!("expected Text response"),
        }
    }

    #[test]
    fn deserialize_tool_call_response() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": null,
                    "tool_calls": [{
                        "id": "call_abc123",
                        "type": "function",
                        "function": {
                            "name": "run_command",
                            "arguments": "{\"command\": \"ls -la\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp: ApiResponse = serde_json::from_value(raw).unwrap();
        let result = resp.try_into_chat_response().unwrap();
        match result {
            ChatResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_abc123");
                assert_eq!(calls[0].name, "run_command");
                assert_eq!(calls[0].arguments["command"], "ls -la");
            }
            _ => panic!("expected ToolCalls response"),
        }
    }

    #[test]
    fn deserialize_multiple_tool_calls() {
        let raw = serde_json::json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "",
                    "tool_calls": [
                        {
                            "id": "call_1",
                            "type": "function",
                            "function": { "name": "list_dir", "arguments": "{\"path\": \"/\"}" }
                        },
                        {
                            "id": "call_2",
                            "type": "function",
                            "function": { "name": "run_command", "arguments": "{\"command\": \"whoami\"}" }
                        }
                    ]
                },
                "finish_reason": "tool_calls"
            }]
        });
        let resp: ApiResponse = serde_json::from_value(raw).unwrap();
        let result = resp.try_into_chat_response().unwrap();
        match result {
            ChatResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].name, "list_dir");
                assert_eq!(calls[1].name, "run_command");
            }
            _ => panic!("expected ToolCalls response"),
        }
    }

    #[test]
    fn message_role_as_str() {
        assert_eq!(MessageRole::System.as_str(), "system");
        assert_eq!(MessageRole::User.as_str(), "user");
        assert_eq!(MessageRole::Assistant.as_str(), "assistant");
        assert_eq!(MessageRole::Tool.as_str(), "tool");
    }

    // ── Smoke tests (behind feature flag, require GLM_API_KEY) ────────────

    #[cfg(feature = "smoke")]
    #[tokio::test]
    async fn smoke_text_response() {
        let api_key = std::env::var("GLM_API_KEY").expect("GLM_API_KEY must be set for smoke tests");
        let config = LlmConfig {
            model: "glm-4-flash".into(),
            api_base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            max_tokens: 256,
        };
        let client = GlmClient::new_with_key(&config, Duration::from_secs(60), &api_key).unwrap();

        let request = ChatRequest {
            messages: vec![
                ChatMessage::system("You are a helpful assistant. Reply in one sentence."),
                ChatMessage::user("What is 2 + 2?"),
            ],
            tools: vec![],
        };

        let response = client.chat(&request).await.expect("chat request failed");
        match response {
            ChatResponse::Text(text) => {
                assert!(!text.is_empty(), "response text should not be empty");
                println!("Smoke test text response: {text}");
            }
            _ => panic!("expected Text response, got ToolCalls"),
        }
    }

    #[cfg(feature = "smoke")]
    #[tokio::test]
    async fn smoke_tool_call() {
        let api_key = std::env::var("GLM_API_KEY").expect("GLM_API_KEY must be set for smoke tests");
        let config = LlmConfig {
            model: "glm-4-flash".into(),
            api_base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            max_tokens: 256,
        };
        let client = GlmClient::new_with_key(&config, Duration::from_secs(60), &api_key).unwrap();

        let request = ChatRequest {
            messages: vec![
                ChatMessage::system("You are a system administrator assistant. Use tools when appropriate."),
                ChatMessage::user("List the files in the current directory."),
            ],
            tools: vec![ToolDef {
                name: "run_command".into(),
                description: "Run a shell command on the remote machine and return stdout.".into(),
                parameters: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "command": {
                            "type": "string",
                            "description": "The shell command to execute."
                        }
                    },
                    "required": ["command"]
                }),
            }],
        };

        let response = client.chat(&request).await.expect("chat request failed");
        match response {
            ChatResponse::ToolCalls(calls) => {
                assert!(!calls.is_empty(), "expected at least one tool call");
                println!("Smoke test tool call: {} → {}", calls[0].name, calls[0].arguments);
            }
            ChatResponse::Text(text) => {
                // Some models may answer in text instead of calling a tool.
                println!("Model responded with text instead of tool call: {text}");
            }
        }
    }

    // ── SSE parser tests ───────────────────────────────────────────────

    #[test]
    fn sse_parse_text_stream_chunked() {
        // Simulate SSE data split across chunks at arbitrary byte boundaries.
        let mut state = SseState::new();

        // Chunk 1: first event, split mid-line.
        let d1 = state.process_chunk("data: {\"choices\":[{\"delta\":{\"content\":\"Hel");
        assert!(d1.is_empty(), "no complete line yet");

        // Chunk 2: rest of first event + start of second.
        let d2 = state.process_chunk(
            "lo\"}}]}

data: {\"choices\":[{\"delta\":{\"content\":\" world\"}}]}

",
        );
        assert_eq!(&d2, &["Hello", " world"]);

        // Chunk 3: [DONE] marker.
        let d3 = state.process_chunk("data: [DONE]\n\n");
        assert!(d3.is_empty());
        assert!(state.done);

        let response = state.into_response().unwrap();
        match response {
            ChatResponse::Text(text) => assert_eq!(text, "Hello world"),
            _ => panic!("expected Text response"),
        }
    }

    #[test]
    fn sse_parse_tool_calls_stream() {
        let mut state = SseState::new();

        // First chunk: tool call with id + name + start of arguments.
        let d1 = state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{\\\"comm\"}}]}}]}\n\n",
        );
        assert!(d1.is_empty(), "no text deltas expected");

        // Second chunk: continuation of arguments.
        let d2 = state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"and\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
        );
        assert!(d2.is_empty());

        // Done.
        state.process_chunk("data: [DONE]\n\n");

        let response = state.into_response().unwrap();
        match response {
            ChatResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].id, "call_1");
                assert_eq!(calls[0].name, "run_command");
                assert_eq!(calls[0].arguments["command"], "ls");
            }
            _ => panic!("expected ToolCalls response"),
        }
    }

    #[test]
    fn sse_parse_multiple_tool_calls() {
        let mut state = SseState::new();

        state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{}\"}}]}}]}\n",
        );
        state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"c2\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"{}\"}}]}}]}\n",
        );
        state.process_chunk("data: [DONE]\n\n");

        let response = state.into_response().unwrap();
        match response {
            ChatResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].id, "c1");
                assert_eq!(calls[0].name, "run_command");
                assert_eq!(calls[1].id, "c2");
                assert_eq!(calls[1].name, "list_dir");
            }
            _ => panic!("expected ToolCalls response"),
        }
    }

    #[test]
    fn sse_parse_text_and_tool_calls() {
        // Model sends text first, then tool calls.
        let mut state = SseState::new();

        let d1 = state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Let me check.\"}}]}\n\n",
        );
        assert_eq!(&d1, &["Let me check."]);

        state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"run_command\",\"arguments\":\"{}\"}}]}}]}\n\n",
        );
        state.process_chunk("data: [DONE]\n\n");

        // Final response is ToolCalls (tool_calls take precedence).
        let response = state.into_response().unwrap();
        match response {
            ChatResponse::ToolCalls(calls) => {
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].name, "run_command");
            }
            _ => panic!("expected ToolCalls response"),
        }
    }

    #[test]
    fn sse_parse_empty_stream() {
        let state = SseState::new();
        let response = state.into_response().unwrap();
        match response {
            ChatResponse::Text(text) => assert!(text.is_empty()),
            _ => panic!("expected Text response"),
        }
    }

    #[test]
    fn serialize_stream_request() {
        let req = ChatRequest {
            messages: vec![ChatMessage::user("Hello")],
            tools: vec![],
        };
        let mut api = ApiRequest::from_chat_request(&req, "glm-4", 4096);
        api.stream = Some(true);
        let json = serde_json::to_value(&api).unwrap();
        assert_eq!(json["stream"], true);

        // Without streaming, stream field should be absent.
        let api_no_stream = ApiRequest::from_chat_request(&req, "glm-4", 4096);
        let json_no_stream = serde_json::to_value(&api_no_stream).unwrap();
        assert!(json_no_stream.get("stream").is_none());
    }
}
