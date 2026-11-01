#![cfg(feature = "kube")]
//! Live `KubeExecutor` tests against `docker/k8s` (#563, #571).
//!
//! Run explicitly with `FILAR_TEST_KUBECONFIG` pointing at the admin config:
//! `cargo test -p filar-transport --test kube_exec -- --ignored --test-threads=1`.
//! `a_deleted_pod_is_replaced_by_the_selector` deletes one `web` pod; the
//! Deployment recreates it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use filar_core::{CoreError, KubeCluster, PodLimits, PodRef};
use filar_transport::{CommandExecutor, KubeExecEvent, KubeExecutor};
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

fn admin_path() -> PathBuf {
    std::env::var_os("FILAR_TEST_KUBECONFIG")
        .map(PathBuf::from)
        .expect("FILAR_TEST_KUBECONFIG must point to docker/k8s admin kubeconfig")
}

fn admin() -> KubeCluster {
    fixture(&admin_path())
}

fn no_exec() -> KubeCluster {
    let path = admin_path()
        .parent()
        .expect("admin kubeconfig has a parent directory")
        .join("no-exec.kubeconfig");
    fixture(&path)
}

fn web(container: Option<&str>) -> PodRef {
    PodRef::by_selector("filar-tests", "shop", "app=web", container)
}

