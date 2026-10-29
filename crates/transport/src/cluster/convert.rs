use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, ReplicaSet, StatefulSet};
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{ConfigMap, Endpoints, Event, Namespace, Node, Pod, Service};
use k8s_openapi::api::networking::v1::Ingress;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{ObjectMeta, Time};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use super::{
    ConfigMapInfo, ContainerInfo, EndpointInfo, EventInfo, IngressInfo, NamespaceInfo,
    NodeCondition, NodeInfo, PodInfo, RolloutRevision, ServiceInfo, ServicePortInfo,
    TerminationInfo, WorkloadInfo, WorkloadKind,
};

pub(super) fn name(meta: &ObjectMeta) -> String {
    meta.name.clone().unwrap_or_default()
}

pub(super) fn namespace(meta: &ObjectMeta) -> String {
    meta.namespace.clone().unwrap_or_default()
}

fn timestamp(time: &Time) -> String {
    time.0.to_string()
}

fn age_seconds(meta: &ObjectMeta) -> Option<u64> {
    let created = meta.creation_timestamp.as_ref()?.0.as_second();
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    Some(now.saturating_sub(created).max(0) as u64)
}

fn creation_timestamp(meta: &ObjectMeta) -> Option<String> {
    meta.creation_timestamp.as_ref().map(timestamp)
}

pub(super) fn pod_info(pod: Pod) -> PodInfo {
    let status = pod.status.as_ref();
    let conditions = status.and_then(|s| s.conditions.as_deref()).unwrap_or(&[]);
    let ready = conditions
        .iter()
        .any(|condition| condition.type_ == "Ready" && condition.status == "True");
    let container_statuses = status
        .and_then(|s| s.container_statuses.as_deref())
        .unwrap_or(&[]);
    let ready_containers = container_statuses
        .iter()
        .filter(|status| status.ready)
        .count() as u32;
    let restarts = container_statuses
        .iter()
        .map(|status| status.restart_count.max(0) as u32)
        .sum();
    let last_termination = container_statuses
        .iter()
        .filter_map(|status| {
            let terminated = status.last_state.as_ref()?.terminated.as_ref()?;
            let finished = terminated.finished_at.as_ref();
            let sort_key = finished.map(|time| time.0.as_second()).unwrap_or(i64::MIN);
            Some((
                sort_key,
                TerminationInfo {
                    container: status.name.clone(),
                    reason: terminated.reason.clone(),
                    exit_code: Some(terminated.exit_code),
                    finished_at: finished.map(timestamp),
                },
            ))
        })
        .max_by_key(|(sort_key, _)| *sort_key)
        .map(|(_, termination)| termination);
    let containers: Vec<ContainerInfo> = pod
        .spec
        .as_ref()
        .map(|spec| {
            spec.containers
                .iter()
                .map(|container| ContainerInfo {
                    name: container.name.clone(),
                    image: container.image.clone().unwrap_or_default(),
                })
                .collect()
        })
        .unwrap_or_default();

    PodInfo {
        namespace: namespace(&pod.metadata),
        name: name(&pod.metadata),
        phase: status
            .and_then(|status| status.phase.clone())
            .unwrap_or_else(|| "Unknown".into()),
        ready,
        ready_containers,
        container_count: containers.len() as u32,
        restarts,
        last_termination,
        node: pod.spec.as_ref().and_then(|spec| spec.node_name.clone()),
        age_seconds: age_seconds(&pod.metadata),
        containers,
    }
}

pub(super) fn event_info(event: Event) -> EventInfo {
    EventInfo {
        namespace: namespace(&event.metadata),
        name: name(&event.metadata),
        type_: event.type_.unwrap_or_default(),
        reason: event.reason.unwrap_or_default(),
        message: event.message.unwrap_or_default(),
        involved_kind: event.involved_object.kind,
        involved_name: event.involved_object.name,
        count: event.count.unwrap_or(0),
        last_timestamp: event
            .last_timestamp
            .as_ref()
            .map(timestamp)
            .or_else(|| event.event_time.as_ref().map(|time| time.0.to_string())),
    }
}

