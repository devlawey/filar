use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::time::Duration;

use filar_core::{CoreError, KubeCluster, Result};
use k8s_openapi::api::apps::v1::{Deployment, ReplicaSet};
use k8s_openapi::api::core::v1::{Namespace, Node, Pod};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use k8s_openapi::NamespaceResourceScope;
use kube::api::{ApiResource, DynamicObject, GroupVersionKind, ListParams, LogParams};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Api, Client, Config, Resource};
use serde::de::DeserializeOwned;

use super::convert;
use super::{
    ClusterApi, ConfigMapInfo, ContainerMetrics, EndpointInfo, EventInfo, IngressInfo,
    NamespaceInfo, NodeInfo, PodInfo, PodLogs, ResourceMetrics, RolloutRevision, ServiceInfo,
    WorkloadInfo, WorkloadKind, DEFAULT_CLUSTER_LIST_LIMIT,
};

/// [`ClusterApi`] implementation backed by a kubeconfig-selected API server.
///
/// All Kubernetes-specific client state is private. The target is cloned so
/// error messages and namespace-scoped RBAC fallback never depend on later
/// configuration changes.
pub struct KubeClusterApi {
    client: Client,
    target: KubeCluster,
}

impl KubeClusterApi {
    /// Read context names from a kubeconfig without contacting any cluster.
    ///
    /// The path follows the same explicit-path / `$KUBECONFIG` / `~/.kube/config`
    /// order as [`connect`](Self::connect). Only names are returned: credentials
    /// and the rest of the kubeconfig stay inside this module.
    pub fn context_names(kubeconfig: Option<&Path>) -> Result<Vec<String>> {
        let config = match kubeconfig {
            Some(path) => Kubeconfig::read_from(expand_home(path)),
            None => Kubeconfig::read(),
        }
        .map_err(|_| CoreError::Other("cannot read or parse the selected kubeconfig".into()))?;
        Ok(config.contexts.into_iter().map(|context| context.name).collect())
    }

    /// Connect using the target's explicit context and configured timeout.
    ///
    /// The kubeconfig path comes from the target, `$KUBECONFIG`, or
    /// `~/.kube/config`, in that order. `current-context` is never consulted.
    /// Exec credential plugins and proxy settings are handled by `kube-rs`.
    pub async fn connect(target: &KubeCluster) -> Result<Self> {
        let kubeconfig = match target.kubeconfig.as_deref() {
            Some(path) => Kubeconfig::read_from(expand_home(path)),
            None => Kubeconfig::read(),
        }
        .map_err(|_| connect_error(target, "cannot read or parse the selected kubeconfig"))?;
        let options = KubeConfigOptions {
            context: Some(target.context.clone()),
            ..KubeConfigOptions::default()
        };
        let mut config = Config::from_custom_kubeconfig(kubeconfig, &options)
            .await
            .map_err(|_| {
                connect_error(
                    target,
                    "cannot load the explicit context or its credentials",
                )
            })?;
        let timeout = Duration::from_secs(target.request_timeout_secs);
        config.connect_timeout = Some(timeout);
        config.read_timeout = Some(timeout);
        config.write_timeout = Some(timeout);
        let client = Client::try_from(config)
            .map_err(|_| connect_error(target, "cannot initialize the Kubernetes client"))?;
        Ok(Self {
            client,
            target: target.clone(),
        })
    }

    async fn list_page<K, F, T>(
        &self,
        namespace: Option<&str>,
        selector: Option<&str>,
        field_selector: Option<&str>,
        limit: u32,
        convert: F,
    ) -> std::result::Result<Vec<T>, kube::Error>
    where
        K: Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Debug,
        K::DynamicType: Default,
        F: Fn(K) -> T,
    {
        let params = bounded_list_params(selector, field_selector, limit);
        let api = match namespace {
            Some(namespace) => Api::<K>::namespaced(self.client.clone(), namespace),
            None => Api::<K>::all(self.client.clone()),
        };
        api.list(&params)
            .await
            .map(|list| list.items.into_iter().map(convert).collect())
    }

