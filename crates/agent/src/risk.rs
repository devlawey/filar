//! Command safety scoring by a decision model (#549).
//!
//! A [`RiskScorer`] rates how safe it is to run a command the agent proposed:
//! `safety_pct` 0–100, where **100 = completely safe**. The rating is
//! advisory only — it never approves, denies or delays a command — and it
//! is independent of the arbiter (`arbiter.rs`), which audits only commands
//! awaiting confirmation.
//!
//! The scorer is meant for *decision models*: models that do not generate
//! text but return a probability distribution over the answers to a typed
//! question (e.g. `typesafe/jev-1.14` on OpenRouter). The rating is the
//! probability-weighted position on an ordered scale of danger levels
//! ([`RISK_LEVELS`]), so the percentage comes from the model's distribution
//! rather than from a number a chat model wrote.
//!
//! [`DecisionsApiScorer`] speaks the Decisions API
//! (`POST {base_url}/decisions`). [`ChatLogprobScorer`] makes any
//! OpenAI-compatible chat model a scorer, as long as its server returns
//! `logprobs`: the model answers with one digit and the level distribution is
//! read from that token's log-probabilities (#550). Other protocols plug in
//! behind the same trait.

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use filar_core::{CoreError, Result, RiskScorerConfig, RiskScorerKind, SecretProvider};
use serde::Deserialize;

/// The ordered danger levels the decision model places a command on, from
/// safest (index 0) to most dangerous. One shared scale for every adapter.
pub const RISK_LEVELS: [&str; 5] = [
    "Read-only: only reads or inspects state, no side effects at all.",
    "Reversible local change: modifies the user's own files or temporary data and is easy to undo.",
    "Configuration change or service restart: changes system configuration or restarts a service; recoverable.",
    "Possible data loss or wide blast radius: recursive or wildcard operations, shared services, network or firewall changes.",
    "Irreversible and/or system-wide: destroys data or affects the whole system with no way back.",
];

/// The question asked about every command.
pub const RISK_QUESTION: &str = "How dangerous is it to run this shell command on the described target? \
Place it on the ordered scale from the safest level to the most dangerous one.";

/// Name of the question in the Decisions API request.
const QUESTION_KEY: &str = "risk";

/// Upper bound on the `state` text sent to the scorer. Requests above ~32 KB
/// of text run in the decision model's slower, less accurate long-context
/// mode, so the input is kept far below that.
pub const MAX_STATE_CHARS: usize = 4000;

/// What the scorer is told about a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RiskInput {
    /// The command as it will run.
    pub command: String,
    /// The agent's explanation of the command (may be empty).
    pub explanation: String,
    /// Where it runs: local machine or `user@host`, as already known.
    pub target: String,
}

impl RiskInput {
    /// The compact `state` text for the decision model, capped at
    /// [`MAX_STATE_CHARS`].
    pub fn state_text(&self) -> String {
        let mut s = format!("Target: {}\nCommand: {}\n", self.target, self.command);
        if !self.explanation.trim().is_empty() {
            s.push_str("Agent's explanation: ");
            s.push_str(self.explanation.trim());
            s.push('\n');
        }
        truncate_chars(&s, MAX_STATE_CHARS)
    }
}

/// A safety rating of one command.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskScore {
    /// 0–100, where 100 = completely safe.
    pub safety_pct: u8,
    /// The model's confidence in its answer, when reported.
    pub confidence: Option<f32>,
    /// Probability per level index (for debugging), when reported.
    pub probabilities: BTreeMap<usize, f32>,
    /// Cost of the request in USD, when reported.
    pub cost_usd: Option<f64>,
    /// The model that produced the rating.
    pub model: String,
}

/// Rates how safe it is to run a proposed command.
///
/// Implementations must not log or return the API key, and should fail
/// fast: the agent shows "unavailable" for any error.
#[async_trait]
pub trait RiskScorer: Send + Sync {
    /// Rate `input`. 0 = dangerous, 100 = safe.
    async fn score(&self, input: &RiskInput) -> Result<RiskScore>;

    /// Model name shown next to the rating.
    fn model_name(&self) -> &str;
}

