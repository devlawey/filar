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

/// Write `lines` (JSON objects) and wait until the LogsQL `marker` finds them.
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

/// `pod_logs_history` (#583) against vlagent-shaped lines: filar writes the
/// LogsQL from namespace, selector and container; a fleet gets a per-pod
/// table from one query.
#[tokio::test]
#[ignore = "needs a VictoriaLogs at FILAR_TEST_VLOGS_URL (docker/logs/README.md)"]
async fn pod_logs_history_reads_vlagent_lines_for_a_pod_and_a_fleet() {
    use filar_agent::logs::k8s::FleetLogMember;
    use filar_agent::{AgentEvent, CommandConfirmer, ToolCall};

    struct Yes;
    #[async_trait::async_trait]
    impl CommandConfirmer for Yes {
        async fn confirm(&self, _: &str, _: &str, _: bool) -> filar_core::Result<bool> {
            Ok(true)
        }
    }

    let ns = format!("hist{}", std::process::id());
    let now = chrono::Utc::now();
    let line = |pod: &str, app: &str, msg: &str, ago: i64| {
        serde_json::json!({
            "_time": (now - chrono::Duration::seconds(ago)).to_rfc3339(),
            "_msg": msg,
            "kubernetes.pod_namespace": ns,
            "kubernetes.pod_name": pod,
            "kubernetes.container_name": "app",
            "kubernetes.pod_labels.app": app,
        })
    };
    let lines = vec![
        line("web-1", "web", "timeout calling pay after 30s", 50),
        line("web-1", "web", "timeout calling pay after 31s", 40),
        line("web-2", "web", "request ok", 30),
        line("pay-1", "pay", "payment accepted", 20),
    ];
    ingest(&lines, &format!("\"kubernetes.pod_namespace\":={ns}")).await;

    let (sources, notes) = build_log_sources(
        &[LogSourceConfig {
            name: "vlogs-live".into(),
            kind: "victorialogs".into(),
            url: url(),
            clusters: vec!["prod-eu".into()],
            ..Default::default()
        }],
        Arc::new(StaticSecretProvider::new()),
        Vec::new(),
    );
    assert!(notes.is_empty(), "{notes:?}");
    let sources = sources.unwrap();
    let emit = |_: AgentEvent| {};
    let call = |args: serde_json::Value| ToolCall {
        id: "h".into(),
        name: "pod_logs_history".into(),
        arguments: args,
    };

    let by_selector = sources
        .process_pod_logs_history(
            &call(serde_json::json!({"selector": "app=web", "since": "10m"})),
            "prod-eu",
            Some(&ns),
            None,
            filar_core::CommandConfirmMode::Never,
            &Yes,
            &emit,
            None,
            1,
        )
        .await
        .unwrap()
        .content;
    assert!(by_selector.contains("3 line(s)"), "{by_selector}");
    assert!(!by_selector.contains("payment accepted"), "{by_selector}");

    let member = |pod: &str| FleetLogMember {
        cluster: "prod-eu".into(),
        namespace: ns.clone(),
        pod: Some(pod.into()),
    };
    let fleet = sources
        .process_fleet_pod_logs(
            &call(serde_json::json!({"since": "10m"})),
            &[member("web-1"), member("web-2"), member("web-3")],
            &Yes,
            &emit,
            None,
            2,
        )
        .await
        .unwrap()
        .content;
    assert!(fleet.contains("web-1 | 2 | 1 | [2×] timeout calling pay"), "{fleet}");
    assert!(fleet.contains("web-2 | 1 | 1 |"), "{fleet}");
    assert!(fleet.contains("web-3 | 0 | 0 | —"), "{fleet}");
}
