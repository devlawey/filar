//! OpenAI-compatible LLM client — `chat/completions` API.
//!
//! Implements [`crate::LlmClient`] using `reqwest` to talk to any
//! OpenAI-compatible endpoint (default: the GLM platform, e.g.
//! `open.bigmodel.cn`). The request/response shapes mirror the standard
//! `chat/completions` format with tool/function calling support, so any
//! provider implementing that protocol (GLM cloud, Ollama / LM Studio at
//! `http://localhost:11434/v1`, DeepSeek, OpenAI, …) works by changing only
//! the config.
//!
//! # Features
//! - Configurable model, base URL, and max tokens (from [`filar_core::LlmConfig`]).
//! - API key read from the `GLM_API_KEY` environment variable by default
//!   (overridable per profile via `filar_core::LlmProfile::key_env`).
//!   Empty `key_env` = keyless local profile: no `Authorization` header.
//! - Retries with exponential backoff on transient failures (5xx, 429, network).
//! - Request timeout.
//! - Tool calling (function calling) support.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use filar_core::{secrets, CoreError, LlmConfig, Result, SecretProvider};

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
// OpenAiCompatClient
// ---------------------------------------------------------------------------

/// Build an HTTP client with the policy shared by both clients, applying the
/// caller's timeout configuration.
///
/// When `api_key` is empty (keyless / local profile), HTTP redirects are
/// disabled so request bodies cannot be forwarded to another host.
fn build_http_client(
    api_key: &str,
    configure: impl FnOnce(reqwest::ClientBuilder) -> reqwest::ClientBuilder,
) -> Result<reqwest::Client> {
    let mut builder = configure(reqwest::Client::builder());
    if api_key.is_empty() {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    builder
        .build()
        .map_err(|e| CoreError::Other(format!("failed to build HTTP client: {e}")))
}

/// Render an error together with its `source()` chain.
///
/// `reqwest::Error` prints only its own layer — a dropped connection and an
/// elapsed timeout both read as `error decoding response body` — so the real
/// cause is only visible one or two levels down. Used for logs and for the
/// message the user finally sees.
fn describe_error_chain(err: &dyn std::error::Error) -> String {
    const MAX_LEVELS: usize = 8;
    let mut parts = vec![err.to_string()];
    let mut source = err.source();
    while let Some(e) = source {
        if parts.len() >= MAX_LEVELS {
            break;
        }
        let text = e.to_string();
        if !parts.iter().any(|p| p == &text) {
            parts.push(text);
        }
        source = e.source();
    }
    parts.join(": ")
}

/// Outcome of reading one streaming response body.
enum StreamOutcome {
    /// The stream ended on its own; carries the assembled response.
    Complete(Result<ChatResponse>),
    /// The transport failed while the body was being read.
    Failed {
        error: ApiError,
        /// Whether any text delta already reached the caller's callback.
        emitted_any: bool,
    },
}

/// [`LlmClient`] implementation backed by an OpenAI-compatible
/// `chat/completions` API (default endpoint: GLM).
pub struct OpenAiCompatClient {
    http: reqwest::Client,
    http_stream: reqwest::Client,
    api_base_url: String,
    model: String,
    max_tokens: u32,
    api_key: String,
    timeout: Duration,
    max_retries: u32,
    backoff_base: Duration,
    temperature: Option<f32>,
    top_p: Option<f32>,
    extra_body: Option<serde_json::Value>,
}

impl OpenAiCompatClient {
    /// Create a new `OpenAiCompatClient` from the given LLM config.
    ///
    /// The API key is read from the `GLM_API_KEY` environment variable.
    pub fn new(config: &LlmConfig, timeout: Duration) -> Result<Self> {
        Self::new_with_key(config, timeout, &secrets::glm_api_key()?)
    }

    /// Create a new `OpenAiCompatClient` using a [`SecretProvider`] to retrieve the
    /// API key.  The `key_name` is the logical name passed to the provider
    /// (e.g. `"GLM_API_KEY"` or a profile-specific env var name).
    ///
    /// This is the preferred constructor for engine consumers (bots, mobile,
    /// GUI-launched sessions) — it avoids direct `std::env::var` calls.
    pub fn new_with_provider(
        config: &LlmConfig,
        timeout: Duration,
        key_name: &str,
        provider: &dyn SecretProvider,
    ) -> Result<Self> {
        let api_key = provider.get(key_name)?;
        Self::new_with_key(config, timeout, &api_key)
    }

    /// Create a new `OpenAiCompatClient` with an explicit API key (useful for testing).
    ///
    /// When `api_key` is empty (keyless / local profile), HTTP redirects are
    /// disabled so request bodies cannot be forwarded to another host.
    pub fn new_with_key(config: &LlmConfig, timeout: Duration, api_key: &str) -> Result<Self> {
        // Non-streaming calls keep a total request timeout: the whole response
        // arrives at once, so bounding the whole call is the right shape.
        let http = build_http_client(api_key, |b| b.timeout(timeout))?;

        // Streaming deliberately does NOT use a total timeout. In reqwest,
        // `Client::timeout` also covers reading the response body, so for a
        // streaming request it caps the entire generation at
        // `[timeouts].llm_secs` and cuts long answers off mid-stream — which
        // surfaces as a body error, not as a timeout. Bound the connect phase
        // and the silence between chunks instead, so a stream may run as long
        // as the model keeps sending.
        let http_stream = build_http_client(api_key, |b| {
            b.connect_timeout(timeout).read_timeout(timeout)
        })?;

        Ok(Self {
            http,
            http_stream,
            api_base_url: config.api_base_url.trim_end_matches('/').to_string(),
            model: config.model.clone(),
            max_tokens: config.max_tokens,
            api_key: api_key.to_string(),
            timeout,
            max_retries: DEFAULT_MAX_RETRIES,
            backoff_base: DEFAULT_BACKOFF_BASE,
            temperature: config.temperature,
            top_p: config.top_p,
            extra_body: config.extra_body.clone(),
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

    /// Whether outbound requests will include an `Authorization` header.
    fn sends_authorization(&self) -> bool {
        !self.api_key.is_empty()
    }

    /// Attach bearer auth only when a key is present.
    fn apply_auth(
        &self,
        builder: reqwest::RequestBuilder,
    ) -> reqwest::RequestBuilder {
        if self.sends_authorization() {
            builder.bearer_auth(&self.api_key)
        } else {
            builder
        }
    }
}

#[async_trait::async_trait]
impl LlmClient for OpenAiCompatClient {
    async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
        let api_request = ApiRequest::from_chat_request(
            request,
            &self.model,
            self.max_tokens,
            self.temperature,
            self.top_p,
        );
        let mut body = serde_json::to_value(&api_request)
            .map_err(|e| CoreError::Other(format!("failed to serialize request: {e}")))?;
        if let Some(ref extra) = self.extra_body {
            merge_extra_body(&mut body, extra);
        }

        debug!(model = %self.model, "sending chat request to OpenAI-compatible API");

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
                    debug!("OpenAI-compatible API request succeeded");
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
        let mut api_request = ApiRequest::from_chat_request(
            request,
            &self.model,
            self.max_tokens,
            self.temperature,
            self.top_p,
        );
        api_request.stream = Some(true);
        let mut body = serde_json::to_value(&api_request)
            .map_err(|e| CoreError::Other(format!("failed to serialize request: {e}")))?;
        if let Some(ref extra) = self.extra_body {
            merge_extra_body(&mut body, extra);
        }

        debug!(model = %self.model, "sending streaming chat request to OpenAI-compatible API");

        // One retry loop covering both phases: establishing the connection and
        // reading the body. Re-sending is safe here — no tool has executed at
        // this point, tool calls run only after this function returns a
        // complete response, so a repeated request has no side effects.
        let started = std::time::Instant::now();
        let mut last_error: Option<ApiError> = None;
        let mut attempts: u32 = 0;

        for attempt in 0..=self.max_retries {
            if attempt > 0 {
                let delay = self.backoff_base * 2u32.pow(attempt - 1);
                warn!(attempt, delay_ms = delay.as_millis(), "retrying after transient error");
                // Awaited inside this future so a cancellation token wrapping
                // `chat_stream` aborts during the backoff, not after it.
                tokio::time::sleep(delay).await;
            }
            attempts += 1;

            let response = match self.send_stream_request(&body).await {
                Ok(r) => r,
                Err(e) if e.is_retryable() => {
                    warn!(attempt, error = %e, "transient error, will retry");
                    last_error = Some(e);
                    continue;
                }
                Err(e) => return Err(e.into_core_error()),
            };

            match self.read_stream(response, on_delta).await {
                StreamOutcome::Complete(result) => return result,
                StreamOutcome::Failed { error, emitted_any } => {
                    // Retrying after deltas already reached the UI would replay
                    // the answer from the start and show it twice: `on_delta`
                    // can only append, there is no way to retract what was
                    // shown. Fail instead, with the real cause.
                    if emitted_any {
                        warn!(attempt, error = %error, "stream failed after partial output, not retrying");
                        return Err(CoreError::Other(format!(
                            "stream interrupted after partial response: {error}"
                        )));
                    }
                    if !error.is_retryable() {
                        return Err(error.into_core_error());
                    }
                    warn!(attempt, error = %error, "stream failed before any output, will retry");
                    last_error = Some(error);
                    continue;
                }
            }
        }

        Err(match last_error {
            Some(e) => CoreError::Other(format!(
                "LLM stream failed after {attempts} attempts over {:.1}s: {e}",
                started.elapsed().as_secs_f32()
            )),
            None => CoreError::Other("exhausted retries".into()),
        })
    }
}

impl OpenAiCompatClient {
    /// Read one streaming response body to its end, emitting text deltas as
    /// they arrive.
    ///
    /// Buffers raw bytes and decodes only complete lines, so a multi-byte
    /// UTF-8 character split across two chunks is not corrupted.
    async fn read_stream(
        &self,
        response: reqwest::Response,
        on_delta: &(dyn Fn(String) + Send + Sync),
    ) -> StreamOutcome {
        use futures::StreamExt;
        let mut stream = response.bytes_stream();
        let mut state = SseState::new();
        let mut raw_buffer: Vec<u8> = Vec::new();
        let mut emitted_any = false;

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
                            emitted_any = true;
                            on_delta(d);
                        }
                    }
                }
                Some(Err(e)) => {
                    let error = self.classify_stream_error(&e);
                    warn!(
                        cause = %describe_error_chain(&e),
                        emitted_any,
                        "LLM stream failed while reading the response body"
                    );
                    return StreamOutcome::Failed { error, emitted_any };
                }
                None => {
                    debug!("stream ended");
                    // The stream is over: `emitted_any` is not tracked past
                    // this point, it only governs whether a *failure* may be
                    // retried.
                    //
                    // Flush raw_buffer: if there's leftover data without a
                    // trailing newline, process it as a final line.
                    if !raw_buffer.is_empty() {
                        let leftover = String::from_utf8_lossy(&raw_buffer).into_owned();
                        let deltas = state.process_chunk(&format!("{}\n", leftover));
                        for d in deltas {
                            on_delta(d);
                        }
                    }
                    // Flush SseState buffer: process any remaining partial
                    // line that was not terminated by a newline.
                    let deltas = state.flush();
                    for d in deltas {
                        on_delta(d);
                    }
                    return StreamOutcome::Complete(state.into_response());
                }
            }
            if state.done {
                return StreamOutcome::Complete(state.into_response());
            }
        }
    }

    /// Classify a failure that happened while reading the response body.
    ///
    /// Both a dropped connection and an elapsed read timeout are reported by
    /// reqwest as `error decoding response body`; only the source chain tells
    /// them apart. Both are transient and worth retrying.
    fn classify_stream_error(&self, e: &reqwest::Error) -> ApiError {
        if e.is_timeout() {
            ApiError::Timeout(self.timeout)
        } else {
            ApiError::Network(describe_error_chain(e))
        }
    }

    /// Send a single request to the API and return the raw response.
    async fn send_request(&self, body: &serde_json::Value) -> std::result::Result<ApiResponse, ApiError> {
        let response = self
            .apply_auth(self.http.post(self.endpoint()))
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
            debug!(status = %status, body_len = body_text.len(), "OpenAI-compatible API success response");
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
            info!(status_code, body = %body_text, "OpenAI-compatible API returned error status");
            Err(ApiError::from_http_status(status_code, body_text))
        }
    }

    /// Send a streaming request — returns the raw HTTP response for SSE parsing.
    async fn send_stream_request(
        &self,
        body: &serde_json::Value,
    ) -> std::result::Result<reqwest::Response, ApiError> {
        let response = self
            .apply_auth(self.http_stream.post(self.endpoint()))
            .json(body)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ApiError::Timeout(self.timeout)
                } else if e.is_connect() {
                    ApiError::Connect(describe_error_chain(&e))
                } else {
                    ApiError::Network(describe_error_chain(&e))
                }
            })?;

        let status = response.status();
        if status.is_success() {
            debug!(status = %status, "GLM streaming API connection established");
            Ok(response)
        } else {
            let status_code = status.as_u16();
            let body_text = response.text().await.unwrap_or_default();
            info!(status_code, body = %body_text, "OpenAI-compatible API returned error status");
            Err(ApiError::from_http_status(status_code, body_text))
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
    /// Provider rejected tool/function calling — agent loop cannot run.
    ToolsUnsupported,
    /// Failed to parse the response body.
    Parse(String),
}

