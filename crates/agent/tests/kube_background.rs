//! Live background-job tests in pods of `docker/k8s` (#599).
//!
//! Run explicitly with `FILAR_TEST_KUBECONFIG` pointing at the admin config:
//! `cargo test -p filar-agent --test kube_background -- --ignored`.

use std::path::PathBuf;
use std::time::Duration;

use filar_agent::background;
use filar_core::{KubeCluster, PodRef};
use filar_transport::{CommandExecutor, KubeExecutor};
use kube::config::Kubeconfig;

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

async fn connect(selector: &str) -> KubeExecutor {
    let target = PodRef::by_selector("filar-tests", "shop", selector, Some("main"));
    KubeExecutor::connect(&admin(), &target)
        .await
        .expect("exec into fixture pod")
}

fn session(name: &str) -> String {
    format!("kube-bg-{name}-{}", std::process::id())
}

/// `web/main`: read-only root, `/tmp` is an emptyDir — the job runs, its
/// output is read, and the log is gone afterwards.
#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn background_job_runs_in_a_read_only_pod_with_tmp_volume() {
    let exec = connect("app=web").await;
    let sid = session("web");

    let started = background::start_job(&sid, "echo filar-bg-ok; sleep 1", false, &exec)
        .await
        .expect("job starts");
    assert!(started.contains("status: running"), "{started}");

    let mut status = String::new();
    for _ in 0..30 {
        status = background::job_status(&sid, "job-1", 20, false, &exec)
            .await
            .expect("status poll");
        if !status.contains("status: running") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(status.contains("status: done"), "{status}");
    assert!(status.contains("filar-bg-ok"), "{status}");

    let left = exec
        .run(&format!("ls /tmp/filar-job-{sid}-* 2>/dev/null | wc -l"))
        .await
        .unwrap();
    assert_eq!(left.stdout.trim(), "0", "log must be removed after completion");
}

/// `nowrite/main`: no writable directory — an honest refusal, no job.
#[tokio::test]
#[ignore = "needs docker/k8s cluster"]
async fn background_job_is_refused_without_a_writable_dir() {
    let exec = connect("app=nowrite").await;
    let sid = session("nowrite");

    let err = background::start_job(&sid, "sleep 1", false, &exec)
        .await
        .expect_err("no writable directory");
    let msg = err.to_string();
    assert!(msg.contains("background job unavailable"), "{msg}");
    assert!(background::snapshot(&sid, 10).is_empty(), "no phantom job");
}
