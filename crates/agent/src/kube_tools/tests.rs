use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use filar_core::{StaticSecretProvider, TagPolicy};
use filar_transport::{
    ConfigMapInfo, EndpointInfo, IngressInfo, NamespaceInfo, NodeInfo, PodLogs, ResourceMetrics,
    RolloutRevision, ServiceInfo, TerminationInfo, WorkloadInfo,
};
use serde_json::json;

use super::*;

#[derive(Default)]
struct MockCluster {
    pods: Vec<PodInfo>,
    events: Vec<EventInfo>,
    logs: Option<PodLogs>,
    metrics_unsupported: bool,
    log_call: Mutex<Option<(bool, u32, usize)>>,
    /// Answer of `pod_http_get`; the request it got is recorded.
    http: Option<filar_transport::PodHttpResponse>,
    http_call: Mutex<Option<filar_transport::PodHttpRequest>>,
}

#[async_trait::async_trait]
impl ClusterApi for MockCluster {
    async fn pod_http_get(
        &self,
        request: &filar_transport::PodHttpRequest,
    ) -> Result<filar_transport::PodHttpResponse> {
        *self.http_call.lock().unwrap() = Some(request.clone());
        self.http
            .clone()
            .ok_or_else(|| CoreError::Unavailable("no HTTP answer in this mock".into()))
    }

    async fn list_pods(
        &self,
        _: Option<&str>,
        _: Option<&str>,
        _: Option<&str>,
    ) -> Result<Vec<PodInfo>> {
        Ok(self.pods.clone())
    }

    async fn get_pod(&self, _: &str, name: &str) -> Result<PodInfo> {
        self.pods
            .iter()
            .find(|pod| pod.name == name)
            .cloned()
            .ok_or_else(|| CoreError::Other("pod not found".into()))
    }

    async fn list_events(&self, _: Option<&str>, involved: Option<&str>) -> Result<Vec<EventInfo>> {
        Ok(self
            .events
            .iter()
            .filter(|event| involved.is_none() || event.involved_name.as_deref() == involved)
            .cloned()
            .collect())
    }

    async fn pod_logs(
        &self,
        _: &str,
        _: &str,
        _: Option<&str>,
        previous: bool,
        _: Option<Duration>,
        tail: u32,
        bytes: usize,
    ) -> Result<PodLogs> {
        *self.log_call.lock().unwrap() = Some((previous, tail, bytes));
        Ok(self.logs.clone().unwrap_or(PodLogs {
            text: String::new(),
            truncated: false,
        }))
    }

    async fn list_workloads(&self, _: Option<&str>, _: WorkloadKind) -> Result<Vec<WorkloadInfo>> {
        Ok(vec![])
    }
    async fn rollout_history(&self, _: &str, _: &str) -> Result<Vec<RolloutRevision>> {
        Ok(vec![])
    }
    async fn list_nodes(&self) -> Result<Vec<NodeInfo>> {
        Ok(vec![])
    }

    async fn top_pods(&self, _: Option<&str>) -> Result<Vec<ResourceMetrics>> {
        if self.metrics_unsupported {
            Err(CoreError::Other(
                "Unsupported: metrics.k8s.io is absent".into(),
            ))
        } else {
            Ok(vec![])
        }
    }

    async fn top_nodes(&self) -> Result<Vec<ResourceMetrics>> {
        self.top_pods(None).await
    }
    async fn list_services(&self, _: Option<&str>) -> Result<Vec<ServiceInfo>> {
        Ok(vec![])
    }
    async fn list_endpoints(&self, _: Option<&str>) -> Result<Vec<EndpointInfo>> {
        Ok(vec![])
    }
    async fn list_ingresses(&self, _: Option<&str>) -> Result<Vec<IngressInfo>> {
        Ok(vec![])
    }
    async fn list_configmaps(&self, _: Option<&str>) -> Result<Vec<ConfigMapInfo>> {
        Ok(vec![])
    }
    async fn namespaces(&self) -> Result<Vec<NamespaceInfo>> {
        Ok(vec![])
    }
}

