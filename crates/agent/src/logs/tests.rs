//! Tests of the log-source frame (#579): byte caps on a streamed answer,
//! cancellation, key hygiene, folding, the tool's frame and gate.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::TimeZone;
use filar_core::{CommandConfirmMode, LogSourceConfig, StaticSecretProvider, TagPolicy};
use serde_json::json;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::*;
use crate::security::{CommandConfirmer, ConfirmDecision};
use crate::{AgentEvent, ToolCall};

fn vlogs(name: &str) -> LogSourceConfig {
    LogSourceConfig {
        name: name.into(),
        kind: "victorialogs".into(),
        url: "https://vlogs.example.com".into(),
        ..Default::default()
    }
}

/// Read one HTTP request head from `socket`.
async fn read_head(socket: &mut tokio::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut buf = [0u8; 1024];
    while !head.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = socket.read(&mut buf).await.unwrap();
        if n == 0 {
            break;
        }
        head.extend_from_slice(&buf[..n]);
    }
    String::from_utf8_lossy(&head).into_owned()
}

// ---------------------------------------------------------------------------
// Line and answer caps
// ---------------------------------------------------------------------------

#[test]
fn a_long_line_is_cut_on_a_char_boundary_and_marked() {
    let (short, cut) = cap_line("hello", 4096);
    assert_eq!((short.as_str(), cut), ("hello", false));
    let long = "я".repeat(3000); // 6000 bytes
    let (capped, cut) = cap_line(&long, 4096);
    assert!(cut);
    assert!(capped.starts_with(&"я".repeat(2048)));
    assert!(capped.ends_with("[line cut: 6000 bytes]"), "{capped}");
}

#[tokio::test]
async fn a_huge_answer_is_read_only_up_to_the_cap() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let sent_all = Arc::new(AtomicBool::new(false));
    let flag = sent_all.clone();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_head(&mut socket).await;
        // Megabyte-long lines, 256 MiB in total — far more than socket
        // buffers can absorb once the client stops reading.
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 268435456\r\n\r\n")
            .await
            .unwrap();
        let line = vec![b'x'; 1024 * 1024];
        for _ in 0..256 {
            if socket.write_all(&line).await.is_err() {
                return;
            }
        }
        flag.store(true, Ordering::SeqCst);
    });
    let response = reqwest::get(format!("http://{addr}/")).await.unwrap();
    let body = read_limited(response, 2 * 1024 * 1024).await.unwrap();
    assert!(body.truncated);
    assert_eq!(body.bytes.len(), 2 * 1024 * 1024);
    let text = String::from_utf8(body.bytes).unwrap();
    let (line, cut) = cap_line(&text[..1024 * 1024], 4096);
    assert!(cut && line.contains("line cut: 1048576 bytes"));
    // The client stopped reading: the server could not push the whole body
    // through a closed connection.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    assert!(!sent_all.load(Ordering::SeqCst), "the whole body was downloaded");
}

#[tokio::test]
async fn cancelling_a_query_closes_the_connection() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_head(&mut socket).await;
        socket
            .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nfirst\r\n")
            .await
            .unwrap();
        // A slow query: nothing more until the client goes away.
        let mut buf = [0u8; 64];
        let closed = matches!(socket.read(&mut buf).await, Ok(0) | Err(_));
        let _ = closed_tx.send(closed);
    });
    let cfg = LogSourceConfig {
        url: format!("http://{addr}"),
        ..vlogs("slow")
    };
    let http = LogHttp::new(&cfg, "").unwrap();
    let token = tokio_util::sync::CancellationToken::new();
    let run = async {
        let response = http.send(http.request(reqwest::Method::POST, "/select/logsql/query")).await?;
        read_limited(response, 1 << 20).await
    };
    let canceller = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        canceller.cancel();
    });
    tokio::select! {
        _ = token.cancelled() => {}
        _ = run => panic!("the query finished although the server never ended it"),
    }
    let closed = tokio::time::timeout(std::time::Duration::from_secs(5), closed_rx)
        .await
        .expect("the server never saw the connection close")
        .unwrap();
    assert!(closed);
}

