//! Log sources (#579): centralised log stores the agent queries over HTTP.
//!
//! A [`LogSource`] speaks one store's search API (VictoriaLogs, OpenSearch,
//! Elasticsearch, Loki — the adapters are #580–#582). The query text is in
//! the store's own language; filar does not translate it. What filar owns is
//! the frame around every query, which the model cannot widen:
//!
//! - a window no longer than the source's `max_window_hours` — a query
//!   without a window is never sent;
//! - at most `max_lines` lines;
//! - a byte cap per line and per answer — the body is read as a stream and
//!   the connection is dropped at the cap ([`read_limited`]);
//! - a timeout; cancelling drops the request and closes the connection
//!   (whether the store then stops the query depends on the backend).
//!
//! Adapters only ever call search endpoints: read-only by construction. The
//! request goes straight from the user's machine to the user's store
//! (invariant 5), and the key never appears in errors or logs (invariant 3).
//!
//! The model sees a folded view ([`fold`]), redacted like pod logs.

pub mod fold;
pub mod tool;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use filar_core::{CoreError, LogAuth, LogSourceConfig, Result, SecretProvider};
use futures::StreamExt;

pub use fold::fold_logs;
pub use tool::{
    build_log_sources, LogSources, QueryLogsCall, LOG_SOURCES_PROMPT_HEADER, TOOL_QUERY_LOGS,
};

/// One query, in the source's own language, inside a window.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    /// Query text in the source's language (LogsQL, `query_string` / DSL, LogQL).
    pub query: String,
    /// Window start, inclusive.
    pub start: DateTime<Utc>,
    /// Window end, exclusive.
    pub end: DateTime<Utc>,
    /// Most lines to return; already within the source's `max_lines`.
    pub limit: usize,
}

/// One log line as the store returned it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogLine {
    /// When the line was written, if the store says.
    pub time: Option<DateTime<Utc>>,
    /// The message, cut to the source's `max_line_bytes`.
    pub message: String,
    /// Other fields (stream labels, Kubernetes metadata), as text.
    pub fields: BTreeMap<String, String>,
    /// Whether the message was cut.
    pub truncated: bool,
}

/// What a query returned.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LogResult {
    /// The lines, newest first when the store sorts.
    pub lines: Vec<LogLine>,
    /// More data existed than was returned: the line limit was reached or the
    /// answer hit its byte cap.
    pub truncated: bool,
    /// A short note from the adapter (e.g. "answer cut at 2 MiB").
    pub note: Option<String>,
}

/// A centralised log store the agent may query.
///
/// Implementations call only search endpoints, apply [`LogLimits`] through
/// [`read_limited`] and [`cap_line`], and never put the key into an error.
#[async_trait]
pub trait LogSource: Send + Sync {
    /// The source's `[[log_sources]]` entry.
    fn config(&self) -> &LogSourceConfig;

    /// Query language and the main fields — a hint for the system prompt.
    fn describe(&self) -> String;

    /// Run one query. Dropping the future closes the connection.
    async fn query(&self, query: &LogQuery) -> Result<LogResult>;
}

/// Byte caps of one source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogLimits {
    /// Cap of one line's message.
    pub max_line_bytes: usize,
    /// Cap of one answer's body.
    pub max_response_bytes: usize,
}

impl LogLimits {
    /// The caps configured for `cfg`.
    pub fn of(cfg: &LogSourceConfig) -> Self {
        Self {
            max_line_bytes: cfg.max_line_bytes,
            max_response_bytes: cfg.max_response_bytes,
        }
    }
}

/// Cut `message` to at most `max_bytes` bytes on a character boundary. The
/// second value says whether anything was cut; a cut message ends with a
/// marker naming the original size.
pub fn cap_line(message: &str, max_bytes: usize) -> (String, bool) {
    if message.len() <= max_bytes {
        return (message.to_string(), false);
    }
    let mut end = max_bytes;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!("{}… [line cut: {} bytes]", &message[..end], message.len()),
        true,
    )
}

/// A body read up to a byte cap.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LimitedBody {
    /// At most the cap's worth of bytes.
    pub bytes: Vec<u8>,
    /// Whether the body went on past the cap (the rest was not read).
    pub truncated: bool,
}

/// Read `response`'s body as a stream, stopping at `max_bytes`: once the cap
/// is reached the response is dropped, which closes the connection, so a
/// huge answer is never downloaded in full.
pub async fn read_limited(response: reqwest::Response, max_bytes: usize) -> Result<LimitedBody> {
    let mut body = LimitedBody::default();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| CoreError::Other(format!("log source: reading the answer failed: {}", without_url(&e))))?;
        let room = max_bytes - body.bytes.len();
        if chunk.len() > room {
            body.bytes.extend_from_slice(&chunk[..room]);
            body.truncated = true;
            break;
        }
        body.bytes.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// A reqwest error without the URL (it may carry query text the user did not
/// mean to log); the key is never in a URL (`LogSourceConfig::validate`).
fn without_url(e: &reqwest::Error) -> String {
    if e.is_timeout() {
        return "timed out".into();
    }
    let mut text = e.to_string();
    if let Some(url) = e.url() {
        text = text.replace(url.as_str(), "<url>");
    }
    if e.is_connect() {
        format!("cannot connect ({text})")
    } else {
        text
    }
}