/// Turn a position on the [`RISK_LEVELS`] scale (0 = safest,
/// `levels − 1` = most dangerous) into a safety percentage
/// (100 = safe), clamped to 0..=100.
pub fn safety_pct_from_position(position: f64, levels: usize) -> u8 {
    if levels < 2 || !position.is_finite() {
        return 0;
    }
    let max = (levels - 1) as f64;
    let pos = position.clamp(0.0, max);
    let pct = 100.0 - pos / max * 100.0;
    pct.round().clamp(0.0, 100.0) as u8
}

/// Expected level index from a distribution over level indices.
/// `None` when the distribution is empty or sums to zero.
pub fn expected_position(probabilities: &BTreeMap<usize, f32>) -> Option<f64> {
    let total: f64 = probabilities.values().map(|p| f64::from(*p).max(0.0)).sum();
    if total <= 0.0 || !total.is_finite() {
        return None;
    }
    let weighted: f64 = probabilities
        .iter()
        .map(|(i, p)| *i as f64 * f64::from(*p).max(0.0))
        .sum();
    Some(weighted / total)
}

fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Decisions API
// ---------------------------------------------------------------------------

/// [`RiskScorer`] over the Decisions API: one `score` question with the
/// [`RISK_LEVELS`] as ordered criteria.
pub struct DecisionsApiScorer {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
}

impl DecisionsApiScorer {
    /// Build a scorer for `base_url` / `model` with an already resolved key
    /// (empty = send no `Authorization` header).
    pub fn new(base_url: &str, model: &str, api_key: &str, timeout: Duration) -> Result<Self> {
        Ok(Self {
            http: scorer_http_client(api_key, timeout)?,
            endpoint: format!("{}/decisions", base_url.trim().trim_end_matches('/')),
            model: model.trim().to_string(),
            api_key: api_key.to_string(),
        })
    }

    /// The JSON request body for `input`.
    pub fn request_body(&self, input: &RiskInput) -> serde_json::Value {
        build_decisions_request(&self.model, input)
    }
}

/// Decisions API request body: `state` text plus one `score` question.
pub fn build_decisions_request(model: &str, input: &RiskInput) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "state": input.state_text(),
        "questions": {
            QUESTION_KEY: {
                "type": "score",
                "instructions": RISK_QUESTION,
                "criteria": RISK_LEVELS,
            }
        }
    })
}

#[derive(Debug, Deserialize)]
struct DecisionsResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    answers: BTreeMap<String, serde_json::Value>,
    #[serde(default)]
    usage: Option<DecisionsUsage>,
}

