//! Read-only Kubernetes cluster API.
//!
//! The public surface deliberately contains no Kubernetes client types and no
//! write operations. In particular, there is no method that can request a
//! `Secret`. Enable the crate feature `kube` to use this module.

use std::collections::BTreeMap;
use std::time::Duration;

use filar_core::Result;

mod client;
mod convert;

pub use client::KubeClusterApi;

/// Maximum number of objects returned by one cluster list operation.
pub const DEFAULT_CLUSTER_LIST_LIMIT: u32 = 500;

/// A container declared by a pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerInfo {
    /// Container name.
    pub name: String,
    /// Image reference, as declared by the pod.
    pub image: String,
}

/// The most recent terminated state reported for a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminationInfo {
    /// Container that terminated.
    pub container: String,
    /// Kubernetes reason such as `OOMKilled` or `Error`.
    pub reason: Option<String>,
    /// Process exit code, when reported.
    pub exit_code: Option<i32>,
    /// RFC 3339 termination time, when reported.
    pub finished_at: Option<String>,
}

/// Operational summary of a Kubernetes pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodInfo {
    /// Pod namespace.
    pub namespace: String,
    /// Pod name.
    pub name: String,
    /// Pod phase such as `Running` or `Pending`.
    pub phase: String,
    /// Whether the pod has the `Ready=True` condition.
    pub ready: bool,
    /// Number of ready regular containers.
    pub ready_containers: u32,
    /// Total number of regular containers.
    pub container_count: u32,
    /// Sum of regular container restart counts.
    pub restarts: u32,
    /// Latest previous termination across all regular containers.
    pub last_termination: Option<TerminationInfo>,
    /// Node to which the pod is assigned.
    pub node: Option<String>,
    /// Age in whole seconds, saturated at zero for future timestamps.
    pub age_seconds: Option<u64>,
    /// Regular containers and their images.
    pub containers: Vec<ContainerInfo>,
}

/// A Kubernetes event related to an object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventInfo {
    /// Event namespace.
    pub namespace: String,
    /// Event name.
    pub name: String,
    /// Event type (`Normal` or `Warning`).
    pub type_: String,
    /// Machine-readable reason.
    pub reason: String,
    /// Human-readable message.
    pub message: String,
    /// Kind of the involved object, when present.
    pub involved_kind: Option<String>,
    /// Name of the involved object, when present.
    pub involved_name: Option<String>,
    /// Number of occurrences.
    pub count: i32,
    /// RFC 3339 timestamp of the latest occurrence.
    pub last_timestamp: Option<String>,
}

/// Log text returned by a bounded, non-following pod log request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodLogs {
    /// UTF-8 log text, truncated at a character boundary when necessary.
    pub text: String,
    /// Whether the client had to truncate the response to `limit_bytes`.
    pub truncated: bool,
}

/// Workload resource kinds supported by [`ClusterApi::list_workloads`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadKind {
    /// Stateless scalable application.
    Deployment,
    /// Stateful application with stable identities.
    StatefulSet,
    /// One pod per selected node.
    DaemonSet,
    /// Pod replica controller, normally owned by a Deployment.
    ReplicaSet,
    /// Run-to-completion workload.
    Job,
    /// Scheduled Job template.
    CronJob,
}

/// Operational summary common to supported workload kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadInfo {
    /// Workload namespace.
    pub namespace: String,
    /// Workload name.
    pub name: String,
    /// Workload kind.
    pub kind: WorkloadKind,
    /// Desired replicas or scheduled pods, when meaningful.
    pub desired: Option<i32>,
    /// Ready replicas or current scheduled pods, when meaningful.
    pub ready: Option<i32>,
    /// Updated replicas, when reported.
    pub updated: Option<i32>,
    /// Available replicas, when reported.
    pub available: Option<i32>,
    /// Age in whole seconds.
    pub age_seconds: Option<u64>,
    /// Container images declared by the workload pod template.
    pub images: Vec<String>,
}

/// One ReplicaSet revision in a Deployment rollout history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolloutRevision {
    /// Value of `deployment.kubernetes.io/revision`, or zero if absent.
    pub revision: u64,
    /// ReplicaSet name.
    pub name: String,
    /// Desired replicas.
    pub desired: Option<i32>,
    /// Ready replicas.
    pub ready: Option<i32>,
    /// RFC 3339 creation timestamp.
    pub created_at: Option<String>,
    /// Value of `kubernetes.io/change-cause`, when present.
    pub change_cause: Option<String>,
}

/// A condition reported by a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeCondition {
    /// Condition type.
    pub type_: String,
    /// Condition status (`True`, `False`, or `Unknown`).
    pub status: String,
    /// Machine-readable reason.
    pub reason: Option<String>,
    /// Human-readable message.
    pub message: Option<String>,
}

/// Operational summary of a cluster node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeInfo {
    /// Node name.
    pub name: String,
    /// Whether the node has `Ready=True`.
    pub ready: bool,
    /// All reported node conditions.
    pub conditions: Vec<NodeCondition>,
    /// Allocatable resources in Kubernetes quantity notation.
    pub allocatable: BTreeMap<String, String>,
    /// Age in whole seconds.
    pub age_seconds: Option<u64>,
}

/// Resource usage of one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContainerMetrics {
    /// Container name.
    pub name: String,
    /// CPU usage in Kubernetes quantity notation.
    pub cpu: String,
    /// Memory usage in Kubernetes quantity notation.
    pub memory: String,
}

/// Resource usage returned by `metrics.k8s.io`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourceMetrics {
    /// Namespace for pod metrics; `None` for node metrics.
    pub namespace: Option<String>,
    /// Pod or node name.
    pub name: String,
    /// Total CPU usage in Kubernetes quantity notation.
    pub cpu: String,
    /// Total memory usage in Kubernetes quantity notation.
    pub memory: String,
    /// Per-container usage for pods; empty for nodes.
    pub containers: Vec<ContainerMetrics>,
}

