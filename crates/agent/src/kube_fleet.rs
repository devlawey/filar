//! Read-only fleets over typed Kubernetes API calls (#568).
//!
//! Composition is discovered once and frozen in [`FleetOperation`]. Every
//! operation reopens that snapshot, binds the same typed call to each member,
//! redacts each answer, and exposes only the existing difference fold to the
//! model. No shell command or `CommandExecutor` adapter is involved.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use filar_core::{
    CoreError, FleetCheck, FleetOperation, FleetTarget, HostGroup, KubeCluster, KubePodRef, KubeScopeRef, OperationId,
    Result, SecretProvider,
};
use filar_transport::{ClusterApi, KubeClusterApi, PodInfo};
use futures::stream::{self, FuturesUnordered, StreamExt};
use tokio_util::sync::CancellationToken;

use crate::fleet_fold::{fold, FoldedTable};
use crate::fleet_gate::{radius_line, ApprovalScope, FleetGate};
use crate::fleet_result::OperationResult;
use crate::fleet_run::{FleetRunReport, HostRun};
use crate::kube_tools::{bind_fleet_call, KubeToolCall, KubeToolExecutor};
use crate::preprocess::PreprocessorRegistry;

/// A configured cluster and the read-only API/redaction sources used for it.
#[derive(Clone)]
pub struct KubeFleetCluster {
    target: KubeCluster,
    api: Arc<dyn ClusterApi>,
    secrets: Arc<dyn SecretProvider>,
}

impl KubeFleetCluster {
    /// Bind an already connected read-only API to its configured target.
    pub fn new(target: KubeCluster, api: Arc<dyn ClusterApi>, secrets: Arc<dyn SecretProvider>) -> Self {
        Self { target, api, secrets }
    }

    /// Connect a configured target through the standard read-only transport.
    pub async fn connect(target: KubeCluster, secrets: Arc<dyn SecretProvider>) -> Result<Self> {
        let api = KubeClusterApi::connect(&target).await?;
        Ok(Self::new(target, Arc::new(api), secrets))
    }
}

/// A frozen Kubernetes fleet and the cluster APIs needed to question it.
pub struct KubeFleet {
    fleet: FleetOperation,
    clusters: BTreeMap<String, KubeFleetCluster>,
}

impl KubeFleet {
    /// Discover and freeze the group's members.
    ///
    /// A selector freezes ready pods in cluster configuration order. Without a
    /// selector, each configured cluster/namespace scope is one member and no
    /// discovery request is needed.
    pub async fn open(group: &HostGroup, clusters: Vec<KubeFleetCluster>) -> Result<Self> {
        group.validate()?;
        let kube = group.kube.as_ref().ok_or_else(|| {
            CoreError::Config(format!(
                "host_groups: \"{}\": Kubernetes fleet requires kube selection",
                group.name
            ))
        })?;
        let mut selected = Vec::with_capacity(kube.clusters.len());
        for name in &kube.clusters {
            let cluster = clusters
                .iter()
                .find(|cluster| &cluster.target.name == name)
                .cloned()
                .ok_or_else(|| {
                    CoreError::Config(format!(
                        "host_groups: \"{}\": unknown Kubernetes cluster \"{name}\"",
                        group.name
                    ))
                })?;
            selected.push(cluster);
        }

        let targets = if kube.selector.is_some() {
            discover_targets(group, &selected).await?
        } else {
            selected
                .iter()
                .map(|cluster| {
                    FleetTarget::KubeCluster(KubeScopeRef {
                        cluster: cluster.target.name.clone(),
                        namespace: kube.namespace.clone(),
                    })
                })
                .collect()
        };
        let clusters = selected
            .into_iter()
            .map(|cluster| (cluster.target.name.clone(), cluster))
            .collect();
        Ok(Self {
            fleet: FleetOperation::from_targets(group, &targets),
            clusters,
        })
    }

