#![cfg(feature = "kube")]
//! Live write tests against `docker/k8s` (#638): apply a pod and a PVC,
//! scale, restart, undo and delete a Deployment of their own.
//!
//! Run explicitly with `FILAR_TEST_KUBECONFIG` pointing at the admin config:
//! `cargo test -p filar-transport --test cluster_write -- --ignored`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

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
  securityContext: {runAsNonRoot: true, runAsUser: 65534, seccompProfile: {type: RuntimeDefault}}
  containers:
  - name: main
    image: "busybox:1.37"
    command: [sleep, "3600"]
    securityContext: {allowPrivilegeEscalation: false, capabilities: {drop: [ALL]}}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: filar-write-test, labels: {filar-test: write}}
spec:
  replicas: 1
  selector: {matchLabels: {app: filar-write-test}}
  template:
    metadata: {labels: {app: filar-write-test}}
    spec:
      securityContext: {runAsNonRoot: true, runAsUser: 65534, seccompProfile: {type: RuntimeDefault}}
      containers:
      - name: main
        image: "busybox:1.36"
        command: [sleep, "3600"]
        securityContext: {allowPrivilegeEscalation: false, capabilities: {drop: [ALL]}}
"#;

const KINDS: [(&str, &str); 3] = [
    ("Deployment", "apps/v1"),
    ("Pod", "v1"),
    ("PersistentVolumeClaim", "v1"),
];

/// Delete the test objects; missing ones are fine (a rerun after a failed
/// run starts from a clean namespace).
async fn delete_workload(api: &KubeClusterApi) -> Vec<DeleteOutcome> {
    let mut outcomes = Vec::new();
    for (kind, api_version) in KINDS {
        let kind = ObjectKind {
            api_version: Some(api_version.into()),
            kind: kind.into(),
        };
        let deleted = api
            .delete(
                &kind,
                Some("shop"),
                &DeleteTarget::Names(vec!["filar-write-test".into()]),
                &DeleteOptions::default(),
                false,
            )
            .await
            .unwrap();
        outcomes.extend(deleted.into_iter().map(|d| d.outcome));
    }
    outcomes
}

/// Wait until nothing named `filar-write-test` is left from an earlier run.
async fn wait_gone(api: &KubeClusterApi) {
    for _ in 0..60 {
        let dry = api.apply(WORKLOAD, Some("shop"), true).await.unwrap();
        if dry
            .iter()
            .all(|object| object.action == ApplyAction::Created)
        {
            return;
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    panic!("objects of an earlier run are still terminating");
}

async fn revision(api: &KubeClusterApi) -> i64 {
    let dry = api.apply(WORKLOAD, Some("shop"), true).await.unwrap();
    dry.iter()
        .find(|object| object.object.kind == "Deployment")
        .and_then(|object| {
            object.live.as_ref()?["metadata"]["annotations"]["deployment.kubernetes.io/revision"]
                .as_str()?
                .parse()
                .ok()
        })
        .unwrap_or(0)
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn apply_scale_restart_undo_and_delete_against_fixture_cluster() {
    let api = Arc::new(KubeClusterApi::connect(&admin_target()).await.unwrap());
    delete_workload(&api).await;
    wait_gone(&api).await;
    // The scenario runs as its own task so a failed assertion still leaves
    // a clean namespace behind.
    if let Err(failure) = tokio::spawn(scenario(api.clone())).await {
        delete_workload(&api).await;
        std::panic::resume_unwind(failure.into_panic());
    }
}

async fn scenario(api: Arc<KubeClusterApi>) {
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

    // A second revision: new image, then back with undo. The controller
    // must have written revision 1 first, or the new image becomes it.
    let mut waited = 0;
    while revision(&api).await < 1 {
        assert!(waited < 60, "revision 1 never appeared");
        tokio::time::sleep(Duration::from_secs(1)).await;
        waited += 1;
    }
    let newer = WORKLOAD.replace("busybox:1.36", "busybox:1.37");
    api.apply(&newer, Some("shop"), false).await.unwrap();
    let restart = api
        .rollout_restart(RestartKind::Deployment, "shop", "filar-write-test", true)
        .await
        .unwrap();
    assert!(
        restart.result["spec"]["template"]["metadata"]["annotations"]
            ["kubectl.kubernetes.io/restartedAt"]
            .is_string()
    );
    let after = api.apply(&newer, Some("shop"), true).await.unwrap();
    let live_template = |objects: &[filar_transport::AppliedObject]| {
        objects
            .iter()
            .find(|object| object.object.kind == "Deployment")
            .and_then(|object| object.live.as_ref())
            .map(|live| live["spec"]["template"].clone())
    };
    assert_eq!(
        live_template(&after),
        Some(restart.live["spec"]["template"].clone()),
        "a dry-run restart is not stored"
    );
    // The controller writes revision 2 asynchronously; undo needs it.
    let mut waited = 0;
    while revision(&api).await < 2 {
        assert!(waited < 60, "revision 2 never appeared");
        tokio::time::sleep(Duration::from_secs(1)).await;
        waited += 1;
    }
    let undo = api
        .rollout_undo("shop", "filar-write-test", None, false)
        .await
        .unwrap();
    assert!(undo.to_revision < undo.from_revision);

    for outcome in delete_workload(&api).await {
        assert!(matches!(
            outcome,
            DeleteOutcome::Deleted | DeleteOutcome::Pending
        ));
    }
}