/// A port exposed by a Service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicePortInfo {
    /// Port name, when set.
    pub name: Option<String>,
    /// Network protocol.
    pub protocol: String,
    /// Service port.
    pub port: i32,
    /// Target port in integer or named form.
    pub target_port: Option<String>,
    /// Node port, when allocated.
    pub node_port: Option<i32>,
}

/// Operational summary of a Service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    /// Service namespace.
    pub namespace: String,
    /// Service name.
    pub name: String,
    /// Service type such as `ClusterIP`.
    pub type_: String,
    /// Cluster IP, when allocated.
    pub cluster_ip: Option<String>,
    /// External IPs declared on the Service.
    pub external_ips: Vec<String>,
    /// Exposed ports.
    pub ports: Vec<ServicePortInfo>,
    /// Label selector.
    pub selector: BTreeMap<String, String>,
    /// Age in whole seconds.
    pub age_seconds: Option<u64>,
}

/// Addresses and ports currently associated with an Endpoints object.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointInfo {
    /// Endpoints namespace.
    pub namespace: String,
    /// Endpoints name.
    pub name: String,
    /// Ready addresses.
    pub addresses: Vec<String>,
    /// Addresses that are not ready.
    pub not_ready_addresses: Vec<String>,
    /// Ports in `name:port/protocol` notation.
    pub ports: Vec<String>,
}

/// Operational summary of an Ingress.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IngressInfo {
    /// Ingress namespace.
    pub namespace: String,
    /// Ingress name.
    pub name: String,
    /// Ingress class name.
    pub class_name: Option<String>,
    /// Host rules.
    pub hosts: Vec<String>,
    /// Load-balancer IPs or hostnames.
    pub addresses: Vec<String>,
    /// Age in whole seconds.
    pub age_seconds: Option<u64>,
}

/// ConfigMap metadata and non-Secret payload.
///
/// Callers displaying `data` must apply the project's ConfigMap redaction
/// policy. `ClusterApi` never reads Kubernetes `Secret` objects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigMapInfo {
    /// ConfigMap namespace.
    pub namespace: String,
    /// ConfigMap name.
    pub name: String,
    /// String data stored in the ConfigMap.
    pub data: BTreeMap<String, String>,
    /// Keys of binary values; binary contents are not exposed by this API.
    pub binary_data_keys: Vec<String>,
    /// Age in whole seconds.
    pub age_seconds: Option<u64>,
}

/// A namespace visible to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceInfo {
    /// Namespace name.
    pub name: String,
    /// Lifecycle phase, or `Unknown` for a configured RBAC fallback.
    pub phase: String,
    /// Age in whole seconds, when returned by the API.
    pub age_seconds: Option<u64>,
}

/// Read-only access to Kubernetes resources needed by agent tools and fleets.
///
/// Passing `None` as a list namespace requests a cluster-wide list. If RBAC
/// rejects that list, implementations may retry only the namespaces explicitly
/// configured on the target. No method follows continuation tokens silently.
#[async_trait::async_trait]
pub trait ClusterApi: Send + Sync {
    /// List pods with optional label and field selectors.
    async fn list_pods(
        &self,
        namespace: Option<&str>,
        selector: Option<&str>,
        field_selector: Option<&str>,
    ) -> Result<Vec<PodInfo>>;

    /// Get one pod by namespace and name.
    async fn get_pod(&self, namespace: &str, name: &str) -> Result<PodInfo>;

    /// List events, optionally restricted to an involved object name.
    async fn list_events(
        &self,
        namespace: Option<&str>,
        involved_object: Option<&str>,
    ) -> Result<Vec<EventInfo>>;

    /// Read bounded pod logs without following the stream.
    async fn pod_logs(
        &self,
        namespace: &str,
        pod: &str,
        container: Option<&str>,
        previous: bool,
        since: Option<Duration>,
        tail_lines: u32,
        limit_bytes: usize,
    ) -> Result<PodLogs>;

    /// List workloads of one supported kind.
    async fn list_workloads(
        &self,
        namespace: Option<&str>,
        kind: WorkloadKind,
    ) -> Result<Vec<WorkloadInfo>>;

    /// Read a Deployment's ReplicaSet-based rollout history.
    async fn rollout_history(
        &self,
        namespace: &str,
        deployment: &str,
    ) -> Result<Vec<RolloutRevision>>;

    /// List cluster nodes with conditions and allocatable resources.
    async fn list_nodes(&self) -> Result<Vec<NodeInfo>>;

    /// Read pod metrics, or `Unsupported` when the metrics API is absent.
    async fn top_pods(&self, namespace: Option<&str>) -> Result<Vec<ResourceMetrics>>;

    /// Read node metrics, or `Unsupported` when the metrics API is absent.
    async fn top_nodes(&self) -> Result<Vec<ResourceMetrics>>;

    /// List Services.
    async fn list_services(&self, namespace: Option<&str>) -> Result<Vec<ServiceInfo>>;

    /// List Endpoints.
    async fn list_endpoints(&self, namespace: Option<&str>) -> Result<Vec<EndpointInfo>>;

    /// List Ingresses.
    async fn list_ingresses(&self, namespace: Option<&str>) -> Result<Vec<IngressInfo>>;

    /// List ConfigMaps. Their data still requires output redaction.
    async fn list_configmaps(&self, namespace: Option<&str>) -> Result<Vec<ConfigMapInfo>>;

    /// List namespaces, falling back to configured names under namespace RBAC.
    async fn namespaces(&self) -> Result<Vec<NamespaceInfo>>;
}