    /// The composition frozen at fleet entry.
    pub fn fleet(&self) -> &FleetOperation {
        &self.fleet
    }

    /// Scoped member labels shown in the single fleet-wide confirmation.
    pub fn radius(&self) -> Vec<String> {
        self.fleet.members().iter().map(|member| member.name()).collect()
    }

    /// Human-readable radius for confirmation dialogs and audit output.
    pub fn confirmation_radius(&self) -> String {
        radius_line(&self.radius())
    }

    /// Validate a call for every member and require exactly one confirmation.
    ///
    /// There is intentionally no confirmation-mode argument: a fleet always
    /// asks once, including modes where a single cluster tab may auto-approve.
    pub fn gate(&self, call: &KubeToolCall) -> FleetGate {
        for member in self.fleet.members() {
            if let Err(error) = bind_fleet_call(call, member.target()) {
                return FleetGate::Refuse(format!("Error: {error}"));
            }
        }
        FleetGate::Ask(ApprovalScope::Once)
    }

    /// Run one typed call over the frozen composition.
    pub async fn run(&self, call: &KubeToolCall) -> Result<KubeFleetReport> {
        self.run_until(call, &CancellationToken::new()).await
    }

    /// [`run`](Self::run) with local cancellation of queued and running reads.
    pub async fn run_until(&self, call: &KubeToolCall, cancel: &CancellationToken) -> Result<KubeFleetReport> {
        let mut op = self.fleet.reopen();
        // Reject an inapplicable call before any cluster is contacted.
        for member in op.members() {
            bind_fleet_call(call, member.target())?;
        }

        let mut tasks = Vec::with_capacity(op.len());
        for (handle, member) in op.handles().zip(op.members()) {
            let target = member.target().clone();
            let cluster_name = match &target {
                FleetTarget::KubePod(pod) => &pod.cluster,
                FleetTarget::KubeCluster(scope) => &scope.cluster,
                FleetTarget::Ssh(_) => {
                    return Err(CoreError::Other("SSH member found in a Kubernetes fleet".into()));
                }
            };
            let cluster = self
                .clusters
                .get(cluster_name)
                .cloned()
                .ok_or_else(|| CoreError::Other(format!("no API for Kubernetes cluster {cluster_name}")))?;
            tasks.push((handle, KubeTask { target, cluster }));
        }

        let deadline = op.per_host_timeout();
        let limit = op.max_parallel().max(1) as usize;
        let handles: Vec<_> = tasks.iter().map(|(handle, _)| *handle).collect();
        let mut queue = tasks.into_iter().enumerate();
        let mut in_flight = FuturesUnordered::new();
        let mut finished = Vec::with_capacity(handles.len());

        loop {
            while in_flight.len() < limit && !cancel.is_cancelled() {
                match queue.next() {
                    Some((index, (_, task))) => {
                        op.set_progress(handles[index], filar_core::HostProgress::Running);
                        in_flight.push(run_one(index, task, call.clone(), deadline, cancel.clone()));
                    }
                    None => break,
                }
            }
            match in_flight.next().await {
                Some((index, run)) => {
                    op.set_progress(handles[index], filar_core::HostProgress::Done);
                    finished.push((index, run));
                }
                None => break,
            }
        }
        for (index, _) in queue {
            op.set_progress(handles[index], filar_core::HostProgress::Done);
            finished.push((index, HostRun::Cancelled));
        }
        finished.sort_by_key(|(index, _)| *index);
        let runs = finished.into_iter().map(|(index, run)| (handles[index], run)).collect();
        let run = FleetRunReport::from_runs(op.id(), runs);
        let mut result = OperationResult::build(&op, &run, &[])?;
        let check = FleetCheck::ad_hoc(call.tool_name()).map_err(CoreError::Other)?;
        let folded = fold(&check, &[], &run, &mut result, &PreprocessorRegistry::new())?;
        let (new_pods, notices) = self.discover_changes(&op).await;
        Ok(KubeFleetReport {
            operation: op.id(),
            result,
            run,
            folded,
            new_pods,
            notices,
        })
    }