async fn admin_client() -> Client {
    let cluster = admin();
    let kubeconfig = Kubeconfig::read_from(cluster.kubeconfig.as_deref().unwrap()).unwrap();
    let options = KubeConfigOptions {
        context: Some(cluster.context.clone()),
        ..KubeConfigOptions::default()
    };
    Client::try_from(
        Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .unwrap(),
    )
    .unwrap()
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn shell_state_exit_codes_and_stderr() {
    let exec = KubeExecutor::connect(&admin(), &web(Some("main"))).await.unwrap();
    assert!(exec.pod().starts_with("web-"), "{}", exec.pod());
    assert_eq!(exec.container(), "main");

    let r = exec.run("cd /tmp && export FILAR_X=42").await.unwrap();
    assert_eq!(r.exit_code, Some(0));
    let r = exec.run("echo \"$PWD $FILAR_X\"").await.unwrap();
    assert_eq!(r.stdout, "/tmp 42");
    assert_eq!(exec.current_cwd().await.as_deref(), Some("/tmp"));

    let r = exec.run("sh -c 'exit 7'").await.unwrap();
    assert_eq!(r.exit_code, Some(7));

    let r = exec.run("echo out; echo err >&2").await.unwrap();
    assert_eq!(r.stdout, "out");
    assert_eq!(r.stderr, "err\n");
    exec.close().await.unwrap();
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn cancel_interrupts_sleep_and_keeps_the_shell() {
    let exec = Arc::new(KubeExecutor::connect(&admin(), &web(Some("main"))).await.unwrap());
    exec.run("cd /tmp").await.unwrap();
    let running = {
        let exec = exec.clone();
        tokio::spawn(async move { exec.run("sleep 30").await })
    };
    tokio::time::sleep(Duration::from_secs(1)).await;
    let start = Instant::now();
    exec.cancel().await.unwrap();
    let interrupted = running.await.unwrap().unwrap();
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_ne!(interrupted.exit_code, Some(0));
    let r = exec.run("pwd").await.unwrap();
    assert_eq!(r.stdout, "/tmp", "the shell survived the interrupt");
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster; deletes one web fixture pod"]
async fn a_deleted_pod_is_replaced_by_the_selector() {
    let exec = KubeExecutor::connect(&admin(), &web(Some("main"))).await.unwrap();
    let mut events = exec.subscribe();
    let old = exec.pod();
    Api::<Pod>::namespaced(admin_client().await, "shop")
        .delete(&old, &DeleteParams::default().grace_period(0))
        .await
        .unwrap();
    // Wait for the exec stream to notice.
    tokio::time::sleep(Duration::from_secs(5)).await;

    let r = exec.run("echo alive").await.unwrap();
    assert_eq!(r.stdout, "alive");
    let new = exec.pod();
    assert_ne!(new, old);
    assert_eq!(exec.take_pod_change(), Some((old.clone(), new.clone())));
    let changed = loop {
        match events.recv().await.unwrap() {
            KubeExecEvent::PodChanged { old, new } => break (old, new),
            KubeExecEvent::Connected { .. } => continue,
        }
    };
    assert_eq!(changed, (old, new));
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn a_container_without_shell_points_to_debug_containers() {
    let target = PodRef::by_selector("filar-tests", "shop", "app=distroless", None);
    let error = match KubeExecutor::connect(&admin(), &target).await {
        Ok(_) => panic!("distroless has no shell"),
        Err(error) => error.to_string(),
    };
    assert!(error.contains("нет /bin/sh") && error.contains("#576"), "{error}");
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn missing_pods_exec_right_is_an_rbac_message() {
    let error = match KubeExecutor::connect(&no_exec(), &web(Some("main"))).await {
        Ok(_) => panic!("no-exec must not exec"),
        Err(error) => error,
    };
    assert!(matches!(error, CoreError::AccessDenied(_)), "{error:?}");
    let text = error.to_string();
    assert!(text.contains("нет права create на pods/exec в namespace shop"), "{text}");
}

#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn pod_limits_match_the_fixture_spec() {
    let exec = KubeExecutor::connect(&admin(), &web(Some("main"))).await.unwrap();
    assert_eq!(
        exec.pod_limits(),
        PodLimits {
            memory_limit: Some("32Mi".into()),
            memory_request: Some("16Mi".into()),
            cpu_limit: None,
            cpu_request: Some("10m".into()),
            run_as_user: Some(65534),
            run_as_non_root: Some(true),
            read_only_root_filesystem: Some(true),
            dropped_capabilities: vec!["ALL".into()],
        }
    );
    assert!(exec.describe().starts_with("Kubernetes pod shop/web-"));
}

fn reader() -> KubeCluster {
    let path = admin_path()
        .parent()
        .expect("admin kubeconfig has a parent directory")
        .join("reader.kubeconfig");
    fixture(&path)
}

/// #576: a debug container next to the shell-less `distroless` pod sees the
/// target's files through `/proc/1/root`; a service account without
/// `pods/ephemeralcontainers` gets an RBAC message, not a workaround.
#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn debug_container_reads_the_target_root_and_rbac_refuses_without_the_right() {
    use filar_transport::{debug_container_name, DebugContainerSpec};
    let admin_api = filar_transport::KubeClusterApi::connect(&admin()).await.unwrap();
    let pod = filar_transport::ClusterApi::list_pods(
        &admin_api,
        Some("shop"),
        Some("app=distroless"),
        Some("status.phase=Running"),
    )
    .await
    .unwrap()
    .remove(0)
    .name;
    let id = format!("{:x}", Instant::now().elapsed().as_nanos() ^ u128::from(std::process::id()));
    let spec = DebugContainerSpec {
        namespace: "shop".into(),
        pod: pod.clone(),
        target_container: "main".into(),
        image: "busybox:1.37.0".into(),
        name: debug_container_name(&id),
    };
    admin_api
        .create_debug_container(&spec, Duration::from_secs(90))
        .await
        .unwrap();
    let exec = KubeExecutor::connect(
        &admin(),
        &PodRef::named("filar-tests", "shop", &pod, Some(&spec.name)),
    )
    .await
    .unwrap();
    let result = exec.run("ls /proc/1/root/ | head -5; cat /proc/1/cmdline | tr '\\0' ' '").await.unwrap();
    assert!(result.stdout.contains("pause"), "{}", result.stdout);
    exec.close().await.unwrap();

    let reader_api = filar_transport::KubeClusterApi::connect(&reader()).await.unwrap();
    let mut denied = spec.clone();
    // A different name: `debug_container_name` keeps only 8 characters.
    denied.name = debug_container_name(&format!("{:08x}", std::process::id() ^ 0xdead_beef));
    assert_ne!(denied.name, spec.name);
    let error = reader_api
        .create_debug_container(&denied, Duration::from_secs(10))
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("pods/ephemeralcontainers"), "{error}");
}