// ---------------------------------------------------------------------------
// Key hygiene
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_key_never_appears_in_an_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for _ in 0..2 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let head = read_head(&mut socket).await;
            // A careless proxy echoes the Authorization header back.
            let auth = head
                .lines()
                .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
                .unwrap_or("")
                .to_string();
            let body = format!("denied for {auth}");
            let reply = format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    for (auth, user) in [("bearer", None), ("basic", Some("reader"))] {
        let cfg = LogSourceConfig {
            url: format!("http://{addr}"),
            auth: auth.into(),
            key_env: "FILAR_LOGS_TEST".into(),
            user: user.map(String::from),
            ..vlogs("keyed")
        };
        let http = LogHttp::new(&cfg, "s3cr3t-token-value").unwrap();
        let err = http
            .send(http.request(reqwest::Method::GET, "/"))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("401"), "{err}");
        assert!(!err.contains("s3cr3t-token-value"), "{err}");
        assert!(!err.contains("cmVhZGVyOnMzY3IzdC10b2tlbi12YWx1ZQ"), "basic token leaked: {err}");
    }
}

#[test]
fn a_missing_key_is_named_by_variable_only() {
    let cfg = LogSourceConfig {
        auth: "bearer".into(),
        key_env: "FILAR_LOGS_NONE".into(),
        ..vlogs("v")
    };
    let err = resolve_log_key(&cfg, &StaticSecretProvider::new()).unwrap_err().to_string();
    assert!(err.contains("FILAR_LOGS_NONE"), "{err}");
    let secrets = StaticSecretProvider::new();
    secrets.insert("FILAR_LOGS_NONE", "k-123");
    assert_eq!(resolve_log_key(&cfg, &secrets).unwrap(), "k-123");
    assert_eq!(resolve_log_key(&vlogs("open"), &StaticSecretProvider::new()).unwrap(), "");
}

#[test]
fn a_source_that_cannot_be_built_is_reported_not_offered() {
    let os = LogSourceConfig {
        kind: "opensearch".into(),
        url: "https://os:9200".into(),
        index: Some("logs-*".into()),
        ca_file: Some("/nonexistent/filar-ca.pem".into()),
        ..vlogs("o")
    };
    let (set, notes) = build_log_sources(&[os], Arc::new(StaticSecretProvider::new()), Vec::new());
    assert!(set.is_none());
    assert_eq!(notes.len(), 1);
    assert!(notes[0].contains("cannot read ca_file"), "{}", notes[0]);
}

#[test]
fn victorialogs_sources_are_built() {
    let (set, notes) = build_log_sources(&[vlogs("v")], Arc::new(StaticSecretProvider::new()), Vec::new());
    assert!(notes.is_empty(), "{notes:?}");
    assert_eq!(set.unwrap().names(), vec!["v".to_string()]);
}

// ---------------------------------------------------------------------------
// Folding
// ---------------------------------------------------------------------------

#[test]
fn normalisation_replaces_numbers_ids_and_times() {
    assert_eq!(
        normalize("2026-11-01T10:00:00.123Z request 7f3c9a2e-1b4d-4c8e-9f00-123456789abc took 154ms from 10.0.0.12:8080"),
        "<time> request <uuid> took <n>ms from <n>:<n>"
    );
    assert_eq!(normalize("trace=4bf92f3577b34da6a3ce929d0e0e4736 span 0x1f"), "trace=<hex> span <hex>");
    assert_eq!(normalize("db-17 down"), "db-<n> down");
    assert_eq!(normalize("deadbeef cafe"), "deadbeef cafe", "hex without digits is a word");
}

fn normalize(s: &str) -> String {
    super::fold::normalize_message(s)
}

fn line(minute: u32, message: &str) -> LogLine {
    LogLine {
        time: Some(Utc.with_ymd_and_hms(2026, 11, 1, 10, minute, 0).unwrap()),
        message: message.into(),
        ..Default::default()
    }
}

fn hour_query(limit: usize) -> LogQuery {
    LogQuery {
        query: "error".into(),
        start: Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
        end: Utc.with_ymd_and_hms(2026, 11, 1, 11, 0, 0).unwrap(),
        limit,
    }
}

