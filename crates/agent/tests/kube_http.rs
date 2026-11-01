//! Live `pod_http_get` tests against `docker/k8s` (#578).
//!
//! Run explicitly with `FILAR_TEST_KUBECONFIG` pointing at the admin config:
//! `cargo test -p filar-agent --test kube_http -- --ignored`.

use std::path::PathBuf;
use std::sync::Arc;

use filar_agent::kube_tools::{parse_kube_tool_call, KubeToolExecutor, TOOL_POD_HTTP_GET};
use filar_core::{KubeCluster, StaticSecretProvider};
use filar_transport::{ClusterApi, KubeClusterApi};
use kube::config::Kubeconfig;
use serde_json::json;

fn admin() -> KubeCluster {
    let path = std::env::var_os("FILAR_TEST_KUBECONFIG")
        .map(PathBuf::from)
        .expect("FILAR_TEST_KUBECONFIG must point to docker/k8s admin kubeconfig");
    let context = std::env::var("FILAR_TEST_KUBE_CONTEXT").ok().unwrap_or_else(|| {
        Kubeconfig::read_from(&path)
            .ok()
            .and_then(|config| config.current_context)
            .expect("test kubeconfig must have a current context")
    });
    KubeCluster {
        name: "filar-tests".into(),
        context,
        kubeconfig: Some(path),
        namespaces: vec!["shop".into()],
        default_namespace: Some("shop".into()),
        request_timeout_secs: 20,
        ..KubeCluster::default()
    }
}

/// Metrics of the `httpd` fixture through port-forward, by port name.
#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn pod_http_get_reads_metrics_through_port_forward() {
    let target = admin();
    let api: Arc<dyn ClusterApi> = Arc::new(KubeClusterApi::connect(&target).await.unwrap());
    let pod = api
        .list_pods(Some("shop"), Some("app=httpd"), Some("status.phase=Running"))
        .await
        .unwrap()
        .into_iter()
        .next()
        .expect("httpd fixture pod")
        .name;
    let executor = KubeToolExecutor::new(api, target, Arc::new(StaticSecretProvider::new()));

    let call = parse_kube_tool_call(
        TOOL_POD_HTTP_GET,
        &json!({"pod": pod, "port": "http", "path": "/metrics", "metrics": ["up"]}),
        Some("shop"),
    )
    .unwrap();
    let result = executor.execute(&call).await.unwrap();
    assert!(result.content.starts_with("HTTP 200"), "{}", result.content);
    assert!(result.content.contains("up 1"), "{}", result.content);
    assert!(!result.content.contains("http_requests_total"), "{}", result.content);

    let undeclared = parse_kube_tool_call(
        TOOL_POD_HTTP_GET,
        &json!({"pod": pod, "port": 22, "path": "/"}),
        Some("shop"),
    )
    .unwrap();
    assert!(executor.execute(&undeclared).await.is_err());
}
