//! Addressing a shell inside a Kubernetes pod (#563).
//!
//! [`PodRef`] says where a pod shell goes; [`PodLimits`] is what filar learns
//! about the chosen container from the pod's own spec, so the agent and the UI
//! can keep diagnostics light (an exec counts against the container's memory
//! limit). Both are plain data: no Kubernetes client types, no credentials.

use serde::{Deserialize, Serialize};

/// Which pod of a namespace a shell enters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PodSelector {
    /// A concrete pod name.
    Name(String),
    /// A label selector such as `app=web`: the first `Running` + `Ready` pod
    /// by name is used, and a replacement is picked the same way when that pod
    /// disappears.
    Labels(String),
}

/// Address of a pod shell: a cluster from `[[kube_clusters]]`, a namespace,
/// a pod (by name or selector) and optionally a container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodRef {
    /// Name of the entry in `[[kube_clusters]]`.
    pub cluster: String,
    /// Namespace containing the pod.
    pub namespace: String,
    /// The pod, by name or by label selector.
    pub pod: PodSelector,
    /// Container to enter. `None`: the only container, else the pod's
    /// `kubectl.kubernetes.io/default-container` annotation.
    pub container: Option<String>,
}

impl PodRef {
    /// A pod chosen by name.
    pub fn named(cluster: &str, namespace: &str, pod: &str, container: Option<&str>) -> Self {
        Self {
            cluster: cluster.into(),
            namespace: namespace.into(),
            pod: PodSelector::Name(pod.into()),
            container: container.map(str::to_owned),
        }
    }

    /// A pod chosen by label selector.
    pub fn by_selector(
        cluster: &str,
        namespace: &str,
        selector: &str,
        container: Option<&str>,
    ) -> Self {
        Self {
            cluster: cluster.into(),
            namespace: namespace.into(),
            pod: PodSelector::Labels(selector.into()),
            container: container.map(str::to_owned),
        }
    }
}

/// Limits and security settings of the container a pod shell runs in, read
/// from the pod spec. `None` / empty means the spec does not set it — nothing
/// is measured or run in the container to fill these in.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PodLimits {
    /// `resources.limits.memory` of the container.
    pub memory_limit: Option<String>,
    /// `resources.requests.memory` of the container.
    pub memory_request: Option<String>,
    /// `resources.limits.cpu` of the container.
    pub cpu_limit: Option<String>,
    /// `resources.requests.cpu` of the container.
    pub cpu_request: Option<String>,
    /// Effective `runAsUser`: the container's, else the pod's.
    pub run_as_user: Option<i64>,
    /// Effective `runAsNonRoot`: the container's, else the pod's.
    pub run_as_non_root: Option<bool>,
    /// `readOnlyRootFilesystem` of the container.
    pub read_only_root_filesystem: Option<bool>,
    /// Capabilities dropped by the container (`ALL` included as written).
    pub dropped_capabilities: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_fill_the_selector_kind() {
        let named = PodRef::named("prod-eu", "shop", "web-1", Some("main"));
        assert_eq!(named.pod, PodSelector::Name("web-1".into()));
        assert_eq!(named.container.as_deref(), Some("main"));
        let selected = PodRef::by_selector("prod-eu", "shop", "app=web", None);
        assert_eq!(selected.pod, PodSelector::Labels("app=web".into()));
        assert!(selected.container.is_none());
    }
}