#[test]
fn folding_groups_similar_messages_with_counts_and_bounds() {
    let mut lines: Vec<LogLine> = (0..30)
        .map(|i| line(i, &format!("connection refused to db-{}:5432 after {}ms", i % 3, i * 10)))
        .collect();
    lines.push(line(45, "OOMKilled: worker 12"));
    let result = LogResult {
        lines,
        truncated: true,
        ..Default::default()
    };
    let text = fold_logs("vlogs", "LogsQL", &hour_query(31), &result);
    assert!(text.contains("31 line(s), limit 31"), "{text}");
    assert!(text.contains("TRUNCATED"), "{text}");
    assert!(text.contains("Groups: 2 distinct"), "{text}");
    assert!(text.contains("[30×] 10:00:00 … 10:29:00  first: connection refused to db-0:5432 after 0ms"), "{text}");
    assert!(text.contains("last: connection refused to db-2:5432 after 290ms"), "{text}");
    assert!(text.contains("[1×] 10:45:00  OOMKilled: worker 12"), "{text}");
    // 12 buckets of 5 minutes: the first six hold 5 lines each.
    assert!(text.contains("Distribution: 10:00 5 · 10:05 5"), "{text}");
    assert!(text.contains("10:45 1"), "{text}");
}

#[test]
fn an_empty_answer_says_so() {
    let text = fold_logs("v", "LogsQL", &hour_query(10), &LogResult::default());
    assert!(text.contains("No lines matched"), "{text}");
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

/// A source answering with fixed lines and recording the queries it got.
struct MockSource {
    cfg: LogSourceConfig,
    lines: Vec<LogLine>,
    seen: Mutex<Vec<LogQuery>>,
}

#[async_trait]
impl LogSource for MockSource {
    fn config(&self) -> &LogSourceConfig {
        &self.cfg
    }
    fn describe(&self) -> String {
        "LogsQL: word filters, field:value, | stats count() by (...)".into()
    }
    async fn query(&self, query: &LogQuery) -> filar_core::Result<LogResult> {
        self.seen.lock().unwrap().push(query.clone());
        Ok(LogResult {
            lines: self.lines.clone(),
            ..Default::default()
        })
    }
}

fn sources(lines: Vec<LogLine>, tags: Vec<String>, policies: Vec<TagPolicy>) -> (LogSources, Arc<MockSource>, StaticSecretProvider) {
    let mock = Arc::new(MockSource {
        cfg: LogSourceConfig {
            max_window_hours: 6,
            max_lines: 300,
            clusters: vec!["prod-eu".into()],
            tags,
            ..vlogs("vlogs-prod")
        },
        lines,
        seen: Mutex::new(Vec::new()),
    });
    let secrets = StaticSecretProvider::new();
    let set = LogSources::new(vec![mock.clone() as Arc<dyn LogSource>], Arc::new(secrets.clone()), policies);
    (set, mock, secrets)
}

fn now() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 11, 1, 12, 0, 0).unwrap()
}

#[test]
fn the_frame_requires_a_window_within_the_cap_and_caps_the_limit() {
    let (set, _, _) = sources(Vec::new(), Vec::new(), Vec::new());
    let err = |args| set.parse_call(&args, now()).unwrap_err().to_string();
    assert!(err(json!({"source": "vlogs-prod", "query": "error"})).contains("window is required"));
    assert!(err(json!({"source": "vlogs-prod", "query": "error", "since": "7h"})).contains("longer than this source allows (6 h)"));
    assert!(err(json!({"source": "other", "query": "x", "since": "1h"})).contains("available: vlogs-prod"));
    assert!(err(json!({"source": "vlogs-prod", "query": "x", "since": "1h", "start": "2026-11-01T10:00:00Z"})).contains("not both"));
    assert!(err(json!({"source": "vlogs-prod", "query": "x", "start": "2026-11-01T13:00:00Z"})).contains("empty"));

    let call = set
        .parse_call(&json!({"source": "vlogs-prod", "query": "error", "since": "30m", "limit": 5000}), now())
        .unwrap();
    assert_eq!(call.query.limit, 300);
    assert_eq!(call.limit_capped_from, Some(5000));
    assert_eq!(call.query.end - call.query.start, chrono::Duration::minutes(30));

    let call = set
        .parse_call(&json!({"source": "vlogs-prod", "query": "error", "start": "2026-11-01T09:00:00+02:00"}), now())
        .unwrap();
    assert_eq!(call.query.start, Utc.with_ymd_and_hms(2026, 11, 1, 7, 0, 0).unwrap());
    assert_eq!(call.query.end, now());
    assert_eq!(call.query.limit, 200, "default limit");
    assert_eq!(
        call.display_command(),
        "query_logs vlogs-prod --start 2026-11-01T07:00:00Z --end 2026-11-01T12:00:00Z --limit 200: error"
    );
}

