//! Live `query_logs` tests against a real Grafana Loki (#582).
//!
//! Start one as described in `docker/logs/README.md`, then:
//! `FILAR_TEST_LOKI_URL=http://127.0.0.1:3100 cargo test -p filar-agent --test loki -- --ignored`

use std::sync::Arc;
use std::time::Duration;

use filar_agent::build_log_sources;
use filar_core::{LogSourceConfig, StaticSecretProvider};

fn url() -> String {
    std::env::var("FILAR_TEST_LOKI_URL").unwrap_or_else(|_| "http://127.0.0.1:3100".into())
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().no_proxy().build().unwrap()
}

#[tokio::test]
#[ignore = "needs a Loki at FILAR_TEST_LOKI_URL (docker/logs/README.md)"]
async fn query_logs_finds_pushed_lines_and_counts_them() {
    let marker = format!("filartest{}", std::process::id());
    let now = chrono::Utc::now().timestamp_nanos_opt().unwrap();
    let values: Vec<serde_json::Value> = (0..5)
        .map(|i| serde_json::json!([(now - (5 - i) * 1_000_000_000).to_string(), format!("level=error msg=\"{marker} connection refused to db-{i}:5432\"")]))
        .chain(std::iter::once(serde_json::json!([now.to_string(), format!("level=info msg=\"{marker} ok\"")])))
        .collect();
    let push = serde_json::json!({"streams": [{"stream": {"namespace": "shop", "pod": "web-1", "test": marker}, "values": values}]});
    let status = client()
        .post(format!("{}/loki/api/v1/push", url()))
        .json(&push)
        .send()
        .await
        .expect("Loki is not reachable; see docker/logs/README.md")
        .status();
    assert!(status.is_success(), "push failed: {status}");

    let (sources, notes) = build_log_sources(
        &[LogSourceConfig {
            name: "loki-live".into(),
            kind: "loki".into(),
            url: url(),
            ..Default::default()
        }],
        Arc::new(StaticSecretProvider::new()),
        Vec::new(),
    );
    assert!(notes.is_empty(), "{notes:?}");
    let sources = sources.unwrap();
    let call = |query: String| {
        sources
            .parse_call(
                &serde_json::json!({"source": "loki-live", "query": query, "since": "10m"}),
                chrono::Utc::now() + chrono::Duration::seconds(1),
            )
            .unwrap()
    };

    let mut text = String::new();
    for _ in 0..30 {
        text = sources
            .execute(&call(format!("{{test=\"{marker}\"}} |= \"error\"")))
            .await
            .unwrap();
        if text.contains("5 line(s)") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert!(text.contains("5 line(s)"), "{text}");
    assert!(text.contains("[5×]"), "{text}");
    assert!(text.contains("pod=web-1"), "labels as fields: {text}");

    let metric = sources
        .execute(&call(format!("sum by (pod) (count_over_time({{test=\"{marker}\"}}[10m]))")))
        .await
        .unwrap();
    assert!(metric.contains("Metric result: 1 series"), "{metric}");
    assert!(metric.contains("{pod=\"web-1\"}"), "{metric}");

    let err = sources
        .execute(&call("{test=\"x\"} |= ".into()))
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("400") && err.contains("parse error"), "{err}");
}