fn target() -> KubeCluster {
    KubeCluster {
        name: "prod-eu".into(),
        context: "prod-eu-admin".into(),
        default_namespace: Some("shop".into()),
        tags: vec!["prod".into()],
        ..KubeCluster::default()
    }
}

fn pod(name: &str) -> PodInfo {
    PodInfo {
        namespace: "shop".into(),
        name: name.into(),
        phase: "Running".into(),
        ready: false,
        ready_containers: 0,
        container_count: 1,
        restarts: 4,
        last_termination: Some(TerminationInfo {
            container: "main".into(),
            reason: Some("OOMKilled".into()),
            exit_code: Some(137),
            finished_at: Some("2026-01-01T00:00:02Z".into()),
        }),
        node: Some("node-a".into()),
        age_seconds: Some(3600),
        containers: vec![filar_transport::ContainerInfo {
            name: "main".into(),
            image: "example/web:1".into(),
            resource_requests: BTreeMap::from([("memory".into(), "64Mi".into())]),
            resource_limits: BTreeMap::from([("memory".into(), "128Mi".into())]),
            ports: Vec::new(),
        }],
    }
}

fn event(name: &str, timestamp: &str, message: &str) -> EventInfo {
    EventInfo {
        namespace: "shop".into(),
        name: name.into(),
        type_: "Warning".into(),
        reason: "BackOff".into(),
        message: message.into(),
        involved_kind: Some("Pod".into()),
        involved_name: Some("web-1".into()),
        count: 3,
        last_timestamp: Some(timestamp.into()),
    }
}

fn executor(cluster: MockCluster) -> KubeToolExecutor {
    let secrets = Arc::new(StaticSecretProvider::new());
    secrets.insert("TEST_TOKEN", "supersecret");
    KubeToolExecutor::new(Arc::new(cluster), target(), secrets)
}

#[test]
fn exposes_exactly_seven_read_only_tools_without_secret() {
    let defs = kube_tool_definitions(CommandConfirmMode::Always);
    assert_eq!(defs.len(), 7);
    assert_eq!(
        defs.iter().map(|def| def.name.as_str()).collect::<Vec<_>>(),
        vec![
            TOOL_K8S_GET,
            TOOL_K8S_DESCRIBE,
            TOOL_K8S_LOGS,
            TOOL_K8S_EVENTS,
            TOOL_K8S_TOP,
            TOOL_K8S_ROLLOUT_HISTORY,
            TOOL_POD_HTTP_GET
        ]
    );
    let schema =
        serde_json::to_string(&defs.iter().map(|def| &def.parameters).collect::<Vec<_>>()).unwrap();
    assert!(!schema.contains("\"Secret\""));
    assert!(!defs
        .iter()
        .any(|def| def.description.contains("kubectl describe")
            && !def.description.contains("not kubectl describe")));
}

