#![cfg(feature = "kube")]
//! Live write tests against `docker/k8s` (#638): apply a pod and a PVC,
//! scale, restart, undo and delete a Deployment of their own.
//!
//! Run explicitly with `FILAR_TEST_KUBECONFIG` pointing at the admin config:
//! `cargo test -p filar-transport --test cluster_write -- --ignored`.

use std::path::PathBuf;

use filar_core::KubeCluster;
use filar_transport::{
    ApplyAction, ClusterWriteApi, DeleteOptions, DeleteOutcome, DeleteTarget, KubeClusterApi,
    ObjectKind, RestartKind, ScaleKind,
};
use kube::config::Kubeconfig;

fn admin_target() -> KubeCluster {
    let path = std::env::var_os("FILAR_TEST_KUBECONFIG")
        .map(PathBuf::from)
        .expect("FILAR_TEST_KUBECONFIG must point to docker/k8s admin kubeconfig");
    let context = std::env::var("FILAR_TEST_KUBE_CONTEXT")
        .ok()
        .unwrap_or_else(|| {
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

const WORKLOAD: &str = r#"
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: filar-write-test}
spec:
  accessModes: [ReadWriteOnce]
  resources: {requests: {storage: 16Mi}}
---
apiVersion: v1
kind: Pod
metadata: {name: filar-write-test, labels: {filar-test: write}}
spec:
  containers: [{name: main, image: "busybox:1.37", command: [sleep, "3600"]}]
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: filar-write-test, labels: {filar-test: write}}
spec:
  replicas: 1
  selector: {matchLabels: {app: filar-write-test}}
  template:
    metadata: {labels: {app: filar-write-test}}
    spec: {containers: [{name: main, image: "busybox:1.36", command: [sleep, "3600"]}]}
"#;

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn apply_scale_restart_undo_and_delete_against_fixture_cluster() {
    let api = KubeClusterApi::connect(&admin_target()).await.unwrap();

    let dry = api.apply(WORKLOAD, Some("shop"), true).await.unwrap();
    assert!(dry
        .iter()
        .all(|object| object.action == ApplyAction::Created));
    let applied = api.apply(WORKLOAD, Some("shop"), false).await.unwrap();
    assert_eq!(applied.len(), 3);
    let again = api.apply(WORKLOAD, Some("shop"), true).await.unwrap();
    assert!(again
        .iter()
        .all(|object| object.action == ApplyAction::Unchanged));

    let scaled = api
        .scale(ScaleKind::Deployment, "shop", "filar-write-test", 2, false)
        .await
        .unwrap();
    assert_eq!((scaled.from, scaled.to), (1, 2));

    // A second revision: new image, then back with undo.
    let newer = WORKLOAD.replace("busybox:1.36", "busybox:1.37");
    api.apply(&newer, Some("shop"), false).await.unwrap();
    api.rollout_restart(RestartKind::Deployment, "shop", "filar-write-test", true)
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let undo = api
        .rollout_undo("shop", "filar-write-test", None, false)
        .await
        .unwrap();
    assert!(undo.to_revision < undo.from_revision);

    for (kind, api_version) in [
        ("Deployment", "apps/v1"),
        ("Pod", "v1"),
        ("PersistentVolumeClaim", "v1"),
    ] {
        let kind = ObjectKind {
            api_version: Some(api_version.into()),
            kind: kind.into(),
        };
        let deleted = api
            .delete(
                &kind,
                Some("shop"),
                &DeleteTarget::Name("filar-write-test".into()),
                &DeleteOptions::default(),
                false,
            )
            .await
            .unwrap();
        assert!(matches!(
            deleted[0].outcome,
            DeleteOutcome::Deleted | DeleteOutcome::Pending
        ));
    }
}