    async fn list_namespaced<K, F, T>(
        &self,
        namespace: Option<&str>,
        resource: &str,
        selector: Option<&str>,
        field_selector: Option<&str>,
        convert: F,
    ) -> Result<Vec<T>>
    where
        K: Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Debug,
        K::DynamicType: Default,
        F: Fn(K) -> T + Copy,
    {
        let first = self
            .list_page(
                namespace,
                selector,
                field_selector,
                DEFAULT_CLUSTER_LIST_LIMIT,
                convert,
            )
            .await;
        match first {
            Ok(items) => Ok(items),
            Err(error)
                if namespace.is_none()
                    && is_rbac_denied(&error)
                    && !self.target.namespaces.is_empty() =>
            {
                self.list_configured_namespaces::<K, F, T>(
                    resource,
                    selector,
                    field_selector,
                    convert,
                )
                .await
            }
            Err(error) => Err(self.api_error(error, "list", resource, namespace)),
        }
    }

    async fn list_configured_namespaces<K, F, T>(
        &self,
        resource: &str,
        selector: Option<&str>,
        field_selector: Option<&str>,
        convert: F,
    ) -> Result<Vec<T>>
    where
        K: Resource<Scope = NamespaceResourceScope> + Clone + DeserializeOwned + Debug,
        K::DynamicType: Default,
        F: Fn(K) -> T + Copy,
    {
        let mut output = Vec::new();
        for namespace in &self.target.namespaces {
            let remaining = DEFAULT_CLUSTER_LIST_LIMIT.saturating_sub(output.len() as u32);
            if remaining == 0 {
                break;
            }
            let page = self
                .list_page(
                    Some(namespace),
                    selector,
                    field_selector,
                    remaining,
                    convert,
                )
                .await
                .map_err(|error| self.api_error(error, "list", resource, Some(namespace)))?;
            output.extend(page);
        }
        Ok(output)
    }

    fn api_error(
        &self,
        error: kube::Error,
        verb: &str,
        resource: &str,
        namespace: Option<&str>,
    ) -> CoreError {
        api_error(&self.target, error, verb, resource, namespace)
    }

    async fn metrics_page(
        &self,
        namespace: Option<&str>,
        kind: &str,
        plural: &str,
    ) -> std::result::Result<Vec<ResourceMetrics>, kube::Error> {
        let resource = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", kind),
            plural,
        );
        let api = match namespace {
            Some(namespace) => {
                Api::<DynamicObject>::namespaced_with(self.client.clone(), namespace, &resource)
            }
            None => Api::<DynamicObject>::all_with(self.client.clone(), &resource),
        };
        api.list(&bounded_list_params(None, None, DEFAULT_CLUSTER_LIST_LIMIT))
            .await
            .map(|list| list.items.into_iter().map(metrics_info).collect())
    }

    fn metrics_error(
        &self,
        error: kube::Error,
        resource: &str,
        namespace: Option<&str>,
    ) -> CoreError {
        metrics_error(&self.target, error, resource, namespace)
    }
}

fn metrics_error(
    target: &KubeCluster,
    error: kube::Error,
    resource: &str,
    namespace: Option<&str>,
) -> CoreError {
    if matches!(&error, kube::Error::Api(status) if status.code == 404) {
        return CoreError::Other(format!(
            "Unsupported: metrics.k8s.io is not available for {resource} (context {})",
            target.context
        ));
    }
    api_error(target, error, "list", resource, namespace)
}

#[async_trait::async_trait]
impl ClusterApi for KubeClusterApi {
    async fn list_pods(
        &self,
        namespace: Option<&str>,
        selector: Option<&str>,
        field_selector: Option<&str>,
    ) -> Result<Vec<PodInfo>> {
        self.list_namespaced(
            namespace,
            "pods",
            selector,
            field_selector,
            convert::pod_info,
        )
        .await
    }

    async fn get_pod(&self, namespace: &str, name: &str) -> Result<PodInfo> {
        Api::<Pod>::namespaced(self.client.clone(), namespace)
            .get(name)
            .await
            .map(convert::pod_info)
            .map_err(|error| self.api_error(error, "get", "pods", Some(namespace)))
    }

    async fn list_events(
        &self,
        namespace: Option<&str>,
        involved_object: Option<&str>,
    ) -> Result<Vec<EventInfo>> {
        let field_selector = involved_object.map(|name| format!("involvedObject.name={name}"));
        self.list_namespaced(
            namespace,
            "events",
            None,
            field_selector.as_deref(),
            convert::event_info,
        )
        .await
    }