impl ApiError {
    fn from_http_status(status_code: u16, body_text: String) -> Self {
        // Only classify tool-calling rejection on non-retryable 4xx (not 429).
        // Applying the heuristic to 5xx would turn transient failures into
        // non-retryable ToolsUnsupported.
        let is_client_4xx = (400..500).contains(&status_code) && status_code != 429;
        if is_client_4xx && looks_like_tools_unsupported(&body_text) {
            warn!(status_code, body = %body_text, "provider rejected tool calling");
            return ApiError::ToolsUnsupported;
        }
        match status_code {
            401 | 403 => ApiError::Auth(format!("HTTP {status_code}: {body_text}")),
            429 => ApiError::RateLimit(body_text),
            500..=599 => ApiError::Server(status_code, body_text),
            _ => ApiError::Client(status_code, body_text),
        }
    }
}

/// Heuristic: provider body says the model/server cannot do tool calling.
fn looks_like_tools_unsupported(body: &str) -> bool {
    let lower = body.to_lowercase();
    let mentions_tools = lower.contains("tool")
        || lower.contains("function call")
        || lower.contains("function_call");
    if !mentions_tools {
        return false;
    }
    lower.contains("not support")
        || lower.contains("unsupported")
        || lower.contains("doesn't support")
        || lower.contains("not available")
        || lower.contains("is not enabled")
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::Connect(msg) => write!(f, "connection error: {msg}"),
            ApiError::Network(msg) => write!(f, "network error: {msg}"),
            ApiError::Timeout(d) => write!(
                f,
                "request timed out after {d:?}. For local models, increase [timeouts].llm_secs in config.toml"
            ),
            ApiError::Auth(msg) => write!(f, "authentication error: {msg}"),
            ApiError::RateLimit(msg) => write!(f, "rate limited: {msg}"),
            ApiError::Server(code, msg) => write!(f, "server error {code}: {msg}"),
            ApiError::Client(code, msg) => write!(f, "client error {code}: {msg}"),
            ApiError::ToolsUnsupported => write!(
                f,
                "this model does not support tool calling; the agent cannot run commands. Choose a model with tool/function calling support"
            ),
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
        match &self {
            ApiError::Timeout(_) | ApiError::ToolsUnsupported => {
                CoreError::Other(self.to_string())
            }
            ApiError::Connect(msg) => CoreError::Other(format!("connection error: {msg}")),
            ApiError::Network(msg) => CoreError::Other(format!("network error: {msg}")),
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
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,
}

impl ApiRequest {
    fn from_chat_request(
        req: &ChatRequest,
        model: &str,
        max_tokens: u32,
        temperature: Option<f32>,
        top_p: Option<f32>,
    ) -> Self {
        let messages = req.messages.iter().map(ApiMessage::from).collect();
        let tools = req.tools.iter().map(ApiTool::from).collect();
        Self {
            model: model.to_string(),
            messages,
            tools,
            max_tokens,
            stream: None,
            temperature,
            top_p,
        }
    }
}

/// Keys that `extra_body` is not allowed to override.
const PROTECTED_KEYS: &[&str] = &["model", "messages", "tools", "stream"];

/// Merge `extra_body` into the serialized JSON request body.
///
/// Protected keys (`model`, `messages`, `tools`, `stream`) are silently
/// ignored with a `warn!` log. All other keys from `extra_body` are
/// inserted into (or override) the body.
fn merge_extra_body(body: &mut serde_json::Value, extra: &serde_json::Value) {
    let Some(extra_map) = extra.as_object() else {
        warn!("extra_body is not a JSON object, ignoring");
        return;
    };
    let Some(body_map) = body.as_object_mut() else {
        warn!("request body is not a JSON object, cannot merge extra_body");
        return;
    };
    for (key, value) in extra_map {
        if PROTECTED_KEYS.contains(&key.as_str()) {
            warn!(key = %key, "extra_body key is protected, ignoring");
            continue;
        }
        body_map.insert(key.clone(), value.clone());
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
    #[serde(default)]
    usage: Option<ApiUsage>,
    #[serde(default)]
    model: Option<String>,
}

#[derive(Deserialize, Default)]
struct ApiUsage {
    #[serde(default)]
    prompt_tokens: Option<u64>,
    #[serde(default)]
    completion_tokens: Option<u64>,
    #[serde(default)]
    total_tokens: Option<u64>,
    #[serde(default)]
    cost: Option<f64>,
    #[serde(default)]
    #[allow(dead_code)]
    cost_details: Option<serde_json::Value>,
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

        let usage = self.usage.map(|u| crate::TokenUsage {
            prompt_tokens: u.prompt_tokens,
            completion_tokens: u.completion_tokens,
            total_tokens: u.total_tokens,
            cost: u.cost,
        });

        let model = self.model;

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
                let mut resp = ChatResponse::tool_calls(
                    choice.message.content.unwrap_or_default(),
                    parsed,
                );
                if let Some(u) = usage { resp = resp.with_usage(u); }
                if let Some(m) = model { resp = resp.with_model(m); }
                return Ok(resp);
            }
        }

        // Otherwise, return the text content.
        let text = choice.message.content.unwrap_or_default();
        let mut resp = ChatResponse::text(text);
        if let Some(u) = usage { resp = resp.with_usage(u); }
        if let Some(m) = model { resp = resp.with_model(m); }
        Ok(resp)
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
    #[serde(default)]
    usage: Option<ApiUsage>,
    #[serde(default)]
    model: Option<String>,
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
    streamed_usage: Option<ApiUsage>,
    streamed_model: Option<String>,
}