/// An HTTP client for one source: its timeout, its auth, its key.
///
/// The key is held here and only here; [`LogHttp::error`] scrubs it from
/// anything that reaches the model, the feed or a log.
pub struct LogHttp {
    client: reqwest::Client,
    auth: LogAuth,
    user: Option<String>,
    key: String,
    /// Base URL without a trailing `/`.
    pub base_url: String,
    /// The source's byte caps.
    pub limits: LogLimits,
    name: String,
}

impl LogHttp {
    /// Build the client for `cfg` with the already resolved `key` (empty
    /// when `auth = "none"`).
    pub fn new(cfg: &LogSourceConfig, key: &str) -> Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(cfg.timeout_secs))
            .connect_timeout(Duration::from_secs(cfg.timeout_secs.min(10)))
            .build()
            .map_err(|e| CoreError::Other(format!("log source \"{}\": HTTP client: {e}", cfg.name)))?;
        Ok(Self {
            client,
            auth: cfg.auth_kind().unwrap_or(LogAuth::None),
            user: cfg.user.clone(),
            key: key.to_string(),
            base_url: cfg.url.trim().trim_end_matches('/').to_string(),
            limits: LogLimits::of(cfg),
            name: cfg.name.clone(),
        })
    }

    /// A request to `{base_url}{path}` with the source's auth applied.
    pub fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let builder = self.client.request(method, format!("{}{path}", self.base_url));
        match self.auth {
            LogAuth::None => builder,
            LogAuth::Basic => builder.basic_auth(self.user.as_deref().unwrap_or(""), Some(&self.key)),
            LogAuth::Bearer => builder.bearer_auth(&self.key),
            LogAuth::ApiKey => builder.header(reqwest::header::AUTHORIZATION, format!("ApiKey {}", self.key)),
        }
    }

    /// Send `request`; a non-2xx status becomes an error carrying the start of
    /// the server's answer (the key scrubbed), 401/403 named as such.
    pub async fn send(&self, request: reqwest::RequestBuilder) -> Result<reqwest::Response> {
        let response = request
            .send()
            .await
            .map_err(|e| self.error(&format!("request failed: {}", without_url(&e))))?;
        let status = response.status();
        if status.is_success() {
            return Ok(response);
        }
        let body = read_limited(response, 2048).await.unwrap_or_default();
        let text = String::from_utf8_lossy(&body.bytes).trim().to_string();
        let what = match status.as_u16() {
            401 => "authentication failed (401) — check the key and auth".to_string(),
            403 => "access denied (403) — the key lacks rights for this search".to_string(),
            _ => format!("HTTP {status}"),
        };
        Err(self.error(&if text.is_empty() { what } else { format!("{what}: {text}") }))
    }

    /// An error of this source with the key scrubbed from `message`.
    pub fn error(&self, message: &str) -> CoreError {
        CoreError::Other(format!("log source \"{}\": {}", self.name, self.scrub(message)))
    }

    /// `text` with the key (and its basic-auth form) replaced by `***`.
    pub fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        if !self.key.is_empty() {
            out = out.replace(&self.key, "***");
            if let Some(user) = &self.user {
                use base64_lite::encode;
                out = out.replace(&encode(&format!("{user}:{}", self.key)), "***");
            }
        }
        out
    }
}

/// Minimal base64 (standard alphabet, padded) for scrubbing a basic-auth
/// token out of a server's echo; no extra dependency for one helper.
mod base64_lite {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    pub fn encode(input: &str) -> String {
        let bytes = input.as_bytes();
        let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
            let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
            out.push(ALPHABET[(n >> 18) as usize & 63] as char);
            out.push(ALPHABET[(n >> 12) as usize & 63] as char);
            out.push(if chunk.len() > 1 { ALPHABET[(n >> 6) as usize & 63] as char } else { '=' });
            out.push(if chunk.len() > 2 { ALPHABET[n as usize & 63] as char } else { '=' });
        }
        out
    }

    #[cfg(test)]
    #[test]
    fn matches_known_vectors() {
        assert_eq!(encode("reader:s3cret"), "cmVhZGVyOnMzY3JldA==");
        assert_eq!(encode("ab"), "YWI=");
        assert_eq!(encode(""), "");
    }
}

/// Build the adapter for `cfg` with its resolved `key`. Kinds without an
/// adapter yet are an error naming the kind.
pub fn build_log_source(cfg: &LogSourceConfig, key: &str) -> Result<Arc<dyn LogSource>> {
    cfg.validate()?;
    let _ = key;
    Err(CoreError::Config(format!(
        "log source \"{}\": kind \"{}\" has no adapter in this build",
        cfg.name, cfg.kind
    )))
}

/// The key of `cfg` from `secrets` under its `key_env`; empty for
/// `auth = "none"`. The error names the variable, never a value.
pub fn resolve_log_key(cfg: &LogSourceConfig, secrets: &dyn SecretProvider) -> Result<String> {
    let needs = cfg.auth_kind().is_some_and(LogAuth::needs_key);
    let key_env = cfg.key_env.trim();
    if !needs || key_env.is_empty() {
        return Ok(String::new());
    }
    secrets
        .get(key_env)
        .ok()
        .filter(|k| !k.is_empty())
        .ok_or_else(|| CoreError::Secret(format!("log source \"{}\": key {key_env} is not set", cfg.name)))
}

#[cfg(test)]
pub(crate) mod tests;