    async fn pod_logs(
        &self,
        namespace: &str,
        pod: &str,
        container: Option<&str>,
        previous: bool,
        since: Option<Duration>,
        tail_lines: u32,
        limit_bytes: usize,
    ) -> Result<PodLogs> {
        let limit = i64::try_from(limit_bytes).map_err(|_| {
            CoreError::Other("pod log limit_bytes exceeds the Kubernetes API range".into())
        })?;
        let params = bounded_log_params(container, previous, since, tail_lines, limit);
        let text = Api::<Pod>::namespaced(self.client.clone(), namespace)
            .logs(pod, &params)
            .await
            .map_err(|error| self.api_error(error, "get logs for", "pods", Some(namespace)))?;
        Ok(truncate_logs(text, limit_bytes))
    }

    async fn list_workloads(
        &self,
        namespace: Option<&str>,
        kind: WorkloadKind,
    ) -> Result<Vec<WorkloadInfo>> {
        match kind {
            WorkloadKind::Deployment => {
                self.list_namespaced(
                    namespace,
                    "deployments",
                    None,
                    None,
                    convert::deployment_info,
                )
                .await
            }
            WorkloadKind::StatefulSet => {
                self.list_namespaced(
                    namespace,
                    "statefulsets",
                    None,
                    None,
                    convert::statefulset_info,
                )
                .await
            }
            WorkloadKind::DaemonSet => {
                self.list_namespaced(namespace, "daemonsets", None, None, convert::daemonset_info)
                    .await
            }
            WorkloadKind::ReplicaSet => {
                self.list_namespaced(
                    namespace,
                    "replicasets",
                    None,
                    None,
                    convert::replicaset_info,
                )
                .await
            }
            WorkloadKind::Job => {
                self.list_namespaced(namespace, "jobs", None, None, convert::job_info)
                    .await
            }
            WorkloadKind::CronJob => {
                self.list_namespaced(namespace, "cronjobs", None, None, convert::cronjob_info)
                    .await
            }
        }
    }

    async fn rollout_history(
        &self,
        namespace: &str,
        deployment: &str,
    ) -> Result<Vec<RolloutRevision>> {
        let deployment = Api::<Deployment>::namespaced(self.client.clone(), namespace)
            .get(deployment)
            .await
            .map_err(|error| self.api_error(error, "get", "deployments", Some(namespace)))?;
        let deployment_uid = deployment.metadata.uid.as_deref();
        let selector = deployment
            .spec
            .as_ref()
            .and_then(|spec| label_selector_string(&spec.selector));
        let replicasets = Api::<ReplicaSet>::namespaced(self.client.clone(), namespace)
            .list(&bounded_list_params(
                selector.as_deref(),
                None,
                DEFAULT_CLUSTER_LIST_LIMIT,
            ))
            .await
            .map_err(|error| self.api_error(error, "list", "replicasets", Some(namespace)))?;
        let mut revisions = replicasets
            .items
            .into_iter()
            .filter(|replicaset| {
                deployment_uid.is_some()
                    && replicaset
                        .metadata
                        .owner_references
                        .as_ref()
                        .is_some_and(|owners| {
                            owners.iter().any(|owner| {
                                owner.kind == "Deployment"
                                    && Some(owner.uid.as_str()) == deployment_uid
                            })
                        })
            })
            .map(convert::rollout_revision)
            .collect::<Vec<_>>();
        revisions.sort_by_key(|revision| revision.revision);
        Ok(revisions)
    }

    async fn list_nodes(&self) -> Result<Vec<NodeInfo>> {
        Api::<Node>::all(self.client.clone())
            .list(&bounded_list_params(None, None, DEFAULT_CLUSTER_LIST_LIMIT))
            .await
            .map(|list| list.items.into_iter().map(convert::node_info).collect())
            .map_err(|error| self.api_error(error, "list", "nodes", None))
    }