    async fn discover_changes(&self, op: &FleetOperation) -> (Vec<String>, Vec<String>) {
        if op
            .group()
            .kube
            .as_ref()
            .and_then(|kube| kube.selector.as_ref())
            .is_none()
        {
            return (Vec::new(), Vec::new());
        }
        let clusters: Vec<_> = op
            .group()
            .kube
            .as_ref()
            .map(|kube| {
                kube.clusters
                    .iter()
                    .filter_map(|name| self.clusters.get(name).cloned())
                    .collect()
            })
            .unwrap_or_default();
        let existing: BTreeSet<String> = op
            .members()
            .iter()
            .filter_map(|member| match member.target() {
                FleetTarget::KubePod(pod) => Some(pod_identity(pod)),
                _ => None,
            })
            .collect();
        match discover_targets(op.group(), &clusters).await {
            Ok(current) => {
                let mut new_pods: Vec<_> = current
                    .into_iter()
                    .filter_map(|target| match target {
                        FleetTarget::KubePod(pod) if !existing.contains(&pod_identity(&pod)) => {
                            Some(FleetTarget::KubePod(pod).name())
                        }
                        _ => None,
                    })
                    .collect();
                new_pods.sort();
                (new_pods, Vec::new())
            }
            Err(error) => (Vec::new(), vec![format!("не удалось проверить новые поды: {error}")]),
        }
    }
}

struct KubeTask {
    target: FleetTarget,
    cluster: KubeFleetCluster,
}

async fn run_one(
    index: usize,
    task: KubeTask,
    call: KubeToolCall,
    deadline: Duration,
    cancel: CancellationToken,
) -> (usize, HostRun) {
    let execute = async {
        let executor = KubeToolExecutor::new(
            Arc::clone(&task.cluster.api),
            task.cluster.target.clone(),
            Arc::clone(&task.cluster.secrets),
        );
        executor.execute_for_fleet(&call, &task.target).await
    };
    let run = tokio::select! {
        biased;
        _ = cancel.cancelled() => HostRun::Cancelled,
        result = async {
            match tokio::time::timeout(deadline, execute).await {
                Err(_) => HostRun::TimedOut(deadline),
                Ok(Ok(answer)) => HostRun::AnsweredText(answer.content),
                Ok(Err(error)) => HostRun::Failed(classify_error(error, &task.target)),
            }
        } => result,
    };
    (index, run)
}

fn classify_error(error: CoreError, target: &FleetTarget) -> CoreError {
    match error {
        CoreError::NotFound(message) if !matches!(target, FleetTarget::KubePod(_)) => CoreError::Other(message),
        other => other,
    }
}

async fn discover_targets(group: &HostGroup, clusters: &[KubeFleetCluster]) -> Result<Vec<FleetTarget>> {
    let kube = group
        .kube
        .as_ref()
        .ok_or_else(|| CoreError::Config(format!("host group {} has no Kubernetes selection", group.name)))?;
    let namespace = kube.namespace.clone();
    let selector = kube.selector.clone();
    let limit = group.max_parallel.max(1) as usize;
    let discovered: Vec<Result<(String, Vec<PodInfo>)>> = stream::iter(clusters.iter().cloned().map(|cluster| {
        let namespace = namespace.clone();
        let selector = selector.clone();
        async move {
            let pods = cluster
                .api
                .list_pods(Some(&namespace), selector.as_deref(), None)
                .await?;
            Ok((cluster.target.name.clone(), pods))
        }
    }))
    .buffered(limit)
    .collect()
    .await;

    let mut targets = Vec::new();
    for discovery in discovered {
        let (cluster, mut pods) = discovery?;
        pods.retain(|pod| pod.ready);
        pods.sort_by(|left, right| (&left.namespace, &left.name).cmp(&(&right.namespace, &right.name)));
        for pod in pods {
            let container = pod
                .containers
                .first()
                .map(|container| container.name.clone())
                .ok_or_else(|| {
                    CoreError::Other(format!(
                        "ready pod {cluster}/{}/{} has no regular container",
                        pod.namespace, pod.name
                    ))
                })?;
            targets.push(FleetTarget::KubePod(KubePodRef {
                cluster: cluster.clone(),
                namespace: pod.namespace,
                pod: pod.name,
                container,
            }));
        }
    }
    Ok(targets)
}

