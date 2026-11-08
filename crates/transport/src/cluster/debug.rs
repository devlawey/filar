//! Ephemeral debug containers (#576): `kubectl debug --target`, by explicit
//! user action only.
//!
//! This is a write to the cluster that stays in the pod's spec until the pod
//! is recreated — an agreed exception to invariant 1 (see `AGENTS.md`). The
//! agent never reaches this module: it is not a tool and not a command.
//! The container inherits the target container's user and drops every
//! capability, so it never runs with more rights than the target.

use std::time::{Duration, Instant};

use filar_core::{CoreError, KubeCluster, Result};
use k8s_openapi::api::core::v1::{Container, Pod, PodSecurityContext};
use kube::api::{Patch, PatchParams};
use kube::Api;
use serde_json::{json, Value};

use super::client::{api_error, KubeClusterApi};

/// User the debug container runs as when the target's user is not in the
/// spec: `nobody`.
pub const NOBODY_UID: i64 = 65534;

/// Prefix of debug container names: `filar-debug-<id>`.
pub const DEBUG_CONTAINER_PREFIX: &str = "filar-debug-";

/// What to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugContainerSpec {
    /// Pod namespace.
    pub namespace: String,
    /// Pod name.
    pub pod: String,
    /// Container whose processes the debug container shares.
    pub target_container: String,
    /// Image, one of the cluster's `debug_images`.
    pub image: String,
    /// `filar-debug-<id>`.
    pub name: String,
}

/// `filar-debug-<id>` from a short id: lowercase hex/digits only, so the
/// name is a valid DNS label.
pub fn debug_container_name(id: &str) -> String {
    let id: String = id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .take(8)
        .collect();
    format!("{DEBUG_CONTAINER_PREFIX}{id}")
}

/// The `securityContext` of the debug container: the target's effective user
/// and group (the container's, else the pod's), non-root when the target is,
/// no privilege escalation, every capability dropped, never privileged — so
/// it is never wider than the target.
///
/// When the spec does not name the target's user (it comes from the image's
/// `USER`), the debug image's own default — often root — could be wider, so
/// the debug container runs as [`NOBODY_UID`] instead: never wider than any
/// target, at worst narrower.
pub fn debug_security_context(target: &Container, pod: Option<&PodSecurityContext>) -> Value {
    let own = target.security_context.as_ref();
    let run_as_user = own.and_then(|c| c.run_as_user).or(pod.and_then(|p| p.run_as_user));
    let run_as_group = own.and_then(|c| c.run_as_group).or(pod.and_then(|p| p.run_as_group));
    let run_as_non_root = own
        .and_then(|c| c.run_as_non_root)
        .or(pod.and_then(|p| p.run_as_non_root));
    let (run_as_user, run_as_group, run_as_non_root) = match run_as_user {
        Some(uid) => (Some(uid), run_as_group, run_as_non_root),
        None => (
            Some(NOBODY_UID),
            run_as_group.or(Some(NOBODY_UID)),
            Some(true),
        ),
    };
    let mut ctx = json!({
        "allowPrivilegeEscalation": false,
        "privileged": false,
        "capabilities": {"drop": ["ALL"]},
    });
    if let Some(uid) = run_as_user {
        ctx["runAsUser"] = json!(uid);
    }
    if let Some(gid) = run_as_group {
        ctx["runAsGroup"] = json!(gid);
    }
    if let Some(non_root) = run_as_non_root {
        ctx["runAsNonRoot"] = json!(non_root);
    }
    if let Some(ro) = own.and_then(|c| c.read_only_root_filesystem) {
        ctx["readOnlyRootFilesystem"] = json!(ro);
    }
    if let Some(seccomp) = own
        .and_then(|c| c.seccomp_profile.clone())
        .or(pod.and_then(|p| p.seccomp_profile.clone()))
    {
        ctx["seccompProfile"] = serde_json::to_value(seccomp).unwrap_or(Value::Null);
    }
    ctx
}

/// The strategic-merge patch for `pods/<pod>/ephemeralcontainers`.
pub fn ephemeral_patch(spec: &DebugContainerSpec, security_context: Value) -> Value {
    json!({
        "spec": {
            "ephemeralContainers": [{
                "name": spec.name,
                "image": spec.image,
                "targetContainerName": spec.target_container,
                "command": ["sh"],
                "stdin": true,
                "tty": false,
                "securityContext": security_context,
            }]
        }
    })
}