impl SseState {
    fn new() -> Self {
        Self {
            buffer: String::new(),
            full_text: String::new(),
            tool_calls: BTreeMap::new(),
            done: false,
            streamed_usage: None,
            streamed_model: None,
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
                    if let Some(u) = event.usage {
                        self.streamed_usage = Some(u);
                    }
                    if let Some(m) = event.model {
                        self.streamed_model = Some(m);
                    }
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

    /// Flush any remaining buffered data as a final SSE line.
    ///
    /// Called when the stream ends without a trailing newline.  If the buffer
    /// contains a partial `data:` line, it is processed as if a newline were
    /// appended.  Returns any text deltas produced.
    fn flush(&mut self) -> Vec<String> {
        if self.buffer.is_empty() {
            return Vec::new();
        }
        // Append a newline so `process_chunk` can handle the trailing line.
        self.process_chunk("\n")
    }

    /// Build the final [`ChatResponse`] from accumulated state.
    fn into_response(self) -> Result<ChatResponse> {
        let mut resp = if !self.tool_calls.is_empty() {
            let calls: Vec<ToolCall> = self.tool_calls.values().map(|tc| {
                let arguments = serde_json::from_str(&tc.arguments).unwrap_or_else(|e| {
                    warn!(error = %e, id = %tc.id, name = %tc.name, "failed to parse accumulated tool call arguments");
                    serde_json::Value::Null
                });
                ToolCall { id: tc.id.clone(), name: tc.name.clone(), arguments }
            }).collect();
            ChatResponse::tool_calls(self.full_text, calls)
        } else {
            ChatResponse::text(self.full_text)
        };
        if let Some(u) = self.streamed_usage {
            resp = resp.with_usage(crate::TokenUsage {
                prompt_tokens: u.prompt_tokens,
                completion_tokens: u.completion_tokens,
                total_tokens: u.total_tokens,
                cost: u.cost,
            });
        }
        if let Some(m) = self.streamed_model {
            resp = resp.with_model(m);
        }
        Ok(resp)
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
        let api = ApiRequest::from_chat_request(&req, "glm-5.1", 4096, None, None);
        let json = serde_json::to_value(&api).unwrap();

        assert_eq!(json["model"], "glm-5.1");
        assert_eq!(json["max_tokens"], 4096);
        assert_eq!(json["messages"][0]["role"], "system");
        assert_eq!(json["messages"][0]["content"], "You are helpful.");
        assert_eq!(json["messages"][1]["role"], "user");
        assert_eq!(json["messages"][1]["content"], "Hello");
        // No tools → "tools" key should be absent.
        assert!(json.get("tools").is_none());
        // No temperature/top_p → absent.
        assert!(json.get("temperature").is_none());
        assert!(json.get("top_p").is_none());
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
        let api = ApiRequest::from_chat_request(&req, "glm-5.1", 4096, None, None);
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
        assert!(!result.has_tool_calls(), "expected Text response");
        assert_eq!(result.text, "Hello! How can I help?");
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
        assert!(result.has_tool_calls(), "expected ToolCalls response");
        let calls = &result.tool_calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_abc123");
        assert_eq!(calls[0].name, "run_command");
        assert_eq!(calls[0].arguments["command"], "ls -la");
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
        assert!(result.has_tool_calls(), "expected ToolCalls response");
        let calls = &result.tool_calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "list_dir");
        assert_eq!(calls[1].name, "run_command");
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
            ..Default::default()
        };
        let client = OpenAiCompatClient::new_with_key(&config, Duration::from_secs(60), &api_key).unwrap();

        let request = ChatRequest {
            messages: vec![
                ChatMessage::system("You are a helpful assistant. Reply in one sentence."),
                ChatMessage::user("What is 2 + 2?"),
            ],
            tools: vec![],
        };

        let response = client.chat(&request).await.expect("chat request failed");
        assert!(!response.has_tool_calls(), "expected Text response, got ToolCalls");
        assert!(!response.text.is_empty(), "response text should not be empty");
    }