fn pod_identity(pod: &KubePodRef) -> String {
    format!("{}/{}/{}", pod.cluster, pod.namespace, pod.pod)
}

/// One completed Kubernetes fleet operation and its model-safe fold.
pub struct KubeFleetReport {
    operation: OperationId,
    result: OperationResult,
    run: FleetRunReport,
    folded: FoldedTable,
    new_pods: Vec<String>,
    notices: Vec<String>,
}

impl KubeFleetReport {
    /// The fresh operation ID used for this call.
    pub fn operation(&self) -> OperationId {
        self.operation
    }

    /// Typed outcome of every frozen member.
    pub fn result(&self) -> &OperationResult {
        &self.result
    }

    /// Redacted member answers and transport outcomes, for UI observers only.
    pub fn run(&self) -> &FleetRunReport {
        &self.run
    }

    /// The existing fleet difference table.
    pub fn folded(&self) -> &FoldedTable {
        &self.folded
    }

    /// Ready pods discovered after the operation but absent from its snapshot.
    pub fn new_pods(&self) -> &[String] {
        &self.new_pods
    }

    /// Render only the summary, fold and composition notices for the model.
    pub fn render(&self) -> String {
        let mut output = self.result.summary().headline();
        output.push('\n');
        output.push_str(&self.folded.render());
        for notice in &self.notices {
            output.push_str(notice);
            output.push('\n');
        }
        if !self.new_pods.is_empty() {
            output.push_str(&format!(
                "появились поды, не вошедшие во флот: {}\n",
                self.new_pods.join(", ")
            ));
        }
        output
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use async_trait::async_trait;
    use filar_core::{KubeGroup, StaticSecretProvider};
    use filar_transport::{
        ConfigMapInfo, ContainerInfo, EndpointInfo, EventInfo, IngressInfo, NamespaceInfo, NodeInfo, PodLogs,
        ResourceMetrics, RolloutRevision, ServiceInfo, WorkloadInfo, WorkloadKind,
    };

    use crate::fleet_result::HostState;
    use crate::kube_tools::{KubeResourceKind, KubeTopResource};

    use super::*;

    #[derive(Clone, Copy, Default)]
    enum Behaviour {
        #[default]
        Answer,
        Denied,
        Gone,
        Unavailable,
        Hang,
    }

    #[derive(Default)]
    struct MockCluster {
        pods: Mutex<Vec<PodInfo>>,
        behaviour: Mutex<BTreeMap<String, Behaviour>>,
        logs: Mutex<BTreeMap<String, PodLogs>>,
        delay: Mutex<Duration>,
        active: AtomicUsize,
        max_active: AtomicUsize,
        log_calls: Mutex<Vec<(String, String, Option<String>, bool)>>,
    }

    struct Active<'a>(&'a AtomicUsize);