/// State of one ephemeral container from the pod status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DebugState {
    /// Not reported yet, or still being created.
    Pending,
    /// Running.
    Running,
    /// Waiting with a reason that will not resolve on its own.
    Failed(String),
}

/// Read the debug container's state from a pod.
pub fn debug_state(pod: &Pod, name: &str) -> DebugState {
    let Some(status) = pod
        .status
        .as_ref()
        .and_then(|s| s.ephemeral_container_statuses.as_ref())
        .and_then(|all| all.iter().find(|s| s.name == name))
    else {
        return DebugState::Pending;
    };
    let Some(state) = status.state.as_ref() else {
        return DebugState::Pending;
    };
    if state.running.is_some() {
        return DebugState::Running;
    }
    if let Some(terminated) = state.terminated.as_ref() {
        return DebugState::Failed(format!(
            "debug container exited ({})",
            terminated.reason.clone().unwrap_or_else(|| terminated.exit_code.to_string())
        ));
    }
    if let Some(reason) = state.waiting.as_ref().and_then(|w| w.reason.clone()) {
        const FATAL: [&str; 5] = [
            "ErrImagePull",
            "ImagePullBackOff",
            "InvalidImageName",
            "CreateContainerConfigError",
            "CreateContainerError",
        ];
        if FATAL.contains(&reason.as_str()) {
            let message = state
                .waiting
                .as_ref()
                .and_then(|w| w.message.clone())
                .unwrap_or_default();
            return DebugState::Failed(format!("образ не скачался или не запустился: {reason} {message}").trim().to_string());
        }
    }
    DebugState::Pending
}

impl KubeClusterApi {
    /// Add the ephemeral debug container and wait until it runs (#576):
    /// [`Self::add_debug_container`], then [`Self::wait_debug_container`].
    pub async fn create_debug_container(
        &self,
        spec: &DebugContainerSpec,
        wait: Duration,
    ) -> Result<()> {
        self.add_debug_container(spec).await?;
        self.wait_debug_container(spec, wait).await
    }

    /// Add the ephemeral debug container to the pod spec (#576). After `Ok`
    /// the container is in the spec for good, whether or not it starts.
    /// RBAC (`patch` on `pods/ephemeralcontainers`) and admission-policy
    /// denials come back as distinct messages; nothing is retried around them.
    pub async fn add_debug_container(&self, spec: &DebugContainerSpec) -> Result<()> {
        let target: &KubeCluster = self.target();
        let pods: Api<Pod> = Api::namespaced(self.client().clone(), &spec.namespace);
        let pod = pods.get(&spec.pod).await.map_err(|error| {
            api_error(target, error, "get", "pods", Some(&spec.namespace))
        })?;
        let container = pod
            .spec
            .as_ref()
            .and_then(|s| s.containers.iter().find(|c| c.name == spec.target_container))
            .ok_or_else(|| {
                CoreError::NotFound(format!(
                    "pod {}/{} has no container {}",
                    spec.namespace, spec.pod, spec.target_container
                ))
            })?;
        let context =
            debug_security_context(container, pod.spec.as_ref().and_then(|s| s.security_context.as_ref()));
        pods.patch_ephemeral_containers(
            &spec.pod,
            &PatchParams::default(),
            &Patch::Strategic(ephemeral_patch(spec, context)),
        )
        .await
        .map_err(|error| {
            api_error(target, error, "patch", "pods/ephemeralcontainers", Some(&spec.namespace))
        })?;
        Ok(())
    }