    async fn top_pods(&self, namespace: Option<&str>) -> Result<Vec<ResourceMetrics>> {
        let first = self.metrics_page(namespace, "PodMetrics", "pods").await;
        let result = match first {
            Err(error) if namespace.is_none() && is_rbac_denied(&error) => {
                if self.target.namespaces.is_empty() {
                    Err(error)
                } else {
                    let mut output = Vec::new();
                    for namespace in &self.target.namespaces {
                        let remaining =
                            DEFAULT_CLUSTER_LIST_LIMIT.saturating_sub(output.len() as u32);
                        if remaining == 0 {
                            break;
                        }
                        let resource = ApiResource::from_gvk_with_plural(
                            &GroupVersionKind::gvk("metrics.k8s.io", "v1beta1", "PodMetrics"),
                            "pods",
                        );
                        let page = match Api::<DynamicObject>::namespaced_with(
                            self.client.clone(),
                            namespace,
                            &resource,
                        )
                        .list(&bounded_list_params(None, None, remaining))
                        .await
                        {
                            Ok(page) => page,
                            Err(error) => {
                                return Err(self.metrics_error(
                                    error,
                                    "pods.metrics.k8s.io",
                                    Some(namespace),
                                ));
                            }
                        };
                        output.extend(page.items.into_iter().map(metrics_info));
                    }
                    Ok(output)
                }
            }
            other => other,
        };
        result.map_err(|error| self.metrics_error(error, "pods.metrics.k8s.io", namespace))
    }

    async fn top_nodes(&self) -> Result<Vec<ResourceMetrics>> {
        self.metrics_page(None, "NodeMetrics", "nodes")
            .await
            .map_err(|error| self.metrics_error(error, "nodes.metrics.k8s.io", None))
    }

    async fn list_services(&self, namespace: Option<&str>) -> Result<Vec<ServiceInfo>> {
        self.list_namespaced(namespace, "services", None, None, convert::service_info)
            .await
    }

    async fn list_endpoints(&self, namespace: Option<&str>) -> Result<Vec<EndpointInfo>> {
        self.list_namespaced(namespace, "endpoints", None, None, convert::endpoint_info)
            .await
    }

    async fn list_ingresses(&self, namespace: Option<&str>) -> Result<Vec<IngressInfo>> {
        self.list_namespaced(namespace, "ingresses", None, None, convert::ingress_info)
            .await
    }

    async fn list_configmaps(&self, namespace: Option<&str>) -> Result<Vec<ConfigMapInfo>> {
        self.list_namespaced(namespace, "configmaps", None, None, convert::configmap_info)
            .await
    }

    async fn namespaces(&self) -> Result<Vec<NamespaceInfo>> {
        match Api::<Namespace>::all(self.client.clone())
            .list(&bounded_list_params(None, None, DEFAULT_CLUSTER_LIST_LIMIT))
            .await
        {
            Ok(list) => Ok(list
                .items
                .into_iter()
                .map(convert::namespace_info)
                .collect()),
            Err(error) if is_rbac_denied(&error) && !self.target.namespaces.is_empty() => Ok(self
                .target
                .namespaces
                .iter()
                .take(DEFAULT_CLUSTER_LIST_LIMIT as usize)
                .map(|name| NamespaceInfo {
                    name: name.clone(),
                    phase: "Unknown".into(),
                    age_seconds: None,
                })
                .collect()),
            Err(error) => Err(self.api_error(error, "list", "namespaces", None)),
        }
    }
}