fn base_workload(
    meta: &ObjectMeta,
    kind: WorkloadKind,
    desired: Option<i32>,
    ready: Option<i32>,
    updated: Option<i32>,
    available: Option<i32>,
    images: Vec<String>,
) -> WorkloadInfo {
    WorkloadInfo {
        namespace: namespace(meta),
        name: name(meta),
        kind,
        desired,
        ready,
        updated,
        available,
        age_seconds: age_seconds(meta),
        images,
    }
}

fn pod_template_images(template: &k8s_openapi::api::core::v1::PodTemplateSpec) -> Vec<String> {
    template
        .spec
        .as_ref()
        .map(|spec| {
            spec.containers
                .iter()
                .filter_map(|container| container.image.clone())
                .collect()
        })
        .unwrap_or_default()
}

pub(super) fn deployment_info(workload: Deployment) -> WorkloadInfo {
    let images = workload
        .spec
        .as_ref()
        .map(|spec| pod_template_images(&spec.template))
        .unwrap_or_default();
    base_workload(
        &workload.metadata,
        WorkloadKind::Deployment,
        workload.spec.as_ref().and_then(|spec| spec.replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.ready_replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.updated_replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.available_replicas),
        images,
    )
}

pub(super) fn statefulset_info(workload: StatefulSet) -> WorkloadInfo {
    let images = workload
        .spec
        .as_ref()
        .map(|spec| pod_template_images(&spec.template))
        .unwrap_or_default();
    base_workload(
        &workload.metadata,
        WorkloadKind::StatefulSet,
        workload.spec.as_ref().and_then(|spec| spec.replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.ready_replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.updated_replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.available_replicas),
        images,
    )
}

pub(super) fn daemonset_info(workload: DaemonSet) -> WorkloadInfo {
    let images = workload
        .spec
        .as_ref()
        .map(|spec| pod_template_images(&spec.template))
        .unwrap_or_default();
    base_workload(
        &workload.metadata,
        WorkloadKind::DaemonSet,
        workload
            .status
            .as_ref()
            .map(|status| status.desired_number_scheduled),
        workload.status.as_ref().map(|status| status.number_ready),
        workload
            .status
            .as_ref()
            .and_then(|status| status.updated_number_scheduled),
        workload
            .status
            .as_ref()
            .and_then(|status| status.number_available),
        images,
    )
}

pub(super) fn replicaset_info(workload: ReplicaSet) -> WorkloadInfo {
    let images = workload
        .spec
        .as_ref()
        .and_then(|spec| spec.template.as_ref())
        .map(pod_template_images)
        .unwrap_or_default();
    base_workload(
        &workload.metadata,
        WorkloadKind::ReplicaSet,
        workload.spec.as_ref().and_then(|spec| spec.replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.ready_replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.fully_labeled_replicas),
        workload
            .status
            .as_ref()
            .and_then(|status| status.available_replicas),
        images,
    )
}

pub(super) fn job_info(workload: Job) -> WorkloadInfo {
    let images = workload
        .spec
        .as_ref()
        .map(|spec| pod_template_images(&spec.template))
        .unwrap_or_default();
    base_workload(
        &workload.metadata,
        WorkloadKind::Job,
        workload.spec.as_ref().and_then(|spec| spec.parallelism),
        workload.status.as_ref().and_then(|status| status.ready),
        workload.status.as_ref().and_then(|status| status.succeeded),
        None,
        images,
    )
}

pub(super) fn cronjob_info(workload: CronJob) -> WorkloadInfo {
    let images = workload
        .spec
        .as_ref()
        .and_then(|spec| spec.job_template.spec.as_ref())
        .map(|spec| pod_template_images(&spec.template))
        .unwrap_or_default();
    base_workload(
        &workload.metadata,
        WorkloadKind::CronJob,
        None,
        workload
            .status
            .as_ref()
            .and_then(|status| status.active.as_ref())
            .map(|active| active.len() as i32),
        None,
        None,
        images,
    )
}