#[derive(Debug, Deserialize)]
struct DecisionsUsage {
    #[serde(default)]
    cost: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ScoreAnswer {
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    confidence: Option<f32>,
    #[serde(default)]
    probabilities: BTreeMap<String, f32>,
}

/// Parse a Decisions API response body into a [`RiskScore`].
///
/// The rating is taken from `score` (the probability-weighted level index);
/// when it is missing, from the expected index over `probabilities`.
pub fn parse_decisions_response(body: &str, requested_model: &str) -> Result<RiskScore> {
    let resp: DecisionsResponse = serde_json::from_str(body)
        .map_err(|e| CoreError::Other(format!("risk scorer: unexpected response: {e}")))?;
    let answer = resp
        .answers
        .get(QUESTION_KEY)
        .ok_or_else(|| CoreError::Other("risk scorer: no answer in response".into()))?;
    let answer: ScoreAnswer = serde_json::from_value(answer.clone())
        .map_err(|e| CoreError::Other(format!("risk scorer: unexpected answer: {e}")))?;

    let probabilities: BTreeMap<usize, f32> = answer
        .probabilities
        .iter()
        .filter_map(|(k, v)| k.trim().parse::<usize>().ok().map(|i| (i, *v)))
        .filter(|(i, v)| *i < RISK_LEVELS.len() && v.is_finite())
        .collect();

    let position = answer
        .score
        .filter(|s| s.is_finite())
        .or_else(|| expected_position(&probabilities))
        .ok_or_else(|| CoreError::Other("risk scorer: answer has no score".into()))?;

    Ok(RiskScore {
        safety_pct: safety_pct_from_position(position, RISK_LEVELS.len()),
        confidence: answer.confidence.filter(|c| c.is_finite()),
        probabilities,
        cost_usd: resp.usage.and_then(|u| u.cost),
        model: resp.model.unwrap_or_else(|| requested_model.to_string()),
    })
}

#[async_trait]
impl RiskScorer for DecisionsApiScorer {
    async fn score(&self, input: &RiskInput) -> Result<RiskScore> {
        let body = self.request_body(input);
        let text = post_json(&self.http, &self.endpoint, &self.api_key, &body).await?;
        parse_decisions_response(&text, &self.model)
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

// ---------------------------------------------------------------------------
// Chat + logprobs
// ---------------------------------------------------------------------------

/// System instruction for [`ChatLogprobScorer`]: the same [`RISK_LEVELS`],
/// numbered, and a one-digit answer.
pub fn chat_logprob_instruction() -> String {
    let mut s = String::from(
        "You rate how dangerous it is to run a shell command on the described target.\n\
         Danger levels, from safest to most dangerous:\n",
    );
    for (i, level) in RISK_LEVELS.iter().enumerate() {
        s.push_str(&format!("{i}: {level}\n"));
    }
    s.push_str(&format!(
        "Answer with exactly one digit from 0 to {} — the level of the command. No other text.",
        RISK_LEVELS.len() - 1
    ));
    s
}

/// Chat-completions request body for [`ChatLogprobScorer`].
pub fn build_chat_logprob_request(model: &str, input: &RiskInput) -> serde_json::Value {
    serde_json::json!({
        "model": model,
        "messages": [
            { "role": "system", "content": chat_logprob_instruction() },
            { "role": "user", "content": input.state_text() },
        ],
        "max_tokens": 1,
        "temperature": 0,
        "logprobs": true,
        "top_logprobs": RISK_LEVELS.len().max(5),
        "stream": false,
    })
}

#[derive(Debug, Deserialize)]
struct ChatLogprobResponse {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    choices: Vec<ChatLogprobChoice>,
    #[serde(default)]
    usage: Option<DecisionsUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatLogprobChoice {
    #[serde(default)]
    logprobs: Option<ChatLogprobs>,
}

#[derive(Debug, Deserialize)]
struct ChatLogprobs {
    #[serde(default)]
    content: Option<Vec<ChatTokenLogprob>>,
}

#[derive(Debug, Deserialize)]
struct ChatTokenLogprob {
    #[serde(default)]
    token: String,
    #[serde(default)]
    logprob: Option<f64>,
    #[serde(default)]
    top_logprobs: Vec<ChatTopLogprob>,
}

#[derive(Debug, Deserialize)]
struct ChatTopLogprob {
    token: String,
    logprob: f64,
}

/// The level a token stands for: a single digit `0..N`, surrounding
/// whitespace ignored (tokenizers often emit `" 2"`).
fn token_level(token: &str) -> Option<usize> {
    let t = token.trim();
    if t.len() != 1 {
        return None;
    }
    t.parse::<usize>().ok().filter(|i| *i < RISK_LEVELS.len())
}

/// Parse a chat-completions response with `logprobs` into a [`RiskScore`].
///
/// The distribution is taken from `top_logprobs` of the first generated
/// token: probabilities of the tokens that name a level are summed per level
/// and normalised. A response without logprobs is an error — the generated
/// digit alone is not a rating.
pub fn parse_chat_logprob_response(body: &str, requested_model: &str) -> Result<RiskScore> {
    let resp: ChatLogprobResponse = serde_json::from_str(body)
        .map_err(|e| CoreError::Other(format!("risk scorer: unexpected response: {e}")))?;
    let first = resp
        .choices
        .first()
        .and_then(|c| c.logprobs.as_ref())
        .and_then(|l| l.content.as_ref())
        .and_then(|c| c.first())
        .ok_or_else(|| CoreError::Other("risk scorer: server does not return logprobs".into()))?;

    let mut mass: BTreeMap<usize, f64> = BTreeMap::new();
    let mut add = |token: &str, logprob: f64| {
        if let Some(level) = token_level(token) {
            if logprob.is_finite() {
                *mass.entry(level).or_insert(0.0) += logprob.exp();
            }
        }
    };
    if first.top_logprobs.is_empty() {
        // Some servers return only the chosen token's logprob.
        if let Some(lp) = first.logprob {
            add(&first.token, lp);
        }
    } else {
        for t in &first.top_logprobs {
            add(&t.token, t.logprob);
        }
    }
    let total: f64 = mass.values().sum();
    if total <= 0.0 || !total.is_finite() {
        return Err(CoreError::Other(
            "risk scorer: no level digit among the returned logprobs".into(),
        ));
    }
    let probabilities: BTreeMap<usize, f32> = mass
        .iter()
        .map(|(i, p)| (*i, (*p / total) as f32))
        .collect();
    let position = expected_position(&probabilities)
        .ok_or_else(|| CoreError::Other("risk scorer: empty distribution".into()))?;
    let confidence = probabilities.values().cloned().fold(None, |m: Option<f32>, p| {
        Some(m.map_or(p, |m| m.max(p)))
    });
    Ok(RiskScore {
        safety_pct: safety_pct_from_position(position, RISK_LEVELS.len()),
        confidence,
        probabilities,
        cost_usd: resp.usage.and_then(|u| u.cost),
        model: resp.model.unwrap_or_else(|| requested_model.to_string()),
    })
}

/// [`RiskScorer`] over an OpenAI-compatible chat-completions endpoint that
/// returns `logprobs` — e.g. a local server, or a provider without the
/// Decisions API.
pub struct ChatLogprobScorer {
    http: reqwest::Client,
    endpoint: String,
    model: String,
    api_key: String,
}

impl ChatLogprobScorer {
    /// Build a scorer for `base_url` / `model` with an already resolved key
    /// (empty = send no `Authorization` header).
    pub fn new(base_url: &str, model: &str, api_key: &str, timeout: Duration) -> Result<Self> {
        Ok(Self {
            http: scorer_http_client(api_key, timeout)?,
            endpoint: format!("{}/chat/completions", base_url.trim().trim_end_matches('/')),
            model: model.trim().to_string(),
            api_key: api_key.to_string(),
        })
    }
}

#[async_trait]
impl RiskScorer for ChatLogprobScorer {
    async fn score(&self, input: &RiskInput) -> Result<RiskScore> {
        let body = build_chat_logprob_request(&self.model, input);
        let text = post_json(&self.http, &self.endpoint, &self.api_key, &body).await?;
        parse_chat_logprob_response(&text, &self.model)
    }

    fn model_name(&self) -> &str {
        &self.model
    }
}

/// HTTP client shared by the scorers: total timeout; keyless (local)
/// servers get no redirects, so the command text cannot be forwarded to
/// another host — same policy as the LLM client.
fn scorer_http_client(api_key: &str, timeout: Duration) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder().timeout(timeout);
    if api_key.is_empty() {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    builder
        .build()
        .map_err(|e| CoreError::Other(format!("failed to build HTTP client: {e}")))
}

/// POST `body` as JSON and return the response text of a 2xx answer. Errors
/// carry neither the key nor the URL.
async fn post_json(
    http: &reqwest::Client,
    endpoint: &str,
    api_key: &str,
    body: &serde_json::Value,
) -> Result<String> {
    let mut req = http.post(endpoint).json(body);
    if !api_key.is_empty() {
        req = req.bearer_auth(api_key);
    }
    let resp = req
        .send()
        .await
        .map_err(|e| CoreError::Other(format!("risk scorer request failed: {}", e.without_url())))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| CoreError::Other(format!("risk scorer: body unreadable: {}", e.without_url())))?;
    if !status.is_success() {
        return Err(CoreError::Other(format!("risk scorer: HTTP {}", status.as_u16())));
    }
    Ok(text)
}

/// Build the scorer described by `[risk_scorer]`, resolving its key from
/// `secrets` (environment / OS keyring) under `key_env`.
///
/// An empty `key_env` means the server needs no key. A non-empty one whose
/// secret is missing is an error — the message names the variable only.
pub fn build_risk_scorer(
    cfg: &RiskScorerConfig,
    secrets: &dyn SecretProvider,
) -> Result<std::sync::Arc<dyn RiskScorer>> {
    let kind = cfg.validate()?;
    let key = if cfg.key_env.trim().is_empty() {
        String::new()
    } else {
        secrets.get(cfg.key_env.trim()).map_err(|_| {
            CoreError::Secret(format!("risk scorer key {} is not set", cfg.key_env.trim()))
        })?
    };
    let timeout = Duration::from_secs(cfg.timeout_secs);
    match kind {
        RiskScorerKind::Decisions => Ok(std::sync::Arc::new(DecisionsApiScorer::new(
            &cfg.base_url,
            &cfg.model,
            &key,
            timeout,
        )?)),
        RiskScorerKind::ChatLogprobs => Ok(std::sync::Arc::new(ChatLogprobScorer::new(
            &cfg.base_url,
            &cfg.model,
            &key,
            timeout,
        )?)),
        _ => Err(CoreError::Config("[risk_scorer] unsupported kind".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const SECRET: &str = "sk-test-SECRET-value";

    fn input() -> RiskInput {
        RiskInput {
            command: "rm -rf /tmp/x".into(),
            explanation: "remove the scratch dir".into(),
            target: "SSH remote host (root@web1)".into(),
        }
    }

    #[test]
    fn pct_from_position() {
        assert_eq!(safety_pct_from_position(0.0, 5), 100);
        assert_eq!(safety_pct_from_position(4.0, 5), 0);
        assert_eq!(safety_pct_from_position(2.0, 5), 50);
        assert_eq!(safety_pct_from_position(1.0, 5), 75);
        // Clamped.
        assert_eq!(safety_pct_from_position(-3.0, 5), 100);
        assert_eq!(safety_pct_from_position(9.0, 5), 0);
        assert_eq!(safety_pct_from_position(f64::NAN, 5), 0);
        assert_eq!(safety_pct_from_position(0.0, 1), 0);
    }

    #[test]
    fn expected_position_weights_levels() {
        let p: BTreeMap<usize, f32> = [(0, 0.5), (4, 0.5)].into_iter().collect();
        assert_eq!(expected_position(&p), Some(2.0));
        assert_eq!(expected_position(&BTreeMap::new()), None);
    }

    #[test]
    fn request_has_model_state_and_five_level_score_question() {
        let body = build_decisions_request("typesafe/jev-1.14", &input());
        assert_eq!(body["model"], "typesafe/jev-1.14");
        let state = body["state"].as_str().unwrap();
        assert!(state.contains("rm -rf /tmp/x"));
        assert!(state.contains("root@web1"));
        assert!(state.contains("remove the scratch dir"));
        let q = &body["questions"]["risk"];
        assert_eq!(q["type"], "score");
        assert_eq!(q["criteria"].as_array().unwrap().len(), 5);
        assert!(q["instructions"].as_str().unwrap().contains("dangerous"));
    }

    #[test]
    fn kubernetes_target_reaches_the_scorer_state() {
        let input = RiskInput {
            command: "k8s_describe Pod shop/web-1".into(),
            explanation: "Inspect termination reason and memory limits".into(),
            target: "Kubernetes cluster prod-eu (namespace shop, tags: prod)".into(),
        };
        let state = input.state_text();
        assert!(state.contains("Target: Kubernetes cluster prod-eu (namespace shop, tags: prod)"));
        assert!(state.contains("k8s_describe Pod shop/web-1"));
    }

    #[test]
    fn state_is_capped() {
        let big = RiskInput {
            command: "x".repeat(MAX_STATE_CHARS * 2),
            ..input()
        };
        assert!(big.state_text().chars().count() <= MAX_STATE_CHARS);
    }

    #[test]
    fn parse_score_answer() {
        let body = r#"{"model":"typesafe/jev-1.14","answers":{"risk":{"type":"score","score":1.0,
            "confidence":0.8,"probabilities":{"0":0.1,"1":0.8,"2":0.1}}},
            "usage":{"input_tokens":120,"output_tokens":0,"cost":0.00002}}"#;
        let s = parse_decisions_response(body, "m").unwrap();
        assert_eq!(s.safety_pct, 75);
        assert_eq!(s.confidence, Some(0.8));
        assert_eq!(s.probabilities.len(), 3);
        assert_eq!(s.cost_usd, Some(0.00002));
        assert_eq!(s.model, "typesafe/jev-1.14");
    }

    #[test]
    fn parse_falls_back_to_probabilities_and_clamps() {
        let body = r#"{"answers":{"risk":{"type":"score","probabilities":{"4":1.0}}},
            "usage":{"input_tokens":1,"output_tokens":0}}"#;
        let s = parse_decisions_response(body, "requested").unwrap();
        assert_eq!(s.safety_pct, 0);
        assert_eq!(s.model, "requested");
        let body = r#"{"answers":{"risk":{"type":"score","score":17.0}},"usage":{"input_tokens":1,"output_tokens":0}}"#;
        assert_eq!(parse_decisions_response(body, "m").unwrap().safety_pct, 0);
    }

    #[test]
    fn parse_rejects_bad_bodies() {
        assert!(parse_decisions_response("not json", "m").is_err());
        assert!(parse_decisions_response(r#"{"answers":{}}"#, "m").is_err());
        assert!(parse_decisions_response(r#"{"answers":{"risk":{"type":"score"}}}"#, "m").is_err());
    }

    /// One-shot HTTP server: answers `status` + `body` and records the request.
    async fn serve_once(status: u16, body: &'static str, delay: Duration) -> (String, Arc<Mutex<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(String::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            let Ok((mut socket, _)) = listener.accept().await else { return };
            let mut buf = Vec::new();
            let mut tmp = [0u8; 4096];
            // Read headers, then Content-Length bytes of body.
            loop {
                let Ok(n) = socket.read(&mut tmp).await else { return };
                if n == 0 { break; }
                buf.extend_from_slice(&tmp[..n]);
                let text = String::from_utf8_lossy(&buf).to_string();
                if let Some(end) = text.find("\r\n\r\n") {
                    let len = text
                        .lines()
                        .find_map(|l| {
                            let l = l.to_ascii_lowercase();
                            l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len { break; }
                }
            }
            *seen2.lock().unwrap() = String::from_utf8_lossy(&buf).to_string();
            tokio::time::sleep(delay).await;
            let resp = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(resp.as_bytes()).await;
            let _ = socket.flush().await;
        });
        (format!("http://{addr}/api/alpha"), seen)
    }

    const OK_BODY: &str = r#"{"model":"typesafe/jev-1.14","answers":{"risk":{"type":"score","score":3.0,"confidence":0.9}},"usage":{"input_tokens":50,"output_tokens":0,"cost":0.0001}}"#;

    #[tokio::test]
    async fn scorer_posts_to_decisions_with_bearer() {
        let (base, seen) = serve_once(200, OK_BODY, Duration::ZERO).await;
        let s = DecisionsApiScorer::new(&base, "typesafe/jev-1.14", SECRET, Duration::from_secs(5)).unwrap();
        let r = s.score(&input()).await.unwrap();
        assert_eq!(r.safety_pct, 25);
        let req = seen.lock().unwrap().clone();
        assert!(req.starts_with("POST /api/alpha/decisions "), "{req}");
        assert!(req.to_ascii_lowercase().contains(&format!("authorization: bearer {}", SECRET.to_ascii_lowercase())));
        assert!(req.contains("\"type\":\"score\""));
    }

    #[tokio::test]
    async fn keyless_scorer_sends_no_authorization() {
        let (base, seen) = serve_once(200, OK_BODY, Duration::ZERO).await;
        let s = DecisionsApiScorer::new(&format!("{base}/"), "local", "", Duration::from_secs(5)).unwrap();
        s.score(&input()).await.unwrap();
        let req = seen.lock().unwrap().to_ascii_lowercase();
        assert!(req.starts_with("post /api/alpha/decisions "), "{req}");
        assert!(!req.contains("authorization:"));
    }

    #[tokio::test]
    async fn http_errors_are_errors_without_the_key() {
        for status in [401u16, 429, 500, 503] {
            let (base, _) = serve_once(status, r#"{"error":{"code":1,"message":"x"}}"#, Duration::ZERO).await;
            let s = DecisionsApiScorer::new(&base, "m", SECRET, Duration::from_secs(5)).unwrap();
            let err = s.score(&input()).await.unwrap_err().to_string();
            assert!(err.contains(&status.to_string()), "{err}");
            assert!(!err.contains(SECRET), "{err}");
        }
    }

    #[tokio::test]
    async fn timeout_and_bad_json_are_errors() {
        let (base, _) = serve_once(200, OK_BODY, Duration::from_secs(3)).await;
        let s = DecisionsApiScorer::new(&base, "m", SECRET, Duration::from_millis(300)).unwrap();
        let err = s.score(&input()).await.unwrap_err().to_string();
        assert!(!err.contains(SECRET), "{err}");

        let (base, _) = serve_once(200, "{oops", Duration::ZERO).await;
        let s = DecisionsApiScorer::new(&base, "m", SECRET, Duration::from_secs(5)).unwrap();
        assert!(s.score(&input()).await.is_err());
    }

    #[tokio::test]
    async fn connection_refused_error_has_no_key() {
        // Bind and drop to get a closed port.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let s = DecisionsApiScorer::new(&format!("http://{addr}"), "m", SECRET, Duration::from_secs(2)).unwrap();
        let err = s.score(&input()).await.unwrap_err().to_string();
        assert!(!err.contains(SECRET), "{err}");
    }

    #[test]
    fn build_resolves_key_and_reports_missing_one_by_name() {
        let cfg = RiskScorerConfig {
            kind: "decisions".into(),
            base_url: "https://openrouter.ai/api/alpha".into(),
            model: "typesafe/jev-1.14".into(),
            key_env: "MY_RISK_KEY".into(),
            timeout_secs: 8,
            ..Default::default()
        };
        let empty = filar_core::secrets::StaticSecretProvider::new();
        let err = build_risk_scorer(&cfg, &empty).err().unwrap().to_string();
        assert!(err.contains("MY_RISK_KEY"), "{err}");

        let with_key = filar_core::secrets::StaticSecretProvider::new();
        with_key.insert("MY_RISK_KEY", SECRET);
        let s = build_risk_scorer(&cfg, &with_key).unwrap();
        assert_eq!(s.model_name(), "typesafe/jev-1.14");

        let keyless = RiskScorerConfig { key_env: String::new(), ..cfg.clone() };
        assert!(build_risk_scorer(&keyless, &empty).is_ok());

        let bad = RiskScorerConfig { kind: "magic".into(), ..cfg };
        assert!(build_risk_scorer(&bad, &with_key).is_err());
    }

    // ── chat + logprobs (#550) ──────────────────────────────────────

    #[test]
    fn chat_request_asks_for_one_token_with_logprobs() {
        let body = build_chat_logprob_request("qwen", &input());
        assert_eq!(body["model"], "qwen");
        assert_eq!(body["max_tokens"], 1);
        assert_eq!(body["temperature"], 0);
        assert_eq!(body["logprobs"], true);
        assert!(body["top_logprobs"].as_u64().unwrap() >= RISK_LEVELS.len() as u64);
        let system = body["messages"][0]["content"].as_str().unwrap();
        for (i, level) in RISK_LEVELS.iter().enumerate() {
            assert!(system.contains(&format!("{i}: {level}")), "{system}");
        }
        assert!(system.contains("exactly one digit"));
        assert!(body["messages"][1]["content"].as_str().unwrap().contains("rm -rf /tmp/x"));
    }

    #[test]
    fn chat_distribution_from_top_logprobs() {
        // "3" and " 3" both name level 3; "Sure" is ignored; 0.5/0.25/0.25 overall.
        let body = format!(
            r#"{{"model":"qwen","choices":[{{"logprobs":{{"content":[{{"token":"3","logprob":{l3a},
            "top_logprobs":[{{"token":"3","logprob":{l3a}}},{{"token":" 3","logprob":{l3b}}},
            {{"token":"1","logprob":{l1}}},{{"token":"Sure","logprob":{lx}}}]}}]}}}}],
            "usage":{{"prompt_tokens":10,"completion_tokens":1}}}}"#,
            l3a = (0.25f64).ln(), l3b = (0.25f64).ln(), l1 = (0.5f64).ln(), lx = (0.1f64).ln()
        );
        let s = parse_chat_logprob_response(&body, "m").unwrap();
        // Expected position: 0.5·3 + 0.5·1 = 2 → 50%.
        assert_eq!(s.safety_pct, 50);
        assert_eq!(s.probabilities.len(), 2);
        assert!((s.confidence.unwrap() - 0.5).abs() < 1e-4);
        assert_eq!(s.model, "qwen");
    }

    #[test]
    fn chat_without_logprobs_is_unavailable() {
        let body = r#"{"choices":[{"message":{"content":"2"}}]}"#;
        let err = parse_chat_logprob_response(body, "m").unwrap_err().to_string();
        assert!(err.contains("does not return logprobs"), "{err}");
        let body = r#"{"choices":[{"logprobs":null}]}"#;
        assert!(parse_chat_logprob_response(body, "m").is_err());
    }

    #[test]
    fn chat_only_chosen_token_logprob_still_rates() {
        let body = r#"{"choices":[{"logprobs":{"content":[{"token":"0","logprob":-0.01,"top_logprobs":[]}]}}]}"#;
        let s = parse_chat_logprob_response(body, "m").unwrap();
        assert_eq!(s.safety_pct, 100);
    }

    #[test]
    fn chat_with_no_level_digit_is_an_error() {
        let body = r#"{"choices":[{"logprobs":{"content":[{"token":"I","logprob":-0.1,
            "top_logprobs":[{"token":"I","logprob":-0.1},{"token":"7","logprob":-3.0},{"token":"12","logprob":-4.0}]}]}}]}"#;
        assert!(parse_chat_logprob_response(body, "m").is_err());
    }

    const CHAT_OK: &str = r#"{"model":"local","choices":[{"logprobs":{"content":[{"token":"4","logprob":0.0,"top_logprobs":[{"token":"4","logprob":0.0}]}]}}]}"#;

    #[tokio::test]
    async fn chat_scorer_posts_to_chat_completions() {
        let (base, seen) = serve_once(200, CHAT_OK, Duration::ZERO).await;
        let s = ChatLogprobScorer::new(&base, "local", "", Duration::from_secs(5)).unwrap();
        let r = s.score(&input()).await.unwrap();
        assert_eq!(r.safety_pct, 0);
        let req = seen.lock().unwrap().to_ascii_lowercase();
        assert!(req.starts_with("post /api/alpha/chat/completions "), "{req}");
        assert!(!req.contains("authorization:"), "keyless sends no key");
        assert!(req.contains("\"logprobs\":true"));
    }

    #[tokio::test]
    async fn chat_scorer_errors_carry_no_key() {
        let (base, seen) = serve_once(500, "{}", Duration::ZERO).await;
        let s = ChatLogprobScorer::new(&base, "m", SECRET, Duration::from_secs(5)).unwrap();
        let err = s.score(&input()).await.unwrap_err().to_string();
        assert!(err.contains("500") && !err.contains(SECRET), "{err}");
        assert!(seen.lock().unwrap().to_ascii_lowercase().contains("authorization: bearer"));
    }

    #[test]
    fn build_picks_the_chat_adapter() {
        let cfg = RiskScorerConfig {
            kind: "chat-logprobs".into(),
            base_url: "http://127.0.0.1:8080/v1".into(),
            model: "qwen".into(),
            key_env: String::new(),
            timeout_secs: 8,
            ..Default::default()
        };
        let s = build_risk_scorer(&cfg, &filar_core::secrets::StaticSecretProvider::new()).unwrap();
        assert_eq!(s.model_name(), "qwen");
    }
}