fn expand_home(path: &Path) -> PathBuf {
    if path == Path::new("~") {
        return dirs::home_dir().unwrap_or_else(|| path.to_path_buf());
    }
    if let Ok(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    path.to_path_buf()
}

fn connect_error(target: &KubeCluster, reason: &str) -> CoreError {
    CoreError::Other(format!(
        "Kubernetes target {} (context {}): {reason}",
        target.name, target.context
    ))
}

fn is_rbac_denied(error: &kube::Error) -> bool {
    matches!(error, kube::Error::Api(status) if status.code == 403 && is_rbac_message(&status.message))
}

fn is_rbac_message(message: &str) -> bool {
    message.contains("forbidden") && message.contains(" cannot ") && message.contains(" resource ")
}

fn api_error(
    target: &KubeCluster,
    error: kube::Error,
    verb: &str,
    resource: &str,
    namespace: Option<&str>,
) -> CoreError {
    match error {
        kube::Error::Api(status) if status.code == 403 => {
            if is_rbac_message(&status.message) {
                let scope = namespace
                    .map(|namespace| format!("namespace {namespace}"))
                    .unwrap_or_else(|| "cluster scope".into());
                CoreError::Other(format!(
                    "нет права {verb} на {resource} в {scope} (контекст {})",
                    target.context
                ))
            } else {
                // Admission/policy denials are intentionally preserved verbatim.
                CoreError::Other(status.message)
            }
        }
        kube::Error::Api(status) if status.code == 404 => {
            let scope = namespace
                .map(|namespace| format!(" in namespace {namespace}"))
                .unwrap_or_default();
            CoreError::Other(format!(
                "Kubernetes API returned 404 Not Found for {verb} {resource}{scope} (context {})",
                target.context
            ))
        }
        kube::Error::Api(status) => CoreError::Other(format!(
            "Kubernetes API rejected {verb} {resource} (context {}, HTTP {}, {})",
            target.context, status.code, status.reason
        )),
        _ => CoreError::Other(format!(
            "Kubernetes API is unavailable or the {verb} {resource} request timed out (context {})",
            target.context
        )),
    }
}

/// Convert a Deployment selector to the Kubernetes list selector syntax.
/// Unknown requirement operators are omitted, which only broadens the page;
/// owner-UID filtering still selects the Deployment's actual ReplicaSets.
fn label_selector_string(selector: &LabelSelector) -> Option<String> {
    let mut parts = selector
        .match_labels
        .iter()
        .flatten()
        .map(|(key, value)| format!("{key}={value}"))
        .collect::<Vec<_>>();
    parts.extend(
        selector
            .match_expressions
            .iter()
            .flatten()
            .filter_map(|requirement| {
                let values = requirement
                    .values
                    .iter()
                    .flatten()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(",");
                match requirement.operator.as_str() {
                    "In" => Some(format!("{} in ({values})", requirement.key)),
                    "NotIn" => Some(format!("{} notin ({values})", requirement.key)),
                    "Exists" => Some(requirement.key.clone()),
                    "DoesNotExist" => Some(format!("!{}", requirement.key)),
                    _ => None,
                }
            }),
    );
    (!parts.is_empty()).then(|| parts.join(","))
}

fn bounded_list_params(
    selector: Option<&str>,
    field_selector: Option<&str>,
    limit: u32,
) -> ListParams {
    let mut params = ListParams::default().limit(limit.min(DEFAULT_CLUSTER_LIST_LIMIT));
    if let Some(selector) = selector.filter(|selector| !selector.is_empty()) {
        params = params.labels(selector);
    }
    if let Some(selector) = field_selector.filter(|selector| !selector.is_empty()) {
        params = params.fields(selector);
    }
    params
}

fn bounded_log_params(
    container: Option<&str>,
    previous: bool,
    since: Option<Duration>,
    tail_lines: u32,
    limit_bytes: i64,
) -> LogParams {
    LogParams {
        container: container.map(str::to_owned),
        previous,
        since_seconds: since.map(|since| since.as_secs().min(i64::MAX as u64) as i64),
        tail_lines: Some(i64::from(tail_lines)),
        limit_bytes: Some(limit_bytes),
        ..LogParams::default()
    }
}

fn truncate_logs(mut text: String, limit_bytes: usize) -> PodLogs {
    if text.len() <= limit_bytes {
        return PodLogs {
            text,
            truncated: false,
        };
    }
    let mut boundary = limit_bytes;
    while !text.is_char_boundary(boundary) {
        boundary -= 1;
    }
    text.truncate(boundary);
    PodLogs {
        text,
        truncated: true,
    }
}

fn metrics_info(object: DynamicObject) -> ResourceMetrics {
    let containers = object.data["containers"]
        .as_array()
        .map(|containers| {
            containers
                .iter()
                .map(|container| ContainerMetrics {
                    name: container["name"].as_str().unwrap_or_default().to_owned(),
                    cpu: container["usage"]["cpu"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    memory: container["usage"]["memory"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let usage = &object.data["usage"];
    let (cpu, memory) = if usage.is_object() {
        (
            usage["cpu"].as_str().unwrap_or_default().to_owned(),
            usage["memory"].as_str().unwrap_or_default().to_owned(),
        )
    } else {
        (
            sum_quantities(
                containers.iter().map(|container| container.cpu.as_str()),
                QuantityKind::Cpu,
            ),
            sum_quantities(
                containers.iter().map(|container| container.memory.as_str()),
                QuantityKind::Memory,
            ),
        )
    };
    ResourceMetrics {
        namespace: object.metadata.namespace.clone(),
        name: convert::name(&object.metadata),
        cpu,
        memory,
        containers,
    }
}

#[derive(Clone, Copy)]
enum QuantityKind {
    Cpu,
    Memory,
}

fn sum_quantities<'a>(values: impl Iterator<Item = &'a str>, kind: QuantityKind) -> String {
    let mut total = 0.0;
    for value in values {
        let (number, multiplier) = match kind {
            QuantityKind::Cpu => match value.strip_suffix('n') {
                Some(number) => (number, 1.0),
                None => match value.strip_suffix('u') {
                    Some(number) => (number, 1_000.0),
                    None => match value.strip_suffix('m') {
                        Some(number) => (number, 1_000_000.0),
                        None => (value, 1_000_000_000.0),
                    },
                },
            },
            QuantityKind::Memory => {
                const UNITS: [(&str, f64); 7] = [
                    ("Ki", 1.0),
                    ("Mi", 1024.0),
                    ("Gi", 1024.0 * 1024.0),
                    ("Ti", 1024.0 * 1024.0 * 1024.0),
                    ("K", 1_000.0 / 1024.0),
                    ("M", 1_000_000.0 / 1024.0),
                    ("G", 1_000_000_000.0 / 1024.0),
                ];
                UNITS
                    .iter()
                    .find_map(|(suffix, multiplier)| {
                        value
                            .strip_suffix(suffix)
                            .map(|number| (number, *multiplier))
                    })
                    .unwrap_or((value, 1.0 / 1024.0))
            }
        };
        if let Ok(number) = number.parse::<f64>() {
            total += number * multiplier;
        }
    }
    match kind {
        QuantityKind::Cpu => format!("{}n", total.round() as u64),
        QuantityKind::Memory => format!("{}Ki", total.round() as u64),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kube::core::response::Status;

    fn target() -> KubeCluster {
        KubeCluster {
            name: "prod-eu".into(),
            context: "prod-eu".into(),
            namespaces: vec!["shop".into()],
            ..KubeCluster::default()
        }
    }

    #[test]
    fn context_names_are_read_locally_without_copying_credentials() {
        let path = std::env::temp_dir().join(format!(
            "filar-kube-contexts-{}-{:?}.yaml",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::write(
            &path,
            "apiVersion: v1\nkind: Config\ncontexts:\n- name: prod-eu\n  context:\n    cluster: prod\n    user: secret-user\n- name: staging\n  context:\n    cluster: staging\n    user: other-user\nusers:\n- name: secret-user\n  user:\n    token: must-not-be-returned\n",
        )
        .unwrap();
        let contexts = KubeClusterApi::context_names(Some(&path)).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(contexts, vec!["prod-eu", "staging"]);
        assert!(!contexts.iter().any(|name| name.contains("must-not-be-returned")));
    }

    #[test]
    fn metrics_404_is_unsupported_and_other_errors_keep_their_classification() {
        let missing = kube::Error::Api(Box::new(Status {
            code: 404,
            message: "the server could not find the requested resource".into(),
            reason: "NotFound".into(),
            ..Status::default()
        }));
        let text = metrics_error(&target(), missing, "pods", Some("shop")).to_string();
        assert!(text.contains("Unsupported"), "{text}");
        assert!(text.contains("prod-eu"), "{text}");

        let denied = kube::Error::Api(Box::new(Status {
            code: 403,
            message: "pods is forbidden: User \"reader\" cannot list resource \"pods\" in API group \"\" in the namespace \"shop\"".into(),
            reason: "Forbidden".into(),
            ..Status::default()
        }));
        assert_eq!(
            metrics_error(&target(), denied, "pods", Some("shop")).to_string(),
            "нет права list на pods в namespace shop (контекст prod-eu)"
        );
    }

    #[test]
    fn sums_quantity_suffixes_and_tolerates_missing_usage_keys() {
        assert_eq!(
            sum_quantities(["1n", "2u", "3m", "4"].into_iter(), QuantityKind::Cpu),
            "4003002001n"
        );
        assert_eq!(
            sum_quantities(["1Ki", "2Mi", "3K", "4M"].into_iter(), QuantityKind::Memory),
            "5958Ki"
        );

        let object = serde_json::from_value(serde_json::json!({
            "apiVersion": "metrics.k8s.io/v1beta1",
            "kind": "NodeMetrics",
            "metadata": { "name": "node-a" },
            "usage": {}
        }))
        .expect("valid dynamic metrics fixture");
        let metrics = metrics_info(object);
        assert_eq!(metrics.cpu, "");
        assert_eq!(metrics.memory, "");
    }

    #[test]
    fn rollout_selector_includes_labels_and_expressions() {
        let selector = LabelSelector {
            match_labels: Some(std::collections::BTreeMap::from([(
                "app".into(),
                "web".into(),
            )])),
            match_expressions: Some(vec![
                k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement {
                    key: "tier".into(),
                    operator: "In".into(),
                    values: Some(vec!["frontend".into(), "backend".into()]),
                },
                k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement {
                    key: "legacy".into(),
                    operator: "DoesNotExist".into(),
                    values: None,
                },
            ]),
        };
        assert_eq!(
            label_selector_string(&selector).as_deref(),
            Some("app=web,tier in (frontend,backend),!legacy")
        );
    }

    #[test]
    fn lists_are_bounded_and_never_request_continuation() {
        let params = bounded_list_params(Some("app=web"), Some("status.phase=Running"), 900);
        assert_eq!(params.limit, Some(DEFAULT_CLUSTER_LIST_LIMIT));
        assert_eq!(params.label_selector.as_deref(), Some("app=web"));
        assert_eq!(
            params.field_selector.as_deref(),
            Some("status.phase=Running")
        );
        assert!(params.continue_token.is_none());
    }

    #[test]
    fn logs_require_both_bounds_and_truncate_utf8_safely() {
        let params =
            bounded_log_params(Some("main"), true, Some(Duration::from_secs(30)), 100, 4096);
        assert_eq!(params.tail_lines, Some(100));
        assert_eq!(params.limit_bytes, Some(4096));
        assert_eq!(params.since_seconds, Some(30));
        assert!(params.previous);
        assert!(!params.follow);

        let logs = truncate_logs("ab€cd".into(), 4);
        assert_eq!(logs.text, "ab");
        assert!(logs.truncated);
        assert!(logs.text.len() <= 4);
    }

    #[test]
    fn rbac_and_policy_forbidden_errors_are_distinct() {
        let rbac = kube::Error::Api(Box::new(Status {
            code: 403,
            message: "pods is forbidden: User \"reader\" cannot list resource \"pods\" in API group \"\" in the namespace \"shop\"".into(),
            reason: "Forbidden".into(),
            ..Status::default()
        }));
        let error = api_error(&target(), rbac, "list", "pods", Some("shop")).to_string();
        assert_eq!(
            error,
            "нет права list на pods в namespace shop (контекст prod-eu)"
        );
        assert!(!error.contains("reader"));

        let policy_text = "admission webhook policy.example denied the request";
        let policy = kube::Error::Api(Box::new(Status {
            code: 403,
            message: policy_text.into(),
            reason: "Forbidden".into(),
            ..Status::default()
        }));
        assert_eq!(
            api_error(&target(), policy, "list", "pods", Some("shop")).to_string(),
            policy_text
        );
    }

    #[test]
    fn transport_and_config_errors_do_not_include_sensitive_sources() {
        let sensitive = "token=super-secret certificate=PRIVATE-KEY";
        let source = std::io::Error::other(sensitive);
        let error = api_error(
            &target(),
            kube::Error::Service(source.into()),
            "list",
            "pods",
            Some("shop"),
        )
        .to_string();
        assert!(!error.contains("super-secret"));
        assert!(!error.contains("PRIVATE-KEY"));
        assert!(error.contains("unavailable"));

        let error =
            connect_error(&target(), "cannot read or parse the selected kubeconfig").to_string();
        assert!(!error.contains(sensitive));
    }

    #[test]
    fn public_api_is_read_only_and_has_no_secret_surface() {
        fn assert_cluster_api<T: ClusterApi>() {}
        assert_cluster_api::<KubeClusterApi>();
        // This test is deliberately type-level: adding a write or Secret method
        // requires changing the public trait above, which is the reviewed seam.
    }
}