pub(super) fn rollout_revision(replicaset: ReplicaSet) -> RolloutRevision {
    let revision = replicaset
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get("deployment.kubernetes.io/revision"))
        .and_then(|revision| revision.parse().ok())
        .unwrap_or(0);
    RolloutRevision {
        revision,
        name: name(&replicaset.metadata),
        desired: replicaset.spec.as_ref().and_then(|spec| spec.replicas),
        ready: replicaset
            .status
            .as_ref()
            .and_then(|status| status.ready_replicas),
        created_at: creation_timestamp(&replicaset.metadata),
        change_cause: replicaset
            .metadata
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get("kubernetes.io/change-cause"))
            .cloned(),
    }
}

pub(super) fn node_info(node: Node) -> NodeInfo {
    let conditions = node
        .status
        .as_ref()
        .and_then(|status| status.conditions.as_ref())
        .map(|conditions| {
            conditions
                .iter()
                .map(|condition| NodeCondition {
                    type_: condition.type_.clone(),
                    status: condition.status.clone(),
                    reason: condition.reason.clone(),
                    message: condition.message.clone(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let ready = conditions
        .iter()
        .any(|condition| condition.type_ == "Ready" && condition.status == "True");
    let allocatable = node
        .status
        .as_ref()
        .and_then(|status| status.allocatable.as_ref())
        .map(|resources| {
            resources
                .iter()
                .map(|(name, quantity)| (name.clone(), quantity.0.clone()))
                .collect()
        })
        .unwrap_or_else(BTreeMap::new);
    NodeInfo {
        name: name(&node.metadata),
        ready,
        conditions,
        allocatable,
        age_seconds: age_seconds(&node.metadata),
    }
}

pub(super) fn service_info(service: Service) -> ServiceInfo {
    let spec = service.spec.as_ref();
    ServiceInfo {
        namespace: namespace(&service.metadata),
        name: name(&service.metadata),
        type_: spec
            .and_then(|spec| spec.type_.clone())
            .unwrap_or_else(|| "ClusterIP".into()),
        cluster_ip: spec.and_then(|spec| spec.cluster_ip.clone()),
        external_ips: spec
            .and_then(|spec| spec.external_ips.clone())
            .unwrap_or_default(),
        ports: spec
            .and_then(|spec| spec.ports.as_ref())
            .map(|ports| {
                ports
                    .iter()
                    .map(|port| ServicePortInfo {
                        name: port.name.clone(),
                        protocol: port.protocol.clone().unwrap_or_else(|| "TCP".into()),
                        port: port.port,
                        target_port: port.target_port.as_ref().map(|target| match target {
                            IntOrString::Int(port) => port.to_string(),
                            IntOrString::String(name) => name.clone(),
                        }),
                        node_port: port.node_port,
                    })
                    .collect()
            })
            .unwrap_or_default(),
        selector: spec
            .and_then(|spec| spec.selector.clone())
            .unwrap_or_default(),
        age_seconds: age_seconds(&service.metadata),
    }
}

pub(super) fn endpoint_info(endpoints: Endpoints) -> EndpointInfo {
    let mut addresses = Vec::new();
    let mut not_ready_addresses = Vec::new();
    let mut ports = Vec::new();
    for subset in endpoints.subsets.unwrap_or_default() {
        addresses.extend(
            subset
                .addresses
                .unwrap_or_default()
                .into_iter()
                .map(|address| address.ip),
        );
        not_ready_addresses.extend(
            subset
                .not_ready_addresses
                .unwrap_or_default()
                .into_iter()
                .map(|address| address.ip),
        );
        ports.extend(subset.ports.unwrap_or_default().into_iter().map(|port| {
            format!(
                "{}:{}/{}",
                port.name.as_deref().unwrap_or("-"),
                port.port,
                port.protocol.as_deref().unwrap_or("TCP")
            )
        }));
    }
    EndpointInfo {
        namespace: namespace(&endpoints.metadata),
        name: name(&endpoints.metadata),
        addresses,
        not_ready_addresses,
        ports,
    }
}

pub(super) fn ingress_info(ingress: Ingress) -> IngressInfo {
    let hosts = ingress
        .spec
        .as_ref()
        .and_then(|spec| spec.rules.as_ref())
        .map(|rules| rules.iter().filter_map(|rule| rule.host.clone()).collect())
        .unwrap_or_default();
    let addresses = ingress
        .status
        .as_ref()
        .and_then(|status| status.load_balancer.as_ref())
        .and_then(|balancer| balancer.ingress.as_ref())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.ip.clone().or_else(|| entry.hostname.clone()))
                .collect()
        })
        .unwrap_or_default();
    IngressInfo {
        namespace: namespace(&ingress.metadata),
        name: name(&ingress.metadata),
        class_name: ingress
            .spec
            .as_ref()
            .and_then(|spec| spec.ingress_class_name.clone()),
        hosts,
        addresses,
        age_seconds: age_seconds(&ingress.metadata),
    }
}

pub(super) fn configmap_info(configmap: ConfigMap) -> ConfigMapInfo {
    ConfigMapInfo {
        namespace: namespace(&configmap.metadata),
        name: name(&configmap.metadata),
        data: configmap.data.unwrap_or_default(),
        binary_data_keys: configmap
            .binary_data
            .map(|data| data.keys().cloned().collect())
            .unwrap_or_default(),
        age_seconds: age_seconds(&configmap.metadata),
    }
}

pub(super) fn namespace_info(namespace: Namespace) -> NamespaceInfo {
    NamespaceInfo {
        name: name(&namespace.metadata),
        phase: namespace
            .status
            .and_then(|status| status.phase)
            .unwrap_or_else(|| "Unknown".into()),
        age_seconds: age_seconds(&namespace.metadata),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pod_conversion_reports_restarts_and_latest_termination() {
        let pod: Pod = serde_json::from_value(json!({
            "metadata": {"name": "web-1", "namespace": "shop"},
            "spec": {
                "nodeName": "node-a",
                "containers": [
                    {"name": "main", "image": "example/web:1"},
                    {"name": "sidecar", "image": "example/side:2"}
                ]
            },
            "status": {
                "phase": "Running",
                "conditions": [{"type": "Ready", "status": "True"}],
                "containerStatuses": [
                    {
                        "name": "main", "image": "example/web:1", "imageID": "",
                        "ready": true, "restartCount": 3,
                        "lastState": {"terminated": {
                            "exitCode": 137, "reason": "OOMKilled",
                            "finishedAt": "2026-01-01T00:00:02Z"
                        }}
                    },
                    {
                        "name": "sidecar", "image": "example/side:2", "imageID": "",
                        "ready": false, "restartCount": 2,
                        "lastState": {"terminated": {
                            "exitCode": 1, "reason": "Error",
                            "finishedAt": "2026-01-01T00:00:01Z"
                        }}
                    }
                ]
            }
        }))
        .unwrap();

        let info = pod_info(pod);
        assert!(info.ready);
        assert_eq!(info.ready_containers, 1);
        assert_eq!(info.container_count, 2);
        assert_eq!(info.restarts, 5);
        assert_eq!(info.node.as_deref(), Some("node-a"));
        assert_eq!(info.containers[0].image, "example/web:1");
        let termination = info.last_termination.unwrap();
        assert_eq!(termination.container, "main");
        assert_eq!(termination.reason.as_deref(), Some("OOMKilled"));
        assert_eq!(termination.exit_code, Some(137));
    }

    #[test]
    fn pod_without_status_is_unknown_and_not_ready() {
        let pod: Pod = serde_json::from_value(json!({
            "metadata": {"name": "pending", "namespace": "shop"},
            "spec": {"containers": [{"name": "main", "image": "busybox"}]}
        }))
        .unwrap();
        let info = pod_info(pod);
        assert_eq!(info.phase, "Unknown");
        assert!(!info.ready);
        assert_eq!(info.restarts, 0);
        assert!(info.last_termination.is_none());
    }
}
