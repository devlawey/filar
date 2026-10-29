#![cfg(feature = "kube")]
//! Live read-only tests against `docker/k8s` (#571).
//!
//! Run explicitly with `FILAR_TEST_KUBECONFIG` pointing at the admin config:
//! `cargo test -p filar-transport --test cluster_api -- --ignored`.

use std::path::{Path, PathBuf};
use std::time::Duration;

use filar_core::KubeCluster;
use filar_transport::{ClusterApi, KubeClusterApi, WorkloadKind};
use kube::config::Kubeconfig;

fn fixture(path: &Path) -> KubeCluster {
    let context = std::env::var("FILAR_TEST_KUBE_CONTEXT").ok().unwrap_or_else(|| {
        Kubeconfig::read_from(path)
            .ok()
            .and_then(|config| config.current_context)
            .expect("test kubeconfig must have a current context (used only to construct the explicit target)")
    });
    KubeCluster {
        name: "filar-tests".into(),
        context,
        kubeconfig: Some(path.to_path_buf()),
        namespaces: vec!["shop".into()],
        default_namespace: Some("shop".into()),
        request_timeout_secs: 20,
        ..KubeCluster::default()
    }
}

fn admin_target() -> KubeCluster {
    let path = std::env::var_os("FILAR_TEST_KUBECONFIG")
        .map(PathBuf::from)
        .expect("FILAR_TEST_KUBECONFIG must point to docker/k8s admin kubeconfig");
    fixture(&path)
}

fn reader_target() -> KubeCluster {
    let admin = admin_target();
    let path = admin
        .kubeconfig
        .as_deref()
        .expect("admin target has a path")
        .parent()
        .expect("admin kubeconfig has a parent directory")
        .join("reader.kubeconfig");
    fixture(&path)
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn all_read_methods_work_against_fixture_cluster() {
    let api = KubeClusterApi::connect(&admin_target()).await.unwrap();

    let pods = api
        .list_pods(Some("shop"), Some("app=web"), Some("status.phase=Running"))
        .await
        .unwrap();
    assert_eq!(pods.len(), 3);
    assert!(pods.iter().all(|pod| {
        pod.ready
            && pod.container_count == 2
            && pod.ready_containers == 2
            && pod
                .containers
                .iter()
                .any(|container| container.name == "sidecar")
    }));

    let crashy = api.get_pod("shop", "crashy").await.unwrap();
    assert!(crashy.restarts > 0);
    let termination = crashy
        .last_termination
        .expect("crashy has a previous termination");
    assert_eq!(termination.reason.as_deref(), Some("Error"));
    assert_eq!(termination.exit_code, Some(1));

    let previous = api
        .pod_logs(
            "shop",
            "crashy",
            None,
            true,
            Some(Duration::from_secs(3600)),
            20,
            16,
        )
        .await
        .unwrap();
    assert!(previous.text.len() <= 16);
    assert!(!previous.text.is_empty());

    let events = api.list_events(Some("shop"), Some("crashy")).await.unwrap();
    assert!(!events.is_empty());
    assert!(events
        .iter()
        .all(|event| event.involved_name.as_deref() == Some("crashy")));

    let deployments = api
        .list_workloads(Some("shop"), WorkloadKind::Deployment)
        .await
        .unwrap();
    assert!(deployments.iter().any(|workload| workload.name == "web"));
    assert!(deployments
        .iter()
        .any(|workload| workload.name == "distroless"));
    for kind in [
        WorkloadKind::StatefulSet,
        WorkloadKind::DaemonSet,
        WorkloadKind::ReplicaSet,
        WorkloadKind::Job,
        WorkloadKind::CronJob,
    ] {
        let _ = api.list_workloads(Some("shop"), kind).await.unwrap();
    }

    let history = api.rollout_history("shop", "web").await.unwrap();
    assert!(!history.is_empty());
    assert!(history
        .windows(2)
        .all(|pair| pair[0].revision <= pair[1].revision));

    let nodes = api.list_nodes().await.unwrap();
    assert!(!nodes.is_empty());
    assert!(nodes.iter().any(|node| node.ready));
    assert!(nodes.iter().all(|node| !node.allocatable.is_empty()));

    match api.top_pods(Some("shop")).await {
        Ok(metrics) => assert!(metrics.iter().all(|metric| !metric.name.is_empty())),
        Err(error) => assert!(error.to_string().contains("Unsupported"), "{error}"),
    }
    match api.top_nodes().await {
        Ok(metrics) => assert!(metrics.iter().all(|metric| !metric.name.is_empty())),
        Err(error) => assert!(error.to_string().contains("Unsupported"), "{error}"),
    }

    let _ = api.list_services(Some("shop")).await.unwrap();
    let _ = api.list_endpoints(Some("shop")).await.unwrap();
    let _ = api.list_ingresses(Some("shop")).await.unwrap();
    let configmaps = api.list_configmaps(Some("shop")).await.unwrap();
    assert!(configmaps
        .iter()
        .any(|configmap| configmap.name == "kube-root-ca.crt"));
    let namespaces = api.namespaces().await.unwrap();
    assert!(namespaces.iter().any(|namespace| namespace.name == "shop"));

    let error = api
        .get_pod("shop", "does-not-exist")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("404"), "{error}");
    let error = api
        .get_pod("does-not-exist", "crashy")
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("404"), "{error}");
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn namespace_rbac_falls_back_without_exec_permission() {
    let api = KubeClusterApi::connect(&reader_target()).await.unwrap();

    let direct = api.list_pods(Some("shop"), None, None).await.unwrap();
    assert!(direct.iter().any(|pod| pod.name == "crashy"));
    let fallback = api.list_pods(None, None, None).await.unwrap();
    assert_eq!(fallback.len(), direct.len());
    assert!(fallback.iter().any(|pod| pod.name == "crashy"));

    let logs = api
        .pod_logs("shop", "crashy", None, true, None, 20, 4096)
        .await
        .unwrap();
    assert!(logs.text.contains("crashy: exiting"));

    let namespaces = api.namespaces().await.unwrap();
    assert_eq!(namespaces.len(), 1);
    assert_eq!(namespaces[0].name, "shop");
    assert_eq!(namespaces[0].phase, "Unknown");
}
