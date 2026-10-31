//! Live read-only Kubernetes fleet tests against `docker/k8s` (#568).
//!
//! Run explicitly with `FILAR_TEST_KUBECONFIG` pointing at the admin config:
//! `cargo test -p filar-agent --test kube_fleet -- --ignored`.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use filar_agent::fleet_result::HostState;
use filar_agent::{KubeFleet, KubeFleetCluster, KubeResourceKind, KubeToolCall};
use filar_core::{FleetTarget, HostGroup, KubeCluster, KubeGroup, StaticSecretProvider};
use k8s_openapi::api::core::v1::Pod;
use kube::api::DeleteParams;
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config};

fn fixture(path: &Path) -> KubeCluster {
    let context = std::env::var("FILAR_TEST_KUBE_CONTEXT").ok().unwrap_or_else(|| {
        Kubeconfig::read_from(path)
            .ok()
            .and_then(|config| config.current_context)
            .expect("test kubeconfig must have a current context")
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

fn group(selector: &str) -> HostGroup {
    HostGroup {
        name: "web-prod".into(),
        kube: Some(KubeGroup {
            clusters: vec!["filar-tests".into()],
            namespace: "shop".into(),
            selector: Some(selector.into()),
        }),
        max_parallel: 3,
        per_host_timeout_secs: 20,
        ..HostGroup::default()
    }
}

async fn open_fleet(selector: &str, expected: usize) -> KubeFleet {
    let target = admin_target();
    let cluster = KubeFleetCluster::connect(target, Arc::new(StaticSecretProvider::new()))
        .await
        .expect("connect to fixture cluster");
    for _ in 0..60 {
        let fleet = KubeFleet::open(&group(selector), vec![cluster.clone()])
            .await
            .expect("discover fixture fleet");
        if fleet.fleet().len() == expected {
            return fleet;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("selector {selector} did not produce {expected} ready pods");
}

fn get_pod() -> KubeToolCall {
    KubeToolCall::Get {
        kind: KubeResourceKind::Pod,
        namespace: None,
        label_selector: None,
        field_selector: None,
        limit: 1,
    }
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn k8s_web_fleet_gets_and_folds_three_ready_pods() {
    let fleet = open_fleet("app=web", 3).await;
    let report = fleet.run(&get_pod()).await.expect("fleet get succeeds");

    assert_eq!(report.result().rows().len(), 3);
    assert_eq!(report.folded().hosts(), 3);
    assert_eq!(report.folded().compared(), 3);
    assert!(report
        .result()
        .rows()
        .iter()
        .all(|row| row.state() == HostState::Success));
    assert!(report.folded().groups().iter().all(|group| group.len() == 1));
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn k8s_crashy_fleet_reads_previous_container_logs() {
    let fleet = open_fleet("app=crashy", 1).await;
    let call = KubeToolCall::Logs {
        namespace: "ignored".into(),
        pod: "ignored".into(),
        container: Some("sidecar".into()),
        previous: true,
        since: Some(Duration::from_secs(3600)),
        tail: 20,
        limit_bytes: 4096,
    };

    let mut last = String::new();
    for _ in 0..60 {
        let report = fleet.run(&call).await.expect("previous-log operation completes");
        last = report.render();
        if report.result().rows()[0].state() == HostState::Success && last.contains("crashy: exiting") {
            return;
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    panic!("crashy --previous did not appear in the fleet fold: {last}");
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster; deletes one web fixture pod"]
async fn k8s_pod_deleted_after_fleet_entry_is_reported_as_disappeared() {
    let target = admin_target();
    let fleet = open_fleet("app=web", 3).await;
    let pod = match fleet.fleet().members()[0].target() {
        FleetTarget::KubePod(pod) => pod.clone(),
        other => panic!("expected a pod member, got {}", other.name()),
    };

    let kubeconfig = Kubeconfig::read_from(target.kubeconfig.as_deref().expect("fixture kubeconfig path"))
        .expect("read fixture kubeconfig");
    let options = KubeConfigOptions {
        context: Some(target.context.clone()),
        ..KubeConfigOptions::default()
    };
    let config = Config::from_custom_kubeconfig(kubeconfig, &options)
        .await
        .expect("load fixture admin context");
    let client = Client::try_from(config).expect("create fixture admin client");
    Api::<Pod>::namespaced(client, "shop")
        .delete(&pod.pod, &DeleteParams::default())
        .await
        .expect("delete the selected fixture pod");

    let report = fleet.run(&get_pod()).await.expect("operation completes");
    assert_eq!(report.result().rows().len(), 3, "replacement pods never join");
    assert_eq!(report.result().summary().count(HostState::PodDisappeared), 1);
    assert!(report.render().contains("pod disappeared"), "{}", report.render());
}
