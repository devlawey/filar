//! `query_logs` against a mock OpenSearch / Elasticsearch (#581).
//!
//! OpenSearch and Elasticsearch cannot be downloaded in the agent's
//! environment, so the server here answers with fixtures in the shape of an
//! OpenSearch 2.x `_search` reply (`tests/fixtures/opensearch`). A live
//! store is a manual check in `docs/SMOKE.md`.

use std::sync::{Arc, Mutex};

use filar_agent::build_log_sources;
use filar_core::{LogSourceConfig, StaticSecretProvider};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One received request: the request line, headers (lower case) and body.
#[derive(Debug, Clone, Default)]
struct Received {
    line: String,
    headers: String,
    body: String,
}

/// Serve `replies` in order, one per connection, recording each request.
async fn mock(replies: Vec<(&'static str, String)>) -> (String, Arc<Mutex<Vec<Received>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    tokio::spawn(async move {
        for (status, body) in replies {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 8192];
            let received = loop {
                let n = socket.read(&mut buf).await.unwrap();
                data.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&data).to_string();
                let Some(split) = text.find("\r\n\r\n") else { continue };
                let head = &text[..split];
                let len = head
                    .lines()
                    .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                    .unwrap_or(0);
                if data.len() >= split + 4 + len || n == 0 {
                    let mut lines = head.lines();
                    break Received {
                        line: lines.next().unwrap_or("").to_string(),
                        headers: lines.collect::<Vec<_>>().join("\n").to_ascii_lowercase(),
                        body: text[split + 4..].to_string(),
                    };
                }
            };
            record.lock().unwrap().push(received);
            let reply = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            socket.write_all(reply.as_bytes()).await.unwrap();
        }
    });
    (format!("http://{addr}"), seen)
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!("{}/tests/fixtures/opensearch/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
}

fn source(kind: &str, url: String, auth: &str) -> LogSourceConfig {
    LogSourceConfig {
        name: "os".into(),
        kind: kind.into(),
        url,
        auth: auth.into(),
        key_env: if auth == "none" { String::new() } else { "FILAR_LOGS_OS".into() },
        user: (auth == "basic").then(|| "reader".into()),
        index: Some("logs-*".into()),
        ..Default::default()
    }
}

fn secrets() -> Arc<StaticSecretProvider> {
    let s = StaticSecretProvider::new();
    s.insert("FILAR_LOGS_OS", "es-key-777");
    Arc::new(s)
}

#[tokio::test]
async fn opensearch_hits_are_found_folded_and_bounded() {
    let (url, seen) = mock(vec![("200 OK", fixture("search_hits.json"))]).await;
    let (sources, notes) = build_log_sources(&[source("opensearch", url, "basic")], secrets(), Vec::new());
    assert!(notes.is_empty(), "{notes:?}");
    let sources = sources.unwrap();
    let call = sources
        .parse_call(
            &serde_json::json!({"source": "os", "query": "kubernetes.namespace_name:shop AND ERROR", "start": "2026-11-01T10:00:00Z", "end": "2026-11-01T11:00:00Z", "limit": 50}),
            chrono::Utc::now(),
        )
        .unwrap();
    let text = sources.execute(&call).await.unwrap();

    let request = seen.lock().unwrap()[0].clone();
    assert_eq!(request.line, "POST /logs-*/_search HTTP/1.1");
    assert!(request.headers.contains("authorization: basic "), "{}", request.headers);
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["size"], 50);
    assert_eq!(body["query"]["bool"]["must"][0]["query_string"]["query"], "kubernetes.namespace_name:shop AND ERROR");
    assert_eq!(body["query"]["bool"]["filter"][0]["range"]["@timestamp"]["gte"], "2026-11-01T10:00:00.000Z");

    assert!(text.contains("3 line(s)"), "{text}");
    assert!(text.contains("214 documents match"), "{text}");
    assert!(text.contains("TRUNCATED"), "{text}");
    assert!(text.contains("[2×] 10:58:29 … 10:58:31  first: ERROR connection refused to db-1:5432 after 3009ms"), "{text}");
    assert!(text.contains("kubernetes.pod_name=web-7d9f6c-9q2mz"), "{text}");
    assert!(text.contains("[1×] 10:41:02  ERROR payment provider timeout"), "{text}");
}

#[tokio::test]
async fn elasticsearch_api_key_and_dsl() {
    let (url, seen) = mock(vec![("200 OK", fixture("search_hits.json"))]).await;
    let (sources, _) = build_log_sources(&[source("elasticsearch", url, "api-key")], secrets(), Vec::new());
    let sources = sources.unwrap();
    let call = sources
        .parse_call(
            &serde_json::json!({"source": "os", "query": "{\"match\": {\"kubernetes.container_name\": \"pay\"}}", "since": "1h"}),
            chrono::Utc::now(),
        )
        .unwrap();
    sources.execute(&call).await.unwrap();
    let request = seen.lock().unwrap()[0].clone();
    assert!(request.headers.contains("authorization: apikey es-key-777"), "{}", request.headers);
    let body: serde_json::Value = serde_json::from_str(&request.body).unwrap();
    assert_eq!(body["query"]["bool"]["must"][0], serde_json::json!({"match": {"kubernetes.container_name": "pay"}}));
}

#[tokio::test]
async fn store_errors_are_plain_and_keyless() {
    let (url, _) = mock(vec![
        ("400 Bad Request", fixture("error_parsing.json")),
        ("404 Not Found", fixture("error_index_not_found.json")),
        ("401 Unauthorized", r#"{"error":"Unauthorized: es-key-777 rejected"}"#.to_string()),
    ])
    .await;
    let (sources, _) = build_log_sources(&[source("elasticsearch", url, "api-key")], secrets(), Vec::new());
    let sources = sources.unwrap();
    let call = sources
        .parse_call(&serde_json::json!({"source": "os", "query": "x", "since": "1h"}), chrono::Utc::now())
        .unwrap();
    let mut errors = Vec::new();
    for _ in 0..3 {
        errors.push(sources.execute(&call).await.unwrap_err().to_string());
    }
    assert!(errors[0].contains("HTTP 400") && errors[0].contains("query_shard_exception: Failed to parse query"), "{}", errors[0]);
    assert!(errors[1].contains("index_not_found_exception: no such index [applogs-*]"), "{}", errors[1]);
    assert!(errors[2].contains("authentication failed (401)"), "{}", errors[2]);
    for e in &errors {
        assert!(!e.contains("es-key-777"), "{e}");
    }
}