    /// Wait until the added debug container runs (#576). Errors here mean it
    /// is already in the pod spec but did not start.
    pub async fn wait_debug_container(&self, spec: &DebugContainerSpec, wait: Duration) -> Result<()> {
        let target: &KubeCluster = self.target();
        let pods: Api<Pod> = Api::namespaced(self.client().clone(), &spec.namespace);
        let deadline = Instant::now() + wait;
        loop {
            let pod = pods.get(&spec.pod).await.map_err(|error| {
                api_error(target, error, "get", "pods", Some(&spec.namespace))
            })?;
            match debug_state(&pod, &spec.name) {
                DebugState::Running => return Ok(()),
                DebugState::Failed(why) => return Err(CoreError::Other(why)),
                DebugState::Pending if Instant::now() >= deadline => {
                    return Err(CoreError::Unavailable(format!(
                        "debug-контейнер {} не запустился за {} с",
                        spec.name,
                        wait.as_secs()
                    )))
                }
                DebugState::Pending => tokio::time::sleep(Duration::from_millis(500)).await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Capabilities, ContainerState, ContainerStateRunning, ContainerStateWaiting,
        ContainerStatus, PodStatus, SecurityContext,
    };

    fn spec() -> DebugContainerSpec {
        DebugContainerSpec {
            namespace: "shop".into(),
            pod: "web-1".into(),
            target_container: "main".into(),
            image: "busybox:1.37".into(),
            name: debug_container_name("A1b2-C3d4e5"),
        }
    }

    #[test]
    fn names_are_dns_labels_with_the_prefix() {
        assert_eq!(debug_container_name("A1b2-C3d4e5"), "filar-debug-a1b2c3d4");
    }

    #[test]
    fn patch_names_the_target_image_and_container() {
        let patch = ephemeral_patch(&spec(), json!({}));
        let ec = &patch["spec"]["ephemeralContainers"][0];
        assert_eq!(ec["name"], "filar-debug-a1b2c3d4");
        assert_eq!(ec["image"], "busybox:1.37");
        assert_eq!(ec["targetContainerName"], "main");
    }

    #[test]
    fn security_context_is_never_wider_than_the_target() {
        let target = Container {
            name: "main".into(),
            security_context: Some(SecurityContext {
                run_as_user: Some(65534),
                run_as_non_root: Some(true),
                read_only_root_filesystem: Some(true),
                capabilities: Some(Capabilities {
                    add: Some(vec!["NET_ADMIN".into()]),
                    drop: None,
                }),
                privileged: Some(true),
                allow_privilege_escalation: Some(true),
                ..Default::default()
            }),
            ..Default::default()
        };
        let pod = PodSecurityContext {
            run_as_group: Some(65534),
            ..Default::default()
        };
        let ctx = debug_security_context(&target, Some(&pod));
        assert_eq!(ctx["runAsUser"], 65534);
        assert_eq!(ctx["runAsGroup"], 65534, "the pod's group when the container sets none");
        assert_eq!(ctx["runAsNonRoot"], true);
        assert_eq!(ctx["readOnlyRootFilesystem"], true);
        assert_eq!(ctx["privileged"], false, "never privileged, whatever the target says");
        assert_eq!(ctx["allowPrivilegeEscalation"], false);
        assert_eq!(ctx["capabilities"], json!({"drop": ["ALL"]}), "no added capabilities");
    }

    #[test]
    fn a_target_without_an_explicit_user_gets_nobody_not_the_image_default() {
        let target = Container {
            name: "main".into(),
            ..Default::default()
        };
        let ctx = debug_security_context(&target, None);
        assert_eq!(ctx["runAsUser"], NOBODY_UID);
        assert_eq!(ctx["runAsGroup"], NOBODY_UID);
        assert_eq!(ctx["runAsNonRoot"], true);
        assert_eq!(ctx["capabilities"], json!({"drop": ["ALL"]}));
    }

    fn pod_with(state: ContainerState) -> Pod {
        Pod {
            status: Some(PodStatus {
                ephemeral_container_statuses: Some(vec![ContainerStatus {
                    name: "filar-debug-a1b2c3d4".into(),
                    state: Some(state),
                    ..Default::default()
                }]),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn running_pending_and_image_failures_are_told_apart() {
        let name = "filar-debug-a1b2c3d4";
        assert_eq!(debug_state(&Pod::default(), name), DebugState::Pending);
        let running = pod_with(ContainerState {
            running: Some(ContainerStateRunning::default()),
            ..Default::default()
        });
        assert_eq!(debug_state(&running, name), DebugState::Running);
        let pulling = pod_with(ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some("ContainerCreating".into()),
                message: None,
            }),
            ..Default::default()
        });
        assert_eq!(debug_state(&pulling, name), DebugState::Pending);
        let backoff = pod_with(ContainerState {
            waiting: Some(ContainerStateWaiting {
                reason: Some("ImagePullBackOff".into()),
                message: Some("not found".into()),
            }),
            ..Default::default()
        });
        assert!(matches!(debug_state(&backoff, name), DebugState::Failed(m) if m.contains("образ не скачался")));
    }
}