#[test]
fn a_huge_since_is_an_error_not_a_panic() {
    let (set, _, _) = sources(Vec::new(), Vec::new(), Vec::new());
    let err = set
        .parse_call(&json!({"source": "vlogs-prod", "query": "x", "since": "9000000000000s"}), now())
        .unwrap_err()
        .to_string();
    assert!(err.contains("too large") || err.contains("longer"), "{err}");
}

/// A source whose query fails with a server reply echoing a secret.
struct FailingSource(LogSourceConfig);

#[async_trait]
impl LogSource for FailingSource {
    fn config(&self) -> &LogSourceConfig {
        &self.0
    }
    fn describe(&self) -> String {
        String::new()
    }
    async fn query(&self, _: &LogQuery) -> filar_core::Result<LogResult> {
        Err(filar_core::CoreError::Other("HTTP 502: upstream said token=other-secret-42".into()))
    }
}

#[tokio::test]
async fn an_error_reply_is_redacted_before_the_model() {
    let secrets = StaticSecretProvider::new();
    secrets.insert("SOME_OTHER_KEY", "other-secret-42");
    let set = LogSources::new(
        vec![Arc::new(FailingSource(vlogs("down"))) as Arc<dyn LogSource>],
        Arc::new(secrets),
        Vec::new(),
    );
    let call = ToolCall {
        id: "c".into(),
        name: TOOL_QUERY_LOGS.into(),
        arguments: json!({"source": "down", "query": "x", "since": "5m"}),
    };
    let emit = |_: AgentEvent| {};
    let msg = set
        .process(&call, CommandConfirmMode::Never, &Answer(true, Mutex::new(0)), &emit, None, 1)
        .await
        .unwrap();
    assert!(msg.content.contains("502"), "{}", msg.content);
    assert!(!msg.content.contains("other-secret-42"), "{}", msg.content);
}

#[test]
fn the_gate_asks_in_always_and_explain_and_tags_tighten() {
    let (set, _, _) = sources(Vec::new(), vec!["prod".into()], vec![TagPolicy {
        tag: "prod".into(),
        confirm_mode: CommandConfirmMode::Always,
    }]);
    let call = set.parse_call(&json!({"source": "vlogs-prod", "query": "x", "since": "1h"}), now()).unwrap();
    assert_eq!(set.decision(CommandConfirmMode::Never, &call), ConfirmDecision::NeedsConfirmation);
    let (open, _, _) = sources(Vec::new(), Vec::new(), Vec::new());
    assert_eq!(open.decision(CommandConfirmMode::Never, &call), ConfirmDecision::AutoApproved);
    assert_eq!(open.decision(CommandConfirmMode::Allowlist, &call), ConfirmDecision::AutoApproved);
    assert_eq!(open.decision(CommandConfirmMode::Always, &call), ConfirmDecision::NeedsConfirmation);
    assert_eq!(open.decision(CommandConfirmMode::Explain, &call), ConfirmDecision::NeedsConfirmation);
}