    impl Drop for Active<'_> {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::SeqCst);
        }
    }

    impl MockCluster {
        fn enter(&self) -> Active<'_> {
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            Active(&self.active)
        }
    }

    #[async_trait]
    impl ClusterApi for MockCluster {
        async fn list_pods(&self, _: Option<&str>, _: Option<&str>, _: Option<&str>) -> Result<Vec<PodInfo>> {
            Ok(self.pods.lock().unwrap_or_else(|p| p.into_inner()).clone())
        }

        async fn get_pod(&self, _: &str, name: &str) -> Result<PodInfo> {
            let _active = self.enter();
            let delay = *self.delay.lock().unwrap_or_else(|p| p.into_inner());
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            let behaviour = self
                .behaviour
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(name)
                .copied()
                .unwrap_or_default();
            match behaviour {
                Behaviour::Answer => self
                    .pods
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .iter()
                    .find(|pod| pod.name == name)
                    .cloned()
                    .ok_or_else(|| CoreError::NotFound(format!("pod {name} disappeared"))),
                Behaviour::Denied => Err(CoreError::AccessDenied("RBAC denied".into())),
                Behaviour::Gone => Err(CoreError::NotFound("pod disappeared".into())),
                Behaviour::Unavailable => Err(CoreError::Unavailable("API unavailable".into())),
                Behaviour::Hang => {
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                    Err(CoreError::Unavailable("request timed out".into()))
                }
            }
        }

        async fn list_events(&self, _: Option<&str>, _: Option<&str>) -> Result<Vec<EventInfo>> {
            Ok(Vec::new())
        }

        async fn pod_logs(
            &self,
            namespace: &str,
            pod: &str,
            container: Option<&str>,
            previous: bool,
            _: Option<Duration>,
            _: u32,
            _: usize,
        ) -> Result<PodLogs> {
            self.log_calls.lock().unwrap_or_else(|p| p.into_inner()).push((
                namespace.into(),
                pod.into(),
                container.map(str::to_owned),
                previous,
            ));
            Ok(self
                .logs
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .get(pod)
                .cloned()
                .unwrap_or(PodLogs {
                    text: "ok".into(),
                    truncated: false,
                }))
        }

        async fn list_workloads(&self, _: Option<&str>, _: WorkloadKind) -> Result<Vec<WorkloadInfo>> {
            Ok(Vec::new())
        }

        async fn rollout_history(&self, _: &str, _: &str) -> Result<Vec<RolloutRevision>> {
            Ok(Vec::new())
        }

        async fn list_nodes(&self) -> Result<Vec<NodeInfo>> {
            Ok(Vec::new())
        }

        async fn top_pods(&self, _: Option<&str>) -> Result<Vec<ResourceMetrics>> {
            Ok(Vec::new())
        }

        async fn top_nodes(&self) -> Result<Vec<ResourceMetrics>> {
            Ok(Vec::new())
        }

        async fn list_services(&self, _: Option<&str>) -> Result<Vec<ServiceInfo>> {
            Ok(Vec::new())
        }

        async fn list_endpoints(&self, _: Option<&str>) -> Result<Vec<EndpointInfo>> {
            Ok(Vec::new())
        }

        async fn list_ingresses(&self, _: Option<&str>) -> Result<Vec<IngressInfo>> {
            Ok(Vec::new())
        }

        async fn list_configmaps(&self, _: Option<&str>) -> Result<Vec<ConfigMapInfo>> {
            Ok(Vec::new())
        }

        async fn namespaces(&self) -> Result<Vec<NamespaceInfo>> {
            Ok(Vec::new())
        }
    }

    fn pod(name: &str) -> PodInfo {
        PodInfo {
            namespace: "shop".into(),
            name: name.into(),
            phase: "Running".into(),
            ready: true,
            ready_containers: 1,
            container_count: 1,
            restarts: 0,
            last_termination: None,
            node: None,
            age_seconds: None,
            containers: vec![ContainerInfo {
                name: "main".into(),
                image: "example/web:1".into(),
                resource_requests: BTreeMap::new(),
                resource_limits: BTreeMap::new(),
            }],
        }
    }

    fn cluster(name: &str, pods: Vec<PodInfo>) -> (KubeFleetCluster, Arc<MockCluster>) {
        let mock = Arc::new(MockCluster {
            pods: Mutex::new(pods),
            ..MockCluster::default()
        });
        let target = KubeCluster {
            name: name.into(),
            context: format!("{name}-context"),
            ..KubeCluster::default()
        };
        let secrets = Arc::new(StaticSecretProvider::new());
        (KubeFleetCluster::new(target, mock.clone(), secrets), mock)
    }

    fn group(clusters: &[&str], selector: Option<&str>) -> HostGroup {
        HostGroup {
            name: "web-prod".into(),
            kube: Some(KubeGroup {
                clusters: clusters.iter().map(|name| (*name).into()).collect(),
                namespace: "shop".into(),
                selector: selector.map(str::to_owned),
            }),
            max_parallel: 2,
            per_host_timeout_secs: 30,
            ..HostGroup::default()
        }
    }

    fn describe() -> KubeToolCall {
        KubeToolCall::Describe {
            kind: KubeResourceKind::Pod,
            namespace: None,
            name: "ignored".into(),
            event_limit: 20,
        }
    }

    fn logs() -> KubeToolCall {
        KubeToolCall::Logs {
            namespace: "other".into(),
            pod: "ignored".into(),
            container: Some("other".into()),
            previous: true,
            since: None,
            tail: 100,
            limit_bytes: 4096,
        }
    }

    #[tokio::test]
    async fn composition_freezes_ready_pods_across_clusters_or_cluster_scopes() {
        let (eu, _) = cluster(
            "prod-eu",
            vec![pod("web-b"), pod("web-a"), {
                let mut pending = pod("pending");
                pending.ready = false;
                pending
            }],
        );
        let (us, _) = cluster("prod-us", vec![pod("web-c")]);
        let fleet = KubeFleet::open(
            &group(&["prod-eu", "prod-us"], Some("app=web")),
            vec![eu.clone(), us.clone()],
        )
        .await
        .expect("pod fleet opens");
        assert_eq!(
            fleet.radius(),
            [
                "prod-eu/shop/web-a/main",
                "prod-eu/shop/web-b/main",
                "prod-us/shop/web-c/main"
            ]
        );

        let scopes = KubeFleet::open(&group(&["prod-eu", "prod-us"], None), vec![eu, us])
            .await
            .expect("cluster fleet opens");
        assert_eq!(scopes.radius(), ["prod-eu/shop", "prod-us/shop"]);
    }

    #[tokio::test]
    async fn outcomes_keep_denial_disappearance_unavailability_and_timeout_distinct() {
        let (cluster, mock) = cluster("prod-eu", vec![pod("ok"), pod("denied"), pod("gone"), pod("offline")]);
        *mock.behaviour.lock().unwrap_or_else(|p| p.into_inner()) = BTreeMap::from([
            ("denied".into(), Behaviour::Denied),
            ("gone".into(), Behaviour::Gone),
            ("offline".into(), Behaviour::Unavailable),
        ]);
        let fleet = KubeFleet::open(&group(&["prod-eu"], Some("app=web")), vec![cluster])
            .await
            .expect("fleet opens");
        let report = fleet.run(&describe()).await.expect("operation completes");
        let states: Vec<_> = report.result().rows().iter().map(|row| row.state()).collect();
        assert_eq!(
            states,
            [
                HostState::AccessDenied,
                HostState::PodDisappeared,
                HostState::NoContact,
                HostState::Success
            ]
        );
        let rendered = report.render();
        assert!(rendered.contains("access denied"), "{rendered}");
        assert!(rendered.contains("pod disappeared"), "{rendered}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_member_deadline_is_a_timeout_not_an_api_denial() {
        let (cluster, mock) = cluster("prod-eu", vec![pod("slow")]);
        mock.behaviour
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert("slow".into(), Behaviour::Hang);
        let mut group = group(&["prod-eu"], Some("app=web"));
        group.per_host_timeout_secs = 1;
        let fleet = KubeFleet::open(&group, vec![cluster]).await.expect("fleet opens");
        let report = fleet.run(&describe()).await.expect("operation completes");
        assert_eq!(report.result().rows()[0].state(), HostState::TimedOut);
    }

    #[tokio::test]
    async fn pod_answers_use_the_existing_difference_fold() {
        let (cluster, mock) = cluster("prod-eu", vec![pod("a"), pod("b"), pod("c")]);
        *mock.logs.lock().unwrap_or_else(|p| p.into_inner()) = BTreeMap::from([
            (
                "a".into(),
                PodLogs {
                    text: "same".into(),
                    truncated: false,
                },
            ),
            (
                "b".into(),
                PodLogs {
                    text: "same".into(),
                    truncated: false,
                },
            ),
            (
                "c".into(),
                PodLogs {
                    text: "different".into(),
                    truncated: false,
                },
            ),
        ]);
        let fleet = KubeFleet::open(&group(&["prod-eu"], Some("app=web")), vec![cluster])
            .await
            .expect("fleet opens");
        let report = fleet.run(&logs()).await.expect("operation completes");
        assert_eq!(report.folded().groups().len(), 2);
        assert_eq!(report.folded().baseline().map(|group| group.len()), Some(2));
        assert_eq!(report.result().summary().count(HostState::Divergent), 1);
        let calls = mock.log_calls.lock().unwrap_or_else(|p| p.into_inner());
        assert!(calls.iter().all(|(namespace, _, container, previous)| {
            namespace == "shop" && container.as_deref() == Some("main") && *previous
        }));
    }

    #[tokio::test]
    async fn new_pods_are_reported_without_joining_the_frozen_operation() {
        let (cluster, mock) = cluster("prod-eu", vec![pod("a"), pod("b")]);
        let fleet = KubeFleet::open(&group(&["prod-eu"], Some("app=web")), vec![cluster])
            .await
            .expect("fleet opens");
        mock.pods.lock().unwrap_or_else(|p| p.into_inner()).push(pod("new"));
        let report = fleet.run(&logs()).await.expect("operation completes");
        assert_eq!(report.result().rows().len(), 2);
        assert_eq!(report.new_pods(), ["prod-eu/shop/new/main"]);
        assert!(report
            .render()
            .contains("появились поды, не вошедшие во флот: prod-eu/shop/new/main"));
    }

    #[tokio::test]
    async fn gate_is_once_with_full_radius_and_rejects_unbound_calls() {
        let (cluster, _) = cluster("prod-eu", vec![pod("a"), pod("b")]);
        let fleet = KubeFleet::open(&group(&["prod-eu"], Some("app=web")), vec![cluster])
            .await
            .expect("fleet opens");
        assert_eq!(fleet.gate(&logs()), FleetGate::Ask(ApprovalScope::Once));
        assert_eq!(ApprovalScope::Once.approvals_needed(fleet.fleet().len()), 1);
        assert_eq!(
            fleet.confirmation_radius(),
            "on 2 hosts: prod-eu/shop/a/main, prod-eu/shop/b/main"
        );
        let rollout = KubeToolCall::RolloutHistory {
            namespace: "shop".into(),
            deployment: "web".into(),
            limit: 20,
        };
        assert!(matches!(fleet.gate(&rollout), FleetGate::Refuse(_)));
        let nodes = KubeToolCall::Top {
            resource: KubeTopResource::Nodes,
            namespace: None,
            limit: 20,
        };
        assert!(matches!(fleet.gate(&nodes), FleetGate::Refuse(_)));
    }

    #[tokio::test]
    async fn fanout_never_exceeds_the_structural_parallel_limit() {
        let (cluster, mock) = cluster("prod-eu", vec![pod("a"), pod("b"), pod("c"), pod("d")]);
        *mock.delay.lock().unwrap_or_else(|p| p.into_inner()) = Duration::from_millis(30);
        let fleet = KubeFleet::open(&group(&["prod-eu"], Some("app=web")), vec![cluster])
            .await
            .expect("fleet opens");
        fleet.run(&describe()).await.expect("operation completes");
        assert_eq!(mock.max_active.load(Ordering::SeqCst), 2);
    }
}