    #[cfg(feature = "smoke")]
    #[tokio::test]
    async fn smoke_tool_call() {
        let api_key = std::env::var("GLM_API_KEY").expect("GLM_API_KEY must be set for smoke tests");
        let config = LlmConfig {
            model: "glm-4-flash".into(),
            api_base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            max_tokens: 256,
            ..Default::default()
        };
        let client = OpenAiCompatClient::new_with_key(&config, Duration::from_secs(60), &api_key).unwrap();

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
        if response.has_tool_calls() {
            assert!(!response.tool_calls.is_empty(), "expected at least one tool call");
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
        assert!(!response.has_tool_calls(), "expected Text response");
        assert_eq!(response.text, "Hello world");
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
        assert!(response.has_tool_calls(), "expected ToolCalls response");
        let calls = &response.tool_calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "run_command");
        assert_eq!(calls[0].arguments["command"], "ls");
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
        assert!(response.has_tool_calls(), "expected ToolCalls response");
        let calls = &response.tool_calls;
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].name, "run_command");
        assert_eq!(calls[1].id, "c2");
        assert_eq!(calls[1].name, "list_dir");
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

        // Final response has tool_calls (tool_calls take precedence).
        let response = state.into_response().unwrap();
        assert!(response.has_tool_calls(), "expected ToolCalls response");
        let calls = &response.tool_calls;
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "run_command");
        // The text preamble should be preserved.
        assert_eq!(response.text, "Let me check.");
    }

    #[test]
    fn sse_parse_empty_stream() {
        let state = SseState::new();
        let response = state.into_response().unwrap();
        assert!(!response.has_tool_calls(), "expected Text response");
        assert!(response.text.is_empty());
    }

    #[test]
    fn serialize_stream_request() {
        let req = ChatRequest {
            messages: vec![ChatMessage::user("Hello")],
            tools: vec![],
        };
        let mut api = ApiRequest::from_chat_request(&req, "glm-4", 4096, None, None);
        api.stream = Some(true);
        let json = serde_json::to_value(&api).unwrap();
        assert_eq!(json["stream"], true);

        // Without streaming, stream field should be absent.
        let api_no_stream = ApiRequest::from_chat_request(&req, "glm-4", 4096, None, None);
        let json_no_stream = serde_json::to_value(&api_no_stream).unwrap();
        assert!(json_no_stream.get("stream").is_none());
    }

    #[test]
    fn sse_parse_malformed_data_line() {
        // Malformed JSON in data line should be skipped gracefully.
        let mut state = SseState::new();
        state.process_chunk("data: not-json\n\n");
        state.process_chunk("data: [DONE]\n\n");
        let response = state.into_response().unwrap();
        assert!(!response.has_tool_calls(), "expected Text response");
        assert!(response.text.is_empty());
    }

    #[test]
    fn sse_parse_partial_chunk() {
        // Partial chunk (no line terminator) should not produce output
        // until the line is completed by a subsequent chunk.
        let mut state = SseState::new();
        let d1 = state.process_chunk("data: {\"choices\":[{\"delta\":{\"content\":\"Hi");
        assert!(d1.is_empty(), "no complete line yet");
        state.process_chunk("\"}}]}\n\n");
        let response = state.into_response().unwrap();
        assert!(!response.has_tool_calls(), "expected Text response");
        assert_eq!(response.text, "Hi");
    }

    #[test]
    fn sse_flush_partial_line_without_newline() {
        // Stream ends with a data line that has no trailing newline.
        // flush() should recover the final delta.
        let mut state = SseState::new();
        // First event complete.
        let d1 = state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hello\"}}]}\n\n",
        );
        assert_eq!(&d1, &["Hello".to_string()]);
        // Second event — no trailing newline.
        let d2 = state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"content\":\"end\"}}]}",
        );
        assert!(d2.is_empty(), "no complete line yet");
        // flush should process the remaining buffer.
        let d3 = state.flush();
        assert_eq!(&d3, &["end".to_string()]);
        let response = state.into_response().unwrap();
        assert!(!response.has_tool_calls(), "expected Text response");
        assert_eq!(response.text, "Helloend");
    }

    #[test]
    fn sse_flush_done_without_newline() {
        // Stream ends with `data: [DONE]` but no trailing newline.
        let mut state = SseState::new();
        state.process_chunk(
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
        );
        // [DONE] without newline.
        state.process_chunk("data: [DONE]");
        assert!(!state.done, "done should not be set until line is processed");
        let deltas = state.flush();
        assert!(deltas.is_empty(), "[DONE] produces no text deltas");
        assert!(state.done, "flush should set done flag");
        let response = state.into_response().unwrap();
        assert!(!response.has_tool_calls(), "expected Text response");
        assert_eq!(response.text, "ok");
    }

    #[test]
    fn sse_flush_empty_buffer_noop() {
        // Flushing an empty buffer should be a no-op.
        let mut state = SseState::new();
        let deltas = state.flush();
        assert!(deltas.is_empty());
        let response = state.into_response().unwrap();
        assert!(!response.has_tool_calls(), "expected Text response");
        assert!(response.text.is_empty());
    }

    #[test]
    fn sse_raw_buffer_flush_on_stream_end() {
        // Simulate the chat_stream None-branch logic: raw_buffer has
        // leftover bytes without a trailing newline.  The flush path
        // should decode, process, and emit the final delta.
        let mut state = SseState::new();
        let mut raw_buffer: Vec<u8> =
            b"data: {\"choices\":[{\"delta\":{\"content\":\"end\"}}]}".to_vec();

        // Normal processing loop — no newline found, nothing processed.
        let mut collected_deltas: Vec<String> = Vec::new();
        while let Some(pos) = raw_buffer.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&raw_buffer[..=pos]).into_owned();
            raw_buffer = raw_buffer[pos + 1..].to_vec();
            for d in state.process_chunk(&line) {
                collected_deltas.push(d);
            }
        }
        assert!(collected_deltas.is_empty(), "no complete line yet");

        // Stream ended — flush raw_buffer (same logic as chat_stream).
        if !raw_buffer.is_empty() {
            let leftover = String::from_utf8_lossy(&raw_buffer).into_owned();
            for d in state.process_chunk(&format!("{}\n", leftover)) {
                collected_deltas.push(d);
            }
        }
        // Flush SseState buffer.
        for d in state.flush() {
            collected_deltas.push(d);
        }

        assert_eq!(&collected_deltas, &["end".to_string()]);
        let response = state.into_response().unwrap();
        assert!(!response.has_tool_calls(), "expected Text response");
        assert_eq!(response.text, "end");
    }

    // ── LLM parameter tests ────────────────────────────────────────────

    #[test]
    fn golden_no_params_unchanged() {
        let req = ChatRequest {
            messages: vec![ChatMessage::user("Hello")],
            tools: vec![],
        };
        let api = ApiRequest::from_chat_request(&req, "glm-5.1", 4096, None, None);
        let json = serde_json::to_value(&api).unwrap();
        // Body should NOT contain temperature or top_p.
        assert!(json.get("temperature").is_none());
        assert!(json.get("top_p").is_none());
        // Core fields present.
        assert_eq!(json["model"], "glm-5.1");
        assert_eq!(json["max_tokens"], 4096);
    }

    #[test]
    fn with_temperature_and_top_p() {
        let req = ChatRequest {
            messages: vec![ChatMessage::user("Hello")],
            tools: vec![],
        };
        let api = ApiRequest::from_chat_request(&req, "glm-5.1", 4096, Some(0.5), Some(0.25));
        let json = serde_json::to_value(&api).unwrap();
        assert_eq!(json["temperature"].as_f64().unwrap(), 0.5);
        assert_eq!(json["top_p"].as_f64().unwrap(), 0.25);
    }

    #[test]
    fn extra_body_merges_into_request() {
        let mut body = serde_json::json!({
            "model": "glm-5.1",
            "messages": [],
            "max_tokens": 4096
        });
        let extra = serde_json::json!({
            "thinking": { "type": "disabled" },
            "reasoning_effort": "low"
        });
        merge_extra_body(&mut body, &extra);
        assert_eq!(body["thinking"]["type"], "disabled");
        assert_eq!(body["reasoning_effort"], "low");
        // Original fields intact.
        assert_eq!(body["model"], "glm-5.1");
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn extra_body_protected_keys_ignored() {
        let mut body = serde_json::json!({
            "model": "glm-5.1",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [],
            "stream": false,
            "max_tokens": 4096
        });
        let extra = serde_json::json!({
            "model": "hacked",
            "messages": [],
            "tools": [{"type": "function"}],
            "stream": true,
            "temperature": 0.5
        });
        merge_extra_body(&mut body, &extra);
        // Protected keys unchanged.
        assert_eq!(body["model"], "glm-5.1");
        assert_eq!(body["messages"][0]["content"], "hi");
        assert!(body["tools"].as_array().unwrap().is_empty());
        assert_eq!(body["stream"], false);
        // Non-protected key allowed.
        assert_eq!(body["temperature"], 0.5);
    }

    #[test]
    fn extra_body_overrides_max_tokens() {
        let mut body = serde_json::json!({
            "model": "glm-5.1",
            "max_tokens": 4096
        });
        let extra = serde_json::json!({ "max_tokens": 8192 });
        merge_extra_body(&mut body, &extra);
        assert_eq!(body["max_tokens"], 8192);
    }

    #[test]
    fn extra_body_non_object_ignored() {
        let mut body = serde_json::json!({ "model": "glm-5.1" });
        let extra = serde_json::json!(42);
        merge_extra_body(&mut body, &extra);
        assert_eq!(body["model"], "glm-5.1");
    }

    #[test]
    #[allow(deprecated)]
    fn glm_client_alias_still_compiles() {
        // The deprecated `GlmClient` alias (re-exported in `lib.rs`) must
        // resolve to the same type as `OpenAiCompatClient`, so existing engine
        // consumers (bots, mobile) keep compiling until the next major engine
        // tag removes the alias.
        fn assert_same_type(_: &OpenAiCompatClient) {}
        let config = LlmConfig {
            model: "glm-5.1".into(),
            api_base_url: "https://open.bigmodel.cn/api/paas/v4".into(),
            max_tokens: 64,
            ..Default::default()
        };
        let client = crate::GlmClient::new_with_key(&config, Duration::from_secs(1), "dummy-key")
            .unwrap();
        assert_same_type(&client);
    }

    #[test]
    fn deserialize_response_with_usage() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        let usage = resp.usage.unwrap();
        assert_eq!(usage.prompt_tokens, Some(10));
        assert_eq!(usage.completion_tokens, Some(5));
        assert_eq!(usage.total_tokens, Some(15));
    }

    #[test]
    fn deserialize_response_without_usage() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"hello"}}]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.usage.is_none(), "usage must be None when absent");
    }

    #[test]
    fn deserialize_response_with_cost_and_model() {
        let json = r#"{
            "model": "openai/gpt-4o-mini",
            "choices": [{"message": {"role": "assistant", "content": "hello"}}],
            "usage": {
                "prompt_tokens": 10,
                "completion_tokens": 20,
                "total_tokens": 30,
                "cost": 0.00015
            }
        }"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.model.as_deref(), Some("openai/gpt-4o-mini"));
        assert_eq!(resp.usage.as_ref().and_then(|u| u.cost), Some(0.00015));
        let chat = resp.try_into_chat_response().unwrap();
        assert_eq!(chat.model.as_deref(), Some("openai/gpt-4o-mini"));
        let u = chat.usage.unwrap();
        assert_eq!(u.cost, Some(0.00015));
    }

    #[test]
    fn deserialize_response_without_cost_or_model() {
        let json = r#"{"choices":[{"message":{"role":"assistant","content":"ok"}}]}"#;
        let resp: ApiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.model.is_none());
        let chat = resp.try_into_chat_response().unwrap();
        assert!(chat.model.is_none());
        assert!(chat.usage.is_none());
    }

    #[test]
    fn sse_parse_model_from_stream() {
        let mut state = SseState::new();
        state.process_chunk("data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}],\"model\":\"cohere/command-r\"}\n");
        state.process_chunk("data: [DONE]\n");
        let resp = state.into_response().unwrap();
        assert_eq!(resp.model.as_deref(), Some("cohere/command-r"));
    }

    #[test]
    fn empty_api_key_does_not_send_authorization() {
        let config = LlmConfig {
            model: "local".into(),
            api_base_url: "http://localhost:11434/v1".into(),
            max_tokens: 128,
            temperature: None,
            top_p: None,
            extra_body: None,
        };
        let client = OpenAiCompatClient::new_with_key(&config, Duration::from_secs(30), "").unwrap();
        assert!(!client.sends_authorization());
        let with_key =
            OpenAiCompatClient::new_with_key(&config, Duration::from_secs(30), "sk-test").unwrap();
        assert!(with_key.sends_authorization());
    }

    #[test]
    fn tools_unsupported_heuristic_matches_common_bodies() {
        assert!(looks_like_tools_unsupported(
            r#"{"error":"does not support tools"}"#
        ));
        assert!(looks_like_tools_unsupported(
            "model does not support function calling"
        ));
        assert!(!looks_like_tools_unsupported("rate limited, try later"));
        assert!(!looks_like_tools_unsupported(
            "invalid parameter: temperature"
        ));
        let err = ApiError::from_http_status(400, "tools are not supported by this model".into());
        assert!(matches!(err, ApiError::ToolsUnsupported));
        let msg = err.to_string();
        assert!(
            msg.contains("does not support tool calling"),
            "user-facing message must be clear, got: {msg}"
        );
        assert!(
            !msg.contains("tools are not supported by this model"),
            "raw provider body must stay in logs, not the user message"
        );
        // 5xx with similar wording must stay retryable Server, not ToolsUnsupported.
        let server = ApiError::from_http_status(
            503,
            "tools are not supported by this model".into(),
        );
        assert!(matches!(server, ApiError::Server(503, _)));
        assert!(server.is_retryable());
        let rate = ApiError::from_http_status(429, "tools not available, retry".into());
        assert!(matches!(rate, ApiError::RateLimit(_)));
        assert!(rate.is_retryable());
    }

    #[test]
    fn timeout_message_hints_local_llm_secs() {
        let err = ApiError::Timeout(Duration::from_secs(60));
        let msg = err.to_string();
        assert!(msg.contains("llm_secs"), "timeout must hint llm_secs: {msg}");
    }

    #[test]
    fn error_chain_includes_sources() {
        #[derive(Debug)]
        struct Inner;
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "connection closed before message completed")
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                write!(f, "error decoding response body")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let text = describe_error_chain(&Outer(Inner));
        assert!(
            text.contains("error decoding response body"),
            "top level must be kept: {text}"
        );
        assert!(
            text.contains("connection closed"),
            "source must be unwrapped: {text}"
        );
    }

    // ── Mid-stream retry (#374) ──────────────────────────────────────────
    //
    // A scripted fake provider over a real loopback socket. Each `Step` serves
    // exactly one connection, so the script also asserts how many attempts the
    // client made.

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// One scripted connection of the fake provider.
    struct Step {
        /// SSE frames to send, each as one chunked-encoding chunk.
        frames: Vec<String>,
        /// Pause before each frame — models a slow but alive stream.
        gap: Duration,
        /// `true` terminates the chunked body properly; `false` drops the
        /// socket mid-body, which is what a lost connection looks like.
        complete: bool,
    }

    impl Step {
        /// Send the frames, then drop the socket without terminating the
        /// chunked body — what a connection lost mid-answer looks like.
        fn dropped(frames: Vec<String>) -> Self {
            Self {
                frames,
                gap: Duration::ZERO,
                complete: false,
            }
        }

        /// Send the frames and terminate the body properly.
        fn finished(frames: Vec<String>) -> Self {
            Self {
                frames,
                gap: Duration::ZERO,
                complete: true,
            }
        }

        fn with_gap(mut self, gap: Duration) -> Self {
            self.gap = gap;
            self
        }
    }

    /// SSE frame carrying a text delta.
    fn text_frame(content: &str) -> String {
        format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{content}\"}}}}]}}\n\n")
    }

    /// First frame of a typical response: announces the role, no content yet.
    fn role_frame() -> String {
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n".to_string()
    }

    fn done_frame() -> String {
        "data: [DONE]\n\n".to_string()
    }

    /// Read one HTTP request (headers plus `Content-Length` body) and discard
    /// it — the fake provider replies from its script regardless of content.
    async fn drain_request(socket: &mut tokio::net::TcpStream) {
        let mut buf: Vec<u8> = Vec::new();
        let mut tmp = [0u8; 2048];
        loop {
            let Ok(n) = socket.read(&mut tmp).await else {
                return;
            };
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&tmp[..n]);
            let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) else {
                continue;
            };
            let head = String::from_utf8_lossy(&buf[..end]).to_lowercase();
            let len = head
                .split("content-length:")
                .nth(1)
                .and_then(|s| s.split("\r\n").next())
                .and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0);
            let mut have = buf.len() - end;
            while have < len {
                let Ok(n) = socket.read(&mut tmp).await else {
                    return;
                };
                if n == 0 {
                    return;
                }
                have += n;
            }
            return;
        }
    }

    /// Start the fake provider. Returns its base URL and a counter of served
    /// connections (= attempts the client made).
    async fn spawn_provider(steps: Vec<Step>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let served = Arc::new(AtomicUsize::new(0));
        let counter = served.clone();

        tokio::spawn(async move {
            for step in steps {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                counter.fetch_add(1, Ordering::SeqCst);
                drain_request(&mut socket).await;

                let head = "HTTP/1.1 200 OK\r\n\
                            Content-Type: text/event-stream\r\n\
                            Transfer-Encoding: chunked\r\n\r\n";
                if socket.write_all(head.as_bytes()).await.is_err() {
                    continue;
                }
                let _ = socket.flush().await;

                for frame in &step.frames {
                    if !step.gap.is_zero() {
                        tokio::time::sleep(step.gap).await;
                    }
                    let chunk = format!("{:x}\r\n{}\r\n", frame.len(), frame);
                    if socket.write_all(chunk.as_bytes()).await.is_err() {
                        break;
                    }
                    let _ = socket.flush().await;
                }

                if step.complete {
                    let _ = socket.write_all(b"0\r\n\r\n").await;
                    let _ = socket.flush().await;
                    // Give the client a moment to read the terminator before
                    // the socket is dropped.
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                drop(socket);
            }
        });

        (format!("http://{addr}"), served)
    }

    fn stream_test_client(base_url: &str, retries: u32, timeout: Duration) -> OpenAiCompatClient {
        let config = LlmConfig {
            model: "test-model".into(),
            api_base_url: base_url.to_string(),
            max_tokens: 128,
            ..Default::default()
        };
        OpenAiCompatClient::new_with_key(&config, timeout, "test-key")
            .unwrap()
            .with_max_retries(retries)
            .with_backoff_base(Duration::from_millis(10))
    }

    fn simple_request() -> ChatRequest {
        ChatRequest {
            messages: vec![ChatMessage::user("hi")],
            tools: vec![],
        }
    }

    /// A drop before any content delta is retried, and the retry's output is
    /// delivered exactly once.
    #[tokio::test]
    async fn stream_retries_when_dropped_before_any_delta() {
        let (url, served) = spawn_provider(vec![
            Step::dropped(vec![role_frame()]),
            Step::finished(vec![
                role_frame(),
                text_frame("Hello"),
                text_frame(" world"),
                done_frame(),
            ]),
        ])
        .await;

        let client = stream_test_client(&url, 1, Duration::from_secs(5));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let callback = move |d: String| sink.lock().unwrap().push(d);

        let response = client
            .chat_stream(&simple_request(), &callback)
            .await
            .expect("second attempt must succeed");

        assert_eq!(response.text, "Hello world");
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["Hello".to_string(), " world".to_string()],
            "deltas must be delivered once, without replaying the first attempt"
        );
        assert_eq!(served.load(Ordering::SeqCst), 2, "expected exactly one retry");
    }

    /// When every attempt fails, the error names the attempt count, the elapsed
    /// time and the unwrapped cause.
    #[tokio::test]
    async fn stream_gives_up_after_all_attempts_with_diagnostic() {
        let (url, served) = spawn_provider(vec![
            Step::dropped(vec![role_frame()]),
            Step::dropped(vec![role_frame()]),
            Step::dropped(vec![role_frame()]),
        ])
        .await;

        let client = stream_test_client(&url, 2, Duration::from_secs(5));
        let callback = |_: String| {};

        let err = client
            .chat_stream(&simple_request(), &callback)
            .await
            .expect_err("all attempts fail, so the call must fail");
        let msg = err.to_string();

        assert!(msg.contains("3 attempts"), "attempt count missing: {msg}");
        assert!(msg.contains("over "), "elapsed time missing: {msg}");
        assert!(
            msg.contains("network error"),
            "classified cause missing: {msg}"
        );
        assert_eq!(served.load(Ordering::SeqCst), 3);
    }

    /// Once deltas have reached the caller there is no retry: replaying would
    /// show the answer twice.
    #[tokio::test]
    async fn stream_does_not_retry_after_partial_output() {
        let (url, served) = spawn_provider(vec![
            Step::dropped(vec![role_frame(), text_frame("Hel")]),
            Step::finished(vec![text_frame("Hello"), done_frame()]),
        ])
        .await;

        let client = stream_test_client(&url, 1, Duration::from_secs(5));
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = seen.clone();
        let callback = move |d: String| sink.lock().unwrap().push(d);

        let err = client
            .chat_stream(&simple_request(), &callback)
            .await
            .expect_err("a partial response must not be retried");

        assert!(
            err.to_string().contains("partial response"),
            "error must say the response was partial: {err}"
        );
        assert_eq!(
            *seen.lock().unwrap(),
            vec!["Hel".to_string()],
            "no duplicated text"
        );
        assert_eq!(
            served.load(Ordering::SeqCst),
            1,
            "the second script step must stay unused"
        );
    }

    /// A stream that runs longer than the configured timeout but never stalls
    /// is not cut off: the timeout bounds silence between chunks, not the total
    /// length of the answer. Regression test for the total-timeout behaviour of
    /// `Client::timeout` on streaming bodies.
    #[tokio::test]
    async fn stream_outlives_timeout_while_chunks_keep_arriving() {
        let timeout = Duration::from_millis(300);
        let frames = vec![
            role_frame(),
            text_frame("a"),
            text_frame("b"),
            text_frame("c"),
            text_frame("d"),
            done_frame(),
        ];
        // 6 frames × 100 ms ≈ 600 ms total — twice the timeout, but no single
        // gap exceeds it.
        let (url, served) =
            spawn_provider(vec![
                Step::finished(frames).with_gap(Duration::from_millis(100))
            ])
            .await;

        let client = stream_test_client(&url, 0, timeout);
        let callback = |_: String| {};

        let response = client
            .chat_stream(&simple_request(), &callback)
            .await
            .expect("a slow but alive stream must not be cut off");

        assert_eq!(response.text, "abcd");
        assert_eq!(served.load(Ordering::SeqCst), 1, "no retry expected");
    }
}