#[test]
fn explain_mode_requires_explanations() {
    for def in kube_tool_definitions(CommandConfirmMode::Explain) {
        assert!(
            def.parameters["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|field| field == "explanation"),
            "{}",
            def.name
        );
    }
}

#[test]
fn read_only_gate_covers_every_mode() {
    for mode in [CommandConfirmMode::Always, CommandConfirmMode::Explain] {
        assert_eq!(
            kube_confirmation_decision(mode, &target(), &[]),
            ConfirmDecision::NeedsConfirmation
        );
    }
    for mode in [CommandConfirmMode::Allowlist, CommandConfirmMode::Never] {
        assert_eq!(
            kube_confirmation_decision(mode, &target(), &[]),
            ConfirmDecision::AutoApproved
        );
    }
}

#[test]
fn tag_policies_only_tighten_the_gate() {
    let policies = [TagPolicy {
        tag: "prod".into(),
        confirm_mode: CommandConfirmMode::Always,
    }];
    assert_eq!(
        kube_confirmation_decision(CommandConfirmMode::Never, &target(), &policies),
        ConfirmDecision::NeedsConfirmation
    );
    assert_eq!(
        kube_confirmation_decision(CommandConfirmMode::Explain, &target(), &policies),
        ConfirmDecision::NeedsConfirmation
    );
    let loosening = [TagPolicy {
        tag: "prod".into(),
        confirm_mode: CommandConfirmMode::Never,
    }];
    assert_eq!(
        kube_confirmation_decision(CommandConfirmMode::Always, &target(), &loosening),
        ConfirmDecision::NeedsConfirmation
    );
}

#[test]
fn parsing_enforces_bounds_and_rejects_secret() {
    let error =
        parse_kube_tool_call(TOOL_K8S_GET, &json!({"kind": "Secret"}), Some("shop")).unwrap_err();
    assert!(error.to_string().contains("unsupported Kubernetes kind"));
    assert!(
        parse_kube_tool_call(TOOL_K8S_GET, &json!({"kind": "ingresses"}), Some("shop")).is_ok()
    );
    assert!(parse_kube_tool_call(
        TOOL_K8S_LOGS,
        &json!({"pod": "web-1", "tail": 501}),
        Some("shop")
    )
    .is_err());
    assert!(parse_kube_tool_call(
        TOOL_K8S_LOGS,
        &json!({"pod": "web-1", "limit_bytes": MAX_KUBE_LOG_BYTES + 1}),
        Some("shop")
    )
    .is_err());
    let nodes =
        parse_kube_tool_call(TOOL_K8S_TOP, &json!({"resource": "nodes"}), Some("shop")).unwrap();
    assert_eq!(nodes.namespace(), None);
}

#[test]
fn confirmation_context_names_tool_arguments_context_and_namespace() {
    let call = parse_kube_tool_call(
        TOOL_K8S_LOGS,
        &json!({"pod": "web-1", "previous": true}),
        Some("shop"),
    )
    .unwrap();
    let context = call.confirmation_context(&target());
    assert!(context.contains("Tool: k8s_logs"));
    assert!(context.contains("\"previous\":true"));
    assert!(context.contains("Kubernetes context: prod-eu-admin"));
    assert!(context.contains("Namespace: shop"));
}

#[test]
fn prompt_matches_capabilities_and_eval_snapshot() {
    let prompt = build_kube_system_prompt(&target(), None);
    for expected in [
        "Kubernetes cluster prod-eu",
        "default namespace shop",
        "read-only API",
        "previous=true",
        "OOMKilled",
        "cannot change",
        "Secrets are unavailable",
        "Environment variable values",
    ] {
        assert!(prompt.contains(expected), "missing {expected:?}: {prompt}");
    }
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../eval/prompts/kube-system.txt");
    let snapshot = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
        .replace("\r\n", "\n");
    assert_eq!(snapshot.trim_end(), prompt.trim_end());
}

#[tokio::test]
async fn get_uses_typed_tables_and_marks_row_truncation() {
    let cluster = MockCluster {
        pods: vec![pod("web-1"), pod("web-2")],
        ..MockCluster::default()
    };
    let executor = executor(cluster);
    let call = parse_kube_tool_call(
        TOOL_K8S_GET,
        &json!({"kind": "pods", "limit": 1}),
        Some("shop"),
    )
    .unwrap();
    let result = executor.execute(&call).await.unwrap();
    assert!(result
        .content
        .contains("NAMESPACE\tNAME\tPHASE\tREADY\tRESTARTS\tLAST REASON\tAGE\tNODE"));
    assert!(result.content.contains("web-1"));
    assert!(result.content.contains("OOMKilled"));
    assert!(!result.content.contains("web-2"));
    assert!(result.content.contains("[truncated: showing 1 of 2 rows"));
    assert!(result.truncated);
}

#[tokio::test]
async fn describe_contains_oom_reason_limits_and_redacted_sorted_events() {
    let cluster = MockCluster {
        pods: vec![pod("web-1")],
        events: vec![
            event("old", "2026-01-01T00:00:01Z", "old supersecret"),
            event("new", "2026-01-01T00:00:03Z", "new event"),
        ],
        ..MockCluster::default()
    };
    let executor = executor(cluster);
    let call = parse_kube_tool_call(
        TOOL_K8S_DESCRIBE,
        &json!({"kind": "Pod", "name": "web-1", "event_limit": 1}),
        Some("shop"),
    )
    .unwrap();
    let result = executor.execute(&call).await.unwrap();
    assert!(result.content.contains("OOMKilled"));
    assert!(result.content.contains("128Mi"));
    assert!(result.content.contains("new event"));
    assert!(!result.content.contains("old event"));
    assert!(!result.content.contains("supersecret"));
    assert!(result.truncated);
}

#[tokio::test]
async fn describe_rejects_direct_pod_call_without_namespace() {
    let executor = executor(MockCluster::default());
    let call = KubeToolCall::Describe {
        kind: KubeResourceKind::Pod,
        namespace: None,
        name: "web-1".into(),
        event_limit: 20,
    };
    let error = executor.execute(&call).await.unwrap_err();
    assert!(error
        .to_string()
        .contains("namespace is required for Pod describe"));
}

#[tokio::test]
async fn logs_pass_previous_and_bounds_then_redact() {
    let cluster = Arc::new(MockCluster {
        logs: Some(PodLogs {
            text: "token=supersecret\nordinary line".into(),
            truncated: true,
        }),
        ..MockCluster::default()
    });
    let secrets = Arc::new(StaticSecretProvider::new());
    secrets.insert("TEST_TOKEN", "supersecret");
    let executor = KubeToolExecutor::new(cluster.clone(), target(), secrets);
    let call = parse_kube_tool_call(TOOL_K8S_LOGS, &json!({"pod": "web-1", "previous": true, "since": "30m", "tail": 500, "limit_bytes": 4096}), Some("shop")).unwrap();
    let result = executor.execute(&call).await.unwrap();
    assert_eq!(*cluster.log_call.lock().unwrap(), Some((true, 500, 4096)));
    assert!(!result.content.contains("supersecret"));
    assert!(result.content.contains("ordinary line"));
    assert!(result.content.contains("log byte limit reached"));
    assert!(result.truncated);
}

#[tokio::test]
async fn log_character_limit_keeps_the_newest_lines() {
    let executor = executor(MockCluster {
        logs: Some(PodLogs {
            text: format!(
                "oldest line\n{}\nnewest OOMKilled line",
                "x".repeat(MAX_KUBE_OUTPUT_CHARS)
            ),
            truncated: false,
        }),
        ..MockCluster::default()
    });
    let call = parse_kube_tool_call(TOOL_K8S_LOGS, &json!({"pod": "web-1"}), Some("shop"))
        .unwrap();
    let result = executor.execute(&call).await.unwrap();
    assert!(!result.content.contains("oldest line"));
    assert!(result.content.contains("newest OOMKilled line"));
    assert!(result.content.contains("older log lines dropped"));
    assert!(result.content.chars().count() <= MAX_KUBE_OUTPUT_CHARS);
    assert!(result.truncated);
}

#[tokio::test]
async fn metrics_unsupported_has_actionable_message() {
    let executor = executor(MockCluster {
        metrics_unsupported: true,
        ..MockCluster::default()
    });
    let call =
        parse_kube_tool_call(TOOL_K8S_TOP, &json!({"resource": "pods"}), Some("shop")).unwrap();
    let error = executor.execute(&call).await.unwrap_err();
    assert!(error.to_string().contains("metrics-server не установлен"));
}


fn pod_with_port(name: &str) -> PodInfo {
    let mut info = pod(name);
    info.containers[0].ports = vec![filar_transport::ContainerPortInfo {
        name: Some("metrics".into()),
        port: 9090,
    }];
    info
}

fn http_call(args: serde_json::Value) -> Result<KubeToolCall> {
    parse_kube_tool_call(TOOL_POD_HTTP_GET, &args, Some("shop"))
}

#[test]
fn pod_http_get_refuses_other_methods_urls_and_binary_pprof() {
    let ok = http_call(json!({"pod": "web-1", "port": 9090, "path": "/metrics"})).unwrap();
    assert_eq!(ok.display_command(), "pod_http_get GET web-1:9090/metrics -n shop");
    let post = http_call(json!({"pod": "web-1", "port": 9090, "path": "/x", "method": "POST"}));
    assert!(post.unwrap_err().to_string().contains("only sends GET"));
    let absolute = http_call(json!({"pod": "web-1", "port": 9090, "path": "http://evil/metrics"}));
    assert!(absolute.is_err());
    let binary = http_call(json!({"pod": "web-1", "port": 6060, "path": "/debug/pprof/heap"}));
    assert!(binary.is_err());
}

#[tokio::test]
async fn pod_http_get_needs_a_declared_or_allowed_port() {
    let cluster = MockCluster {
        pods: vec![pod_with_port("web-1")],
        ..MockCluster::default()
    };
    let call = http_call(json!({"pod": "web-1", "port": 22, "path": "/"})).unwrap();
    let err = executor(cluster).execute(&call).await.unwrap_err().to_string();
    assert!(err.contains("diagnostic_ports"), "{err}");
}

#[tokio::test]
async fn pod_http_get_shows_requested_metrics_redacted_and_marks_truncation() {
    let body = "# HELP up Up\nup 1\ndb_up{password=\"hunter2\"} 1\nhttp_requests_total{code=\"200\"} 9\ntoken_leak supersecret\n";
    let cluster = Arc::new(MockCluster {
        pods: vec![pod_with_port("web-1")],
        http: Some(filar_transport::PodHttpResponse {
            status: 200,
            content_type: Some("text/plain; version=0.0.4".into()),
            body: body.as_bytes().to_vec(),
            truncated: true,
        }),
        ..MockCluster::default()
    });
    let secrets = Arc::new(StaticSecretProvider::new());
    secrets.insert("TEST_TOKEN", "supersecret");
    let exec = KubeToolExecutor::new(cluster.clone(), target(), secrets);
    let call = http_call(json!({"pod": "web-1", "port": "metrics", "path": "/metrics", "metrics": ["db_up", "token_leak"]})).unwrap();
    let result = exec.execute(&call).await.unwrap();
    assert!(result.truncated);
    assert!(result.content.starts_with("HTTP 200 from shop/web-1:9090/metrics"), "{}", result.content);
    assert!(result.content.contains("[truncated: the answer was longer than 1 MiB]"));
    assert!(result.content.contains("password=\"***\""), "{}", result.content);
    assert!(!result.content.contains("hunter2"));
    assert!(!result.content.contains("supersecret"), "known secrets are masked");
    assert!(!result.content.contains("http_requests_total"), "only the named metrics");
    let sent = cluster.http_call.lock().unwrap().clone().unwrap();
    assert_eq!((sent.port, sent.path.as_str()), (9090, "/metrics"));
    assert_eq!(sent.max_bytes, crate::pod_http::MAX_POD_HTTP_BYTES);
}

#[test]
fn pod_http_get_is_a_kube_tool_and_never_a_fleet_call() {
    assert!(is_kube_tool_name(TOOL_POD_HTTP_GET));
    assert!(is_kube_tool_name(TOOL_K8S_GET));
    assert!(!is_kube_tool_name("run_command"));
    let call = http_call(json!({"pod": "web-1", "port": 9090, "path": "/metrics"})).unwrap();
    let member = filar_core::FleetTarget::KubeCluster(filar_core::KubeScopeRef {
        cluster: "prod-eu".into(),
        namespace: "shop".into(),
    });
    assert!(bind_fleet_call(&call, &member).is_err());
}