#[tokio::test]
async fn the_answer_is_redacted_before_the_model() {
    let (set, _, secrets) = sources(
        vec![
            line(10, "auth header Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.payload.sig"),
            line(11, "connecting to postgres://app:hunter2@db:5432/shop"),
            line(12, "api key is k-live-123456"),
        ],
        Vec::new(),
        Vec::new(),
    );
    secrets.insert("FILAR_LOGS_VLOGS_PROD", "k-live-123456");
    let call = set
        .parse_call(&json!({"source": "vlogs-prod", "query": "x", "start": "2026-11-01T10:00:00Z", "end": "2026-11-01T11:00:00Z"}), now())
        .unwrap();
    let text = set.execute(&call).await.unwrap();
    assert!(!text.contains("eyJhbGciOiJIUzI1NiJ9"), "{text}");
    assert!(!text.contains("hunter2"), "{text}");
    assert!(!text.contains("k-live-123456"), "{text}");
}

#[test]
fn the_prompt_lists_sources_with_language_and_limits() {
    let (set, _, _) = sources(Vec::new(), Vec::new(), Vec::new());
    let prompt = set.prompt_section();
    assert!(prompt.contains(LOG_SOURCES_PROMPT_HEADER));
    assert!(prompt.contains("- vlogs-prod — victorialogs, language LogsQL; window ≤ 6 h, ≤ 300 lines; logs of clusters: prod-eu"), "{prompt}");
    assert!(prompt.contains("| stats count() by"), "adapter hint: {prompt}");
    assert!(prompt.contains("Always give a window"), "{prompt}");
}

#[test]
fn explanation_is_required_only_in_explain_mode() {
    let (set, _, _) = sources(Vec::new(), Vec::new(), Vec::new());
    let required = |mode| set.tool_definition(mode).parameters["required"].clone();
    assert_eq!(required(CommandConfirmMode::Always), json!(["source", "query"]));
    assert_eq!(required(CommandConfirmMode::Explain), json!(["source", "query", "explanation"]));
    assert!(tool::explanation_error(&json!({})).is_some());
    assert!(tool::explanation_error(&json!({"explanation": "why"})).is_none());
}

struct Answer(bool, Mutex<usize>);

#[async_trait]
impl CommandConfirmer for Answer {
    async fn confirm(&self, _: &str, _: &str, _: bool) -> filar_core::Result<bool> {
        *self.1.lock().unwrap() += 1;
        Ok(self.0)
    }
}

#[tokio::test]
async fn a_denied_query_is_never_sent_and_an_approved_one_runs_once() {
    let (set, mock, _) = sources(vec![line(10, "boom")], Vec::new(), Vec::new());
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let emit = move |e: AgentEvent| sink.lock().unwrap().push(e);
    let call = ToolCall {
        id: "c1".into(),
        name: TOOL_QUERY_LOGS.into(),
        arguments: json!({"source": "vlogs-prod", "query": "boom", "since": "1h"}),
    };
    let deny = Answer(false, Mutex::new(0));
    let msg = set.process(&call, CommandConfirmMode::Always, &deny, &emit, None, 1).await.unwrap();
    assert!(msg.content.contains("denied"), "{}", msg.content);
    assert!(mock.seen.lock().unwrap().is_empty());

    let allow = Answer(true, Mutex::new(0));
    let msg = set.process(&call, CommandConfirmMode::Always, &allow, &emit, None, 2).await.unwrap();
    assert!(msg.content.contains("[1×]"), "{}", msg.content);
    assert_eq!(mock.seen.lock().unwrap().len(), 1);
    assert_eq!(*allow.1.lock().unwrap(), 1);

    // Auto-approved in `never`: no question at all.
    let msg = set.process(&call, CommandConfirmMode::Never, &deny, &emit, None, 3).await.unwrap();
    assert!(msg.content.contains("boom"));
    assert_eq!(*deny.1.lock().unwrap(), 1, "never mode must not ask");

    let events = events.lock().unwrap();
    let finished = events.iter().filter(|e| matches!(e, AgentEvent::CommandFinished { .. })).count();
    assert_eq!(finished, 3);
}

/// One stub source `vlogs-prod` answering `lines` — for the agent loops' tests.
pub(crate) fn stub_log_sources(lines: Vec<LogLine>) -> Arc<LogSources> {
    Arc::new(sources(lines, Vec::new(), Vec::new()).0)
}

/// A line at 10:`minute` UTC on 2026-11-01.
pub(crate) fn stub_line(minute: u32, message: &str) -> LogLine {
    line(minute, message)
}
