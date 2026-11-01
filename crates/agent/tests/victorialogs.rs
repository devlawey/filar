//! Live `query_logs` tests against a real VictoriaLogs (#580).
//!
//! Start one as described in `docker/logs/README.md`, then:
//! `FILAR_TEST_VLOGS_URL=http://127.0.0.1:9428 cargo test -p filar-agent --test victorialogs -- --ignored`

use std::sync::Arc;
use std::time::Duration;

use filar_agent::build_log_sources;
use filar_core::{LogSourceConfig, StaticSecretProvider};

fn url() -> String {
    std::env::var("FILAR_TEST_VLOGS_URL").unwrap_or_else(|_| "http://127.0.0.1:9428".into())
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

/// Write `lines` (JSON objects) and wait until `marker` is searchable.
async fn ingest(lines: &[serde_json::Value], marker: &str) {
    let body: String = lines.iter().map(|l| format!("{l}\n")).collect();
    let status = client()
        .post(format!("{}/insert/jsonline?_stream_fields=kubernetes.pod_namespace", url()))
        .header("Content-Type", "application/stream+json")
        .body(body)
        .send()
        .await
        .expect("VictoriaLogs is not reachable; see docker/logs/README.md")
        .status();
    assert!(status.is_success(), "insert failed: {status}");
    for _ in 0..50 {
        let found = client()
            .post(format!("{}/select/logsql/query", url()))
            .form(&[("query", marker), ("limit", "1")])
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();
        if !found.trim().is_empty() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    panic!("the inserted lines never became searchable");
}

#[tokio::test]
#[ignore = "needs a VictoriaLogs at FILAR_TEST_VLOGS_URL (docker/logs/README.md)"]
async fn query_logs_finds_inserted_lines_folded_and_newest_first() {
    let marker = format!("filartest{}", std::process::id());
    let now = chrono::Utc::now();
    let lines: Vec<serde_json::Value> = (0..6)
        .map(|i| {
            serde_json::json!({
                "_time": (now - chrono::Duration::seconds(60 - i * 5)).to_rfc3339(),
                "_msg": format!("{marker} connection refused to db-{i}:5432"),
                "kubernetes.pod_namespace": "shop",
                "level": "error",
            })
        })
        .chain(std::iter::once(serde_json::json!({
            "_time": now.to_rfc3339(),
            "_msg": format!("{marker} OOMKilled worker"),
            "kubernetes.pod_namespace": "shop",
        })))
        .collect();
    ingest(&lines, &marker).await;

    let (sources, notes) = build_log_sources(
        &[LogSourceConfig {
            name: "vlogs-live".into(),
            kind: "victorialogs".into(),
            url: url(),
            ..Default::default()
        }],
        Arc::new(StaticSecretProvider::new()),
        Vec::new(),
    );
    assert!(notes.is_empty(), "{notes:?}");
    let sources = sources.unwrap();
    let call = sources
        .parse_call(
            &serde_json::json!({"source": "vlogs-live", "query": format!("{marker} kubernetes.pod_namespace:shop"), "since": "10m"}),
            chrono::Utc::now() + chrono::Duration::seconds(1),
        )
        .unwrap();
    let text = sources.execute(&call).await.unwrap();
    assert!(text.contains("7 line(s)"), "{text}");
    assert!(text.contains("[6×]"), "{text}");
    assert!(text.contains("[1×]") && text.contains("OOMKilled worker"), "{text}");
    // Groups come by count: the six refusals before the single OOMKilled.
    let first_group = text.lines().find(|l| l.starts_with('[')).unwrap();
    assert!(first_group.starts_with("[6×]"), "groups by count: {text}");

    // A broken LogsQL query comes back as the server's reason.
    let bad = sources
        .parse_call(
            &serde_json::json!({"source": "vlogs-live", "query": "error | stats count(", "since": "5m"}),
            chrono::Utc::now(),
        )
        .unwrap();
    let err = sources.execute(&bad).await.unwrap_err().to_string();
    assert!(err.contains("400") && err.contains("cannot parse"), "{err}");
}
