//! Pod tabs (#565): a shell in a pod's container, entered from a cluster tab.
//!
//! A pod tab is an ordinary tab whose executor is a `KubeExecutor` and whose
//! [`KubeTabState::pod`] is set. This module holds the pod and container
//! chooser, the tab's lifecycle (connect, failure, pod replaced) and the
//! texts the status bar, F1 and the confirmation dialog show for it. Live
//! clients stay in the runner, as for cluster tabs.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use filar_core::{ChatBlock, PodLimits, PodRef, PodSelector};

use super::{App, KubePodTabState, KubeTabState, SessionId};
use crate::ui::theme::Glyphs;

/// Refusal for the file panels (Ctrl+G) in a pod tab.
pub const KUBE_POD_PANELS_REFUSAL: &str =
    "файловые панели (Ctrl+G) для пода пока недоступны — они работают с локальным диском и SSH";

/// A pod list for the chooser, run by the runner over the cluster tab's API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubePodListRequest {
    /// The cluster tab the chooser was opened from.
    pub session_id: SessionId,
    /// The chooser instance the result belongs to.
    pub select_id: u64,
    /// Namespace to list.
    pub namespace: String,
}

/// A pod shell to open, run by the runner with `KubeExecutor::connect`.
#[derive(Debug, Clone)]
pub struct KubePodConnectRequest {
    /// The pod tab.
    pub session_id: SessionId,
    /// The tab's connection generation; stale results are dropped.
    pub connection_id: u64,
    /// The cluster entry from `[[kube_clusters]]`.
    pub cluster: filar_core::KubeCluster,
    /// The pod to enter.
    pub pod_ref: PodRef,
}

/// One pod offered by the chooser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodChoice {
    /// Pod name.
    pub name: String,
    /// Whether the pod is `Ready`.
    pub ready: bool,
    /// Its regular containers, in spec order.
    pub containers: Vec<String>,
}

/// Where the chooser is: picking a pod, or a container of the chosen pod.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodSelectStage {
    /// The pod list (with the typed filter).
    Pods,
    /// Containers of `pod`.
    Containers {
        /// The chosen pod.
        pod: String,
        /// Its containers.
        containers: Vec<String>,
        /// Highlighted container.
        index: usize,
    },
    /// Debug image for `pod`/`container` (#576), from `debug_images`.
    Images {
        /// The chosen pod.
        pod: String,
        /// The target container.
        container: String,
        /// The cluster's `debug_images`.
        images: Vec<String>,
        /// Highlighted image.
        index: usize,
    },
}

/// The pod and container chooser of a cluster tab (#565).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KubePodSelect {
    /// The cluster tab it belongs to.
    pub session_id: SessionId,
    /// Chooser instance: a late pod list for an older chooser is dropped.
    pub select_id: u64,
    /// The cluster.
    pub cluster: filar_core::KubeCluster,
    /// Namespace whose pods are listed.
    pub namespace: String,
    /// The pod list has not arrived yet.
    pub loading: bool,
    /// Why the list could not be read.
    pub error: Option<String>,
    /// Running pods of the namespace.
    pub pods: Vec<PodChoice>,
    /// Typed filter. A filter with `=` is a label selector.
    pub filter: String,
    /// Highlighted row of [`KubePodSelect::rows`].
    pub index: usize,
    /// Pod list or container list.
    pub stage: PodSelectStage,
    /// Chooser for a debug container (#576): a concrete pod, its target
    /// container, then an image from `debug_images`.
    pub debug: bool,
}

/// A debug container waiting for the user's explicit yes (#576), and,
/// once approved, the runner's request to create it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugProposal {
    /// The tab the action came from (its feed records the trace).
    pub origin: SessionId,
    /// The cluster.
    pub cluster: filar_core::KubeCluster,
    /// Pod namespace.
    pub namespace: String,
    /// Pod name.
    pub pod: String,
    /// Container whose processes the debug container shares.
    pub target_container: String,
    /// Image from `debug_images`.
    pub image: String,
    /// `filar-debug-<id>`.
    pub name: String,
}

impl DebugProposal {
    /// The confirmation text, without softening: what is added, where, and
    /// that it cannot be removed.
    pub fn confirmation_text(&self) -> String {
        format!(
            "В под {}/{} будет добавлен эфемерный контейнер {} (образ {}, target {}). \
             Он останется в спецификации пода до его пересоздания; удалить его нельзя. Создать?",
            self.namespace, self.pod, self.name, self.image, self.target_container
        )
    }

    /// The trace line for the feed and the session export.
    pub fn trace_text(&self, at: &str) -> String {
        format!(
            "След в кластере: в под {}/{} добавлен эфемерный контейнер {} (образ {}, target {}, {at}). \
             Он останется в спецификации пода до его пересоздания.",
            self.namespace, self.pod, self.name, self.image, self.target_container
        )
    }

    /// Whether the cluster is tagged `prod` (the dialog is highlighted).
    pub fn is_prod(&self) -> bool {
        self.cluster.tags.iter().any(|t| t.eq_ignore_ascii_case("prod"))
    }
}

/// Short id for `filar-debug-<id>`: the low hex digits of the clock.
fn debug_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{:08x}", (nanos & 0xffff_ffff) as u32)
}

/// A row of the pod list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodSelectRow {
    /// The typed filter as a label selector (`app=web`): the first ready pod,
    /// followed again when it is replaced.
    Selector(String),
    /// A pod at `pods[i]`.
    Pod(usize),
}

impl KubePodSelect {
    /// Rows under the current filter: a filter with `=` is a label selector
    /// (one row); otherwise the pods whose name contains it.
    pub fn rows(&self) -> Vec<PodSelectRow> {
        let filter = self.filter.trim();
        if filter.contains('=') {
            return vec![PodSelectRow::Selector(filter.to_string())];
        }
        let needle = filter.to_lowercase();
        self.pods
            .iter()
            .enumerate()
            .filter(|(_, pod)| pod.name.to_lowercase().contains(&needle))
            .map(|(i, _)| PodSelectRow::Pod(i))
            .collect()
    }
}

/// Pod choices from the cluster API's pod list: ready first, then by name.
pub fn pod_choices(pods: Vec<(String, bool, Vec<String>)>) -> Vec<PodChoice> {
    let mut choices: Vec<PodChoice> = pods
        .into_iter()
        .map(|(name, ready, containers)| PodChoice {
            name,
            ready,
            containers,
        })
        .collect();
    choices.sort_by(|a, b| b.ready.cmp(&a.ready).then_with(|| a.name.cmp(&b.name)));
    choices
}

/// Parse the pod part of `--target cluster/namespace/<pod>`: a pod name, or
/// a label selector when it contains `=` (#565).
pub fn parse_pod_target(value: &str) -> Option<PodSelector> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    Some(if value.contains('=') {
        PodSelector::Labels(value.to_string())
    } else {
        PodSelector::Name(value.to_string())
    })
}

/// The pod's limits for the status bar, after the target:
/// `non-root · ro-fs · mem 512Mi`. Known facts only; `None` when nothing is
/// known.
pub fn pod_limits_summary(limits: &PodLimits, glyphs: &Glyphs) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if limits.run_as_non_root == Some(true) || limits.run_as_user.is_some_and(|uid| uid != 0) {
        parts.push("non-root".into());
    } else if limits.run_as_user == Some(0) {
        parts.push("root".into());
    }
    if limits.read_only_root_filesystem == Some(true) {
        parts.push("ro-fs".into());
    }
    if let Some(memory) = &limits.memory_limit {
        parts.push(format!("mem {memory}"));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(&format!(" {} ", glyphs.middle_dot)))
    }
}

/// Every known setting of the pod's container, one per line, for F1.
pub fn pod_limits_details(limits: &PodLimits) -> Vec<String> {
    let mut lines = Vec::new();
    let mut push = |label: &str, value: Option<String>| {
        if let Some(value) = value {
            lines.push(format!("{label}: {value}"));
        }
    };
    push("memory limit", limits.memory_limit.clone());
    push("memory request", limits.memory_request.clone());
    push("CPU limit", limits.cpu_limit.clone());
    push("CPU request", limits.cpu_request.clone());
    push("user", limits.run_as_user.map(|uid| uid.to_string()));
    push(
        "non-root",
        limits.run_as_non_root.map(|v| if v { "yes" } else { "no" }.to_string()),
    );
    push(
        "read-only root filesystem",
        limits
            .read_only_root_filesystem
            .map(|v| if v { "yes" } else { "no" }.to_string()),
    );
    if !limits.dropped_capabilities.is_empty() {
        lines.push(format!(
            "dropped capabilities: {}",
            limits.dropped_capabilities.join(", ")
        ));
    }
    if lines.is_empty() {
        lines.push("the pod spec sets no limits or security settings for this container".into());
    }
    lines
}

/// The target line of the confirmation dialog in a pod tab:
/// `context prod-eu · namespace shop · pod web-7f9c… · container main`.
pub fn pod_confirm_target_line(kube: &KubeTabState, glyphs: &Glyphs) -> Option<String> {
    let pod = kube.pod.as_ref()?;
    let sep = format!(" {} ", glyphs.middle_dot);
    let mut parts = vec![
        format!("context {}", kube.target.context),
        format!(
            "namespace {}",
            kube.namespace
                .as_deref()
                .unwrap_or(pod.pod_ref.namespace.as_str())
        ),
        format!("pod {}", pod.display_pod()),
    ];
    if let Some(container) = pod.container.as_deref().or(pod.pod_ref.container.as_deref()) {
        parts.push(format!("container {container}"));
    }
    Some(parts.join(&sep))
}

/// Feed line when the pod behind a tab was replaced (#563, #565).
pub fn pod_changed_message(old: &str, new: &str) -> String {
    format!("Под {old} пропал, вкладка теперь в {new} — cd и переменные сброшены")
}

impl App {
    /// The active tab is a pod tab.
    pub fn in_pod_tab(&self) -> bool {
        self.kube.as_ref().is_some_and(|kube| kube.pod.is_some())
    }

    /// The active tab is a cluster tab (no shell).
    pub fn in_cluster_tab(&self) -> bool {
        self.kube.as_ref().is_some_and(|kube| kube.pod.is_none())
    }

    /// Refuse a shell action in a Kubernetes tab with the message that fits
    /// it: a cluster tab has no shell at all, a pod tab lacks this feature.
    pub(crate) fn refuse_kube_action(&mut self, pod_message: &str) {
        if self.in_pod_tab() {
            self.push_message(ChatBlock::System(pod_message.into()));
        } else {
            self.refuse_kube_shell();
        }
    }

    /// Open the pod chooser of the active cluster tab (Ctrl+E, Ctrl+O row).
    pub fn open_pod_select(&mut self) {
        self.open_pod_chooser(false);
    }

    /// Ctrl+B: a debug container (#576). In a pod tab it is proposed for the
    /// tab's pod and container at once; in a cluster tab the chooser asks for
    /// a pod and a target container first. Unavailable without
    /// `debug_images` in the cluster's config.
    pub fn open_debug_select(&mut self) {
        let Some(kube) = self.kube.clone() else {
            return;
        };
        if kube.target.debug_images.is_empty() {
            self.push_message(ChatBlock::System(format!(
                "Debug-контейнер недоступен: для кластера {} не задан debug_images в [[kube_clusters]]",
                kube.target.name
            )));
            return;
        }
        if let Some(state) = kube.pod.as_ref() {
            if state.debug_target.is_some() {
                self.push_message(ChatBlock::System(
                    "Это уже вкладка debug-контейнера".into(),
                ));
                return;
            }
            let (Some(pod), Some(container)) = (state.pod.clone(), state.container.clone()) else {
                self.push_message(ChatBlock::System(format!(
                    "Под {} ещё подключается — debug-контейнер можно добавить после подключения \
                     или из вкладки кластера (Ctrl+B)",
                    self.target_name
                )));
                return;
            };
            let namespace = kube.namespace.clone().unwrap_or(state.pod_ref.namespace.clone());
            self.choose_debug_image(kube.target, namespace, pod, container);
            return;
        }
        self.open_pod_chooser(true);
    }

    /// Image step, then the confirmation (#576).
    fn choose_debug_image(
        &mut self,
        cluster: filar_core::KubeCluster,
        namespace: String,
        pod: String,
        container: String,
    ) {
        if cluster.debug_images.len() == 1 {
            let image = cluster.debug_images[0].clone();
            self.propose_debug(cluster, namespace, pod, container, image);
            return;
        }
        let session_id = self.active_session().id;
        let select_id = self.next_kube_id();
        let images = cluster.debug_images.clone();
        self.kube_pod_select = Some(KubePodSelect {
            session_id,
            select_id,
            cluster,
            namespace,
            loading: false,
            error: None,
            pods: Vec::new(),
            filter: String::new(),
            index: 0,
            stage: PodSelectStage::Images {
                pod,
                container,
                images,
                index: 0,
            },
            debug: true,
        });
    }

    /// Show the confirmation for a debug container.
    fn propose_debug(
        &mut self,
        cluster: filar_core::KubeCluster,
        namespace: String,
        pod: String,
        target_container: String,
        image: String,
    ) {
        self.kube_pod_select = None;
        self.debug_proposal = Some(DebugProposal {
            origin: self.active_session().id,
            cluster,
            namespace,
            pod,
            target_container,
            image,
            name: filar_transport::debug_container_name(&debug_id()),
        });
    }

    /// Keys of the debug confirmation: `y` creates, `n` / Esc cancels;
    /// Enter alone does nothing, so a stray key never writes to a cluster.
    pub(crate) fn handle_debug_proposal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Char('y' | 'Y' | 'н' | 'Н') => {
                if let Some(proposal) = self.debug_proposal.take() {
                    self.push_message(ChatBlock::System(format!(
                        "Создаю debug-контейнер {} в поде {}/{}…",
                        proposal.name, proposal.namespace, proposal.pod
                    )));
                    self.pending_kube_debug_creates.push(proposal);
                }
            }
            KeyCode::Char('n' | 'N' | 'т' | 'Т') | KeyCode::Esc => {
                self.debug_proposal = None;
                self.push_message(ChatBlock::System(
                    "Debug-контейнер не создан.".into(),
                ));
            }
            _ => {}
        }
    }

    /// Take the approved debug containers (runner).
    pub fn take_pending_kube_debug_creates(&mut self) -> Vec<DebugProposal> {
        std::mem::take(&mut self.pending_kube_debug_creates)
    }

    /// The debug container runs: record the trace in the origin tab and open
    /// a pod tab in it.
    pub fn debug_container_created(&mut self, proposal: DebugProposal, at: &str) {
        let trace = proposal.trace_text(at);
        self.push_kube_message(proposal.origin, ChatBlock::System(trace.clone()));
        let pod_ref = PodRef::named(
            &proposal.cluster.name,
            &proposal.namespace,
            &proposal.pod,
            Some(&proposal.name),
        );
        self.open_pod_tab(proposal.cluster, proposal.namespace, pod_ref);
        if let Some(state) = self.kube.as_mut().and_then(|kube| kube.pod.as_mut()) {
            state.debug_target = Some(proposal.target_container);
        }
        self.push_message(ChatBlock::System(trace));
    }

    /// The debug container failed: the reason in the origin tab's feed.
    /// `added_at` is set when it already got into the pod spec (did not
    /// start: image, timeout) — then the trace is recorded too, since the
    /// container stays in the spec all the same.
    pub fn debug_container_failed(
        &mut self,
        proposal: &DebugProposal,
        error: &str,
        added_at: Option<&str>,
    ) {
        let Some(at) = added_at else {
            self.push_kube_message(
                proposal.origin,
                ChatBlock::Error(format!(
                    "Debug-контейнер {} в поде {}/{} не создан: {error}",
                    proposal.name, proposal.namespace, proposal.pod
                )),
            );
            return;
        };
        self.push_kube_message(proposal.origin, ChatBlock::System(proposal.trace_text(at)));
        self.push_kube_message(
            proposal.origin,
            ChatBlock::Error(format!(
                "Debug-контейнер {} добавлен в спецификацию пода {}/{}, но не запустился: {error}",
                proposal.name, proposal.namespace, proposal.pod
            )),
        );
    }

    fn open_pod_chooser(&mut self, debug: bool) {
        let Some(kube) = self.kube.clone() else {
            return;
        };
        if kube.pod.is_some() {
            self.push_message(ChatBlock::System(
                "Это уже вкладка пода; войти в другой под можно из вкладки кластера".into(),
            ));
            return;
        }
        let Some(namespace) = kube.namespace.clone() else {
            self.push_message(ChatBlock::System(
                "Сначала выберите namespace кластера (Ctrl+O), затем войдите в под".into(),
            ));
            return;
        };
        let session_id = self.active_session().id;
        let select_id = self.next_kube_id();
        self.kube_pod_select = Some(KubePodSelect {
            session_id,
            select_id,
            cluster: kube.target,
            namespace: namespace.clone(),
            loading: true,
            error: None,
            pods: Vec::new(),
            filter: String::new(),
            index: 0,
            stage: PodSelectStage::Pods,
            debug,
        });
        self.pending_kube_pod_lists.push(KubePodListRequest {
            session_id,
            select_id,
            namespace,
        });
    }

    /// Take the queued pod lists (runner).
    pub fn take_pending_kube_pod_lists(&mut self) -> Vec<KubePodListRequest> {
        std::mem::take(&mut self.pending_kube_pod_lists)
    }

    /// Take the queued pod connections (runner).
    pub fn take_pending_kube_pod_connects(&mut self) -> Vec<KubePodConnectRequest> {
        std::mem::take(&mut self.pending_kube_pod_connects)
    }

    /// Fill the chooser with the pod list, or its error. Stale results
    /// (closed or newer chooser) are dropped.
    pub fn complete_pod_list(
        &mut self,
        session_id: SessionId,
        select_id: u64,
        result: Result<Vec<PodChoice>, String>,
    ) {
        let Some(select) = self.kube_pod_select.as_mut() else {
            return;
        };
        if select.session_id != session_id || select.select_id != select_id {
            return;
        }
        select.loading = false;
        match result {
            Ok(pods) => {
                select.pods = pods;
                select.index = 0;
            }
            Err(error) => select.error = Some(error),
        }
    }

    /// Keys of the open pod chooser: it is modal, like the namespace one.
    pub(crate) fn handle_pod_select_key(&mut self, key: KeyEvent) {
        let Some(select) = self.kube_pod_select.as_mut() else {
            return;
        };
        let ctrl = key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match (&mut select.stage, key.code) {
            (PodSelectStage::Containers { .. }, KeyCode::Esc) => {
                select.stage = PodSelectStage::Pods;
            }
            (PodSelectStage::Images { .. }, KeyCode::Esc) => {
                self.kube_pod_select = None;
            }
            (PodSelectStage::Images { index, .. }, KeyCode::Up) => {
                *index = index.saturating_sub(1);
            }
            (PodSelectStage::Images { index, images, .. }, KeyCode::Down) => {
                if *index + 1 < images.len() {
                    *index += 1;
                }
            }
            (PodSelectStage::Pods, KeyCode::Esc) => {
                if select.filter.is_empty() {
                    self.kube_pod_select = None;
                } else {
                    select.filter.clear();
                    select.index = 0;
                }
            }
            (PodSelectStage::Containers { index, .. }, KeyCode::Up) => {
                *index = index.saturating_sub(1);
            }
            (PodSelectStage::Containers { index, containers, .. }, KeyCode::Down) => {
                if *index + 1 < containers.len() {
                    *index += 1;
                }
            }
            (PodSelectStage::Pods, KeyCode::Up) => select.index = select.index.saturating_sub(1),
            (PodSelectStage::Pods, KeyCode::Down) => {
                if select.index + 1 < select.rows().len() {
                    select.index += 1;
                }
            }
            (PodSelectStage::Pods, KeyCode::Backspace) => {
                select.filter.pop();
                select.index = 0;
            }
            (PodSelectStage::Pods, KeyCode::Char(c)) if !ctrl => {
                select.filter.push(c);
                select.index = 0;
            }
            (_, KeyCode::Enter) => self.confirm_pod_select(),
            _ => {}
        }
    }

    /// Enter in the chooser: pick the selector or pod (then its container
    /// when it has several), or the highlighted container.
    fn confirm_pod_select(&mut self) {
        let Some(select) = self.kube_pod_select.clone() else {
            return;
        };
        match &select.stage {
            PodSelectStage::Images {
                pod,
                container,
                images,
                index,
            } => {
                if let Some(image) = images.get(*index).cloned() {
                    self.propose_debug(
                        select.cluster.clone(),
                        select.namespace.clone(),
                        pod.clone(),
                        container.clone(),
                        image,
                    );
                }
            }
            PodSelectStage::Containers {
                pod,
                containers,
                index,
            } if select.debug => {
                if let Some(container) = containers.get(*index).cloned() {
                    self.choose_debug_image(
                        select.cluster.clone(),
                        select.namespace.clone(),
                        pod.clone(),
                        container,
                    );
                }
            }
            PodSelectStage::Containers {
                pod,
                containers,
                index,
            } => {
                let container = containers.get(*index).cloned();
                self.kube_pod_select = None;
                let pod_ref = PodRef {
                    cluster: select.cluster.name.clone(),
                    namespace: select.namespace.clone(),
                    pod: PodSelector::Name(pod.clone()),
                    container,
                };
                self.open_pod_tab(select.cluster, select.namespace, pod_ref);
            }
            PodSelectStage::Pods => {
                let rows = select.rows();
                match rows.get(select.index) {
                    Some(PodSelectRow::Selector(_)) if select.debug => {
                        self.push_message(ChatBlock::System(
                            "Для debug-контейнера выберите конкретный под из списка".into(),
                        ));
                    }
                    Some(PodSelectRow::Pod(i)) if select.debug => {
                        let Some(choice) = select.pods.get(*i).cloned() else {
                            return;
                        };
                        if choice.containers.len() == 1 {
                            self.choose_debug_image(
                                select.cluster.clone(),
                                select.namespace.clone(),
                                choice.name,
                                choice.containers[0].clone(),
                            );
                        } else if let Some(open) = self.kube_pod_select.as_mut() {
                            open.stage = PodSelectStage::Containers {
                                pod: choice.name,
                                containers: choice.containers,
                                index: 0,
                            };
                        }
                    }
                    Some(PodSelectRow::Selector(selector)) => {
                        self.kube_pod_select = None;
                        let pod_ref = PodRef {
                            cluster: select.cluster.name.clone(),
                            namespace: select.namespace.clone(),
                            pod: PodSelector::Labels(selector.clone()),
                            container: None,
                        };
                        self.open_pod_tab(select.cluster, select.namespace, pod_ref);
                    }
                    Some(PodSelectRow::Pod(i)) => {
                        let Some(choice) = select.pods.get(*i).cloned() else {
                            return;
                        };
                        if choice.containers.len() > 1 {
                            if let Some(open) = self.kube_pod_select.as_mut() {
                                open.stage = PodSelectStage::Containers {
                                    pod: choice.name,
                                    containers: choice.containers,
                                    index: 0,
                                };
                            }
                        } else {
                            self.kube_pod_select = None;
                            let pod_ref = PodRef {
                                cluster: select.cluster.name.clone(),
                                namespace: select.namespace.clone(),
                                pod: PodSelector::Name(choice.name),
                                container: choice.containers.into_iter().next(),
                            };
                            self.open_pod_tab(select.cluster, select.namespace, pod_ref);
                        }
                    }
                    None => {}
                }
            }
        }
    }

    /// Open a new pod tab for `pod_ref`; the cluster tab stays as it is.
    pub fn open_pod_tab(
        &mut self,
        cluster: filar_core::KubeCluster,
        namespace: String,
        pod_ref: PodRef,
    ) {
        self.new_tab();
        self.start_pod_connection(cluster, namespace, pod_ref);
    }

    /// Point the active tab at a pod and queue its shell connection.
    pub(crate) fn start_pod_connection(
        &mut self,
        cluster: filar_core::KubeCluster,
        namespace: String,
        pod_ref: PodRef,
    ) {
        let session_id = self.active_session().id;
        let connection_id = self.next_kube_id();
        let state = KubePodTabState::pending(pod_ref.clone());
        self.target_name = state.display_pod();
        self.ssh_info = None;
        self.cwd = None;
        self.push_message(ChatBlock::System(format!(
            "Вход в под {} (namespace {namespace}, кластер {})…",
            state.display_pod(),
            cluster.name
        )));
        self.kube = Some(KubeTabState {
            target: cluster.clone(),
            namespace: Some(namespace),
            connection_id,
            pod: Some(state),
        });
        self.pending_kube_pod_connects.push(KubePodConnectRequest {
            session_id,
            connection_id,
            cluster,
            pod_ref,
        });
        self.sync_confirm_mode();
    }

    /// Pod tab of session `sid` whose connection generation is
    /// `connection_id`, if that is still the tab's current target.
    fn current_pod_tab(&mut self, sid: SessionId, connection_id: u64) -> Option<usize> {
        let idx = self.find_session_idx(sid)?;
        let kube = self.sessions[idx].kube.as_ref()?;
        (kube.connection_id == connection_id && kube.pod.is_some()).then_some(idx)
    }

    /// Whether a pod connection result still belongs to the tab's target.
    pub fn kube_pod_connection_is_current(&self, sid: SessionId, connection_id: u64) -> bool {
        self.sessions.iter().any(|session| {
            session.id == sid
                && session
                    .kube
                    .as_ref()
                    .is_some_and(|kube| kube.connection_id == connection_id && kube.pod.is_some())
        })
    }

    /// The shell is open: record where it runs and say so in the feed.
    pub fn complete_kube_pod_connection(
        &mut self,
        sid: SessionId,
        connection_id: u64,
        pod: String,
        container: String,
        limits: PodLimits,
    ) -> bool {
        let Some(idx) = self.current_pod_tab(sid, connection_id) else {
            return false;
        };
        let session = &mut self.sessions[idx];
        let Some(kube) = session.kube.as_mut() else {
            return false;
        };
        let namespace = kube.namespace.clone().unwrap_or_default();
        let cluster = kube.target.name.clone();
        if let Some(state) = kube.pod.as_mut() {
            state.pod = Some(pod.clone());
            state.container = Some(container.clone());
            state.limits = Some(limits);
        }
        session.target_name = pod.clone();
        self.push_kube_message(
            sid,
            ChatBlock::System(format!(
                "Подключено к поду {pod} (контейнер {container}, namespace {namespace}, кластер {cluster})"
            )),
        );
        true
    }

    /// The pod shell could not be opened (RBAC, admission policy, no pod,
    /// no shell): say why, and keep the tab on its previous target — the
    /// cluster and namespace it was entered from.
    pub fn fail_kube_pod_connection(&mut self, sid: SessionId, connection_id: u64, error: String) {
        let Some(idx) = self.current_pod_tab(sid, connection_id) else {
            return;
        };
        let new_id = self.next_kube_id();
        let session = &mut self.sessions[idx];
        let Some(kube) = session.kube.as_mut() else {
            return;
        };
        let pod = kube.pod.take().map(|state| state.display_pod()).unwrap_or_default();
        kube.connection_id = new_id;
        let target = kube.target.clone();
        let namespace = kube.namespace.clone();
        session.target_name = target.name.clone();
        self.push_kube_message(
            sid,
            ChatBlock::Error(format!(
                "Не удалось войти в под {pod}, вкладка осталась на кластере {}: {error}",
                target.name
            )),
        );
        self.pending_kube_connects.push(super::KubeConnectRequest {
            session_id: sid,
            target,
            namespace,
            connection_id: new_id,
            discover: false,
        });
    }

    /// The pod behind the tab was replaced and the shell moved (#563).
    /// Returns `false` for an event of a shell the tab no longer uses.
    pub fn kube_pod_changed(
        &mut self,
        sid: SessionId,
        connection_id: u64,
        old: &str,
        new: &str,
    ) -> bool {
        let Some(idx) = self.current_pod_tab(sid, connection_id) else {
            return false;
        };
        let session = &mut self.sessions[idx];
        if let Some(state) = session.kube.as_mut().and_then(|kube| kube.pod.as_mut()) {
            state.pod = Some(new.to_string());
        }
        session.target_name = new.to_string();
        // The shell is a fresh one: its cwd is unknown again.
        session.cwd = None;
        self.push_kube_message(sid, ChatBlock::System(pod_changed_message(old, new)));
        true
    }

    /// The shell reconnected (a fresh shell, in the same pod or the one a
    /// selector picked): refresh pod, container and limits, and forget the
    /// cwd — `cd` and variables do not survive a new shell. Returns `false`
    /// for an event of a shell the tab no longer uses.
    pub fn kube_pod_identity(
        &mut self,
        sid: SessionId,
        connection_id: u64,
        pod: String,
        container: String,
        limits: PodLimits,
    ) -> bool {
        let Some(idx) = self.current_pod_tab(sid, connection_id) else {
            return false;
        };
        let session = &mut self.sessions[idx];
        session.cwd = None;
        if let Some(state) = session.kube.as_mut().and_then(|kube| kube.pod.as_mut()) {
            state.pod = Some(pod.clone());
            state.container = Some(container);
            state.limits = Some(limits);
        }
        session.target_name = pod;
        true
    }

    /// What the agent is told about the active pod tab's target, once its
    /// shell is connected.
    pub fn pod_agent_target(&self) -> Option<filar_agent::KubePodTarget> {
        let kube = self.kube.as_ref()?;
        let state = kube.pod.as_ref()?;
        let target = filar_agent::KubePodTarget::new(
            &kube.target,
            kube.namespace.as_deref().unwrap_or(&state.pod_ref.namespace),
            state.pod.as_deref()?,
            state.container.as_deref()?,
            state.limits.clone().unwrap_or_default(),
        );
        Some(match state.debug_target.as_deref() {
            Some(debug_target) => target.with_debug_target(debug_target),
            None => target,
        })
    }

    /// The pod tab is not connected yet: commands would run on the tab's
    /// placeholder executor, so sending is refused (input kept).
    pub(crate) fn handle_pod_connecting_key(&mut self, key: &KeyEvent) -> bool {
        let connecting = self
            .kube
            .as_ref()
            .and_then(|kube| kube.pod.as_ref())
            .is_some_and(|state| state.pod.is_none());
        if !connecting || self.mode != super::AppMode::Normal {
            return false;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let send = key.code == KeyCode::Enter && !ctrl && !self.input.trim().is_empty();
        if !send {
            return false;
        }
        self.push_message(ChatBlock::System(format!(
            "Под {} ещё подключается: ничего не выполнено (ввод сохранён).",
            self.target_name
        )));
        self.scroll = 0;
        true
    }

    /// Ctrl+E: enter a pod from a cluster tab. Returns `true` when handled.
    pub(crate) fn handle_pod_tab_key(&mut self, key: &KeyEvent) -> bool {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        // Normal mode only: in a confirmation or password prompt the chooser
        // would take over the keys meant for it (#565 review).
        if self.mode != super::AppMode::Normal || !ctrl || self.kube.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Char('e' | 'у') => self.open_pod_select(),
            // Debug container (#576) — a user action only.
            KeyCode::Char('b' | 'и') => self.open_debug_select(),
            _ => return false,
        }
        true
    }

    /// Snapshot of a pod tab for session save (#565).
    pub(crate) fn pod_snapshot(state: &KubePodTabState) -> filar_core::KubePodSnapshot {
        filar_core::KubePodSnapshot {
            pod: state.pod_ref.pod.clone(),
            container: state
                .pod_ref
                .container
                .clone()
                .or_else(|| state.container.clone()),
            debug_target: state.debug_target.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Glyphs;
    use filar_core::CommandConfirmMode;

    #[test]
    fn pod_terminal_refusal_tells_connecting_from_lost_shell() {
        let mut state = KubePodTabState::pending(PodRef::named("prod-eu", "shop", "web-1", None));
        let connecting = state.terminal_unavailable_message();
        assert!(connecting.contains("ещё подключается"), "{connecting}");
        assert!(connecting.contains("web-1"), "{connecting}");
        state.pod = Some("web-1".into());
        let lost = state.terminal_unavailable_message();
        assert!(lost.contains("недоступен"), "{lost}");
        assert!(!lost.contains("подключается"), "{lost}");
    }

    fn cluster() -> filar_core::KubeCluster {
        filar_core::KubeCluster {
            name: "prod-eu".into(),
            context: "prod-eu-ctx".into(),
            tags: vec!["prod".into()],
            namespaces: vec!["shop".into()],
            ..Default::default()
        }
    }

    fn app_in_cluster_tab() -> App {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.kube = Some(KubeTabState {
            target: cluster(),
            namespace: Some("shop".into()),
            connection_id: 1,
            pod: None,
        });
        app
    }

    fn choice(name: &str, ready: bool, containers: &[&str]) -> PodChoice {
        PodChoice {
            name: name.into(),
            ready,
            containers: containers.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.handle_pod_select_key(key(KeyCode::Char(c)));
        }
    }

    fn opened_select(app: &mut App, pods: Vec<PodChoice>) {
        app.open_pod_select();
        let select = app.kube_pod_select.clone().expect("chooser open");
        let request = app.take_pending_kube_pod_lists().remove(0);
        assert_eq!(request.namespace, "shop");
        app.complete_pod_list(select.session_id, select.select_id, Ok(pods));
    }

    #[test]
    fn choices_put_ready_pods_first_then_by_name() {
        let pods = pod_choices(vec![
            ("web-b".into(), true, vec!["main".into()]),
            ("web-c".into(), false, vec![]),
            ("web-a".into(), true, vec![]),
        ]);
        let names: Vec<&str> = pods.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["web-a", "web-b", "web-c"]);
    }

    #[test]
    fn pod_target_is_a_name_or_a_selector() {
        assert_eq!(parse_pod_target("web-1"), Some(PodSelector::Name("web-1".into())));
        assert_eq!(parse_pod_target("app=web"), Some(PodSelector::Labels("app=web".into())));
        assert_eq!(parse_pod_target(" "), None);
    }

    #[test]
    fn chooser_needs_a_cluster_tab_with_a_namespace() {
        let mut app = app_in_cluster_tab();
        app.kube.as_mut().unwrap().namespace = None;
        app.open_pod_select();
        assert!(app.kube_pod_select.is_none());
        assert!(app.take_pending_kube_pod_lists().is_empty());
    }

    #[test]
    fn ctrl_e_opens_the_chooser_in_a_cluster_tab() {
        let mut app = app_in_cluster_tab();
        app.handle_key(KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL));
        assert!(app.kube_pod_select.is_some());
    }

    #[test]
    fn filter_narrows_pods_and_a_selector_becomes_its_own_row() {
        let mut app = app_in_cluster_tab();
        opened_select(
            &mut app,
            vec![choice("web-1", true, &["main"]), choice("db-0", true, &["pg"])],
        );
        type_text(&mut app, "web");
        let select = app.kube_pod_select.clone().unwrap();
        assert_eq!(select.rows(), vec![PodSelectRow::Pod(0)]);
        type_text(&mut app, "=x");
        let select = app.kube_pod_select.clone().unwrap();
        assert_eq!(select.rows(), vec![PodSelectRow::Selector("web=x".into())]);
    }

    #[test]
    fn single_container_pod_opens_a_new_pod_tab_and_keeps_the_cluster_tab() {
        let mut app = app_in_cluster_tab();
        let cluster_sid = app.active_session().id;
        opened_select(&mut app, vec![choice("web-1", true, &["main"])]);
        app.handle_pod_select_key(key(KeyCode::Enter));
        assert!(app.kube_pod_select.is_none());
        assert_ne!(app.active_session().id, cluster_sid, "a new tab");
        assert!(app.in_pod_tab());
        let cluster_tab = app.sessions.iter().find(|s| s.id == cluster_sid).unwrap();
        assert!(cluster_tab.kube.as_ref().unwrap().pod.is_none(), "cluster tab untouched");
        let request = app.take_pending_kube_pod_connects().remove(0);
        assert_eq!(request.pod_ref, PodRef::named("prod-eu", "shop", "web-1", Some("main")));
        assert_eq!(request.session_id, app.active_session().id);
    }

    #[test]
    fn multi_container_pod_asks_for_the_container() {
        let mut app = app_in_cluster_tab();
        opened_select(&mut app, vec![choice("web-1", true, &["main", "sidecar"])]);
        app.handle_pod_select_key(key(KeyCode::Enter));
        assert!(matches!(
            app.kube_pod_select.as_ref().unwrap().stage,
            PodSelectStage::Containers { .. }
        ));
        app.handle_pod_select_key(key(KeyCode::Down));
        app.handle_pod_select_key(key(KeyCode::Enter));
        let request = app.take_pending_kube_pod_connects().remove(0);
        assert_eq!(request.pod_ref.container.as_deref(), Some("sidecar"));
    }

    #[test]
    fn selector_row_opens_a_tab_that_follows_the_selector() {
        let mut app = app_in_cluster_tab();
        opened_select(&mut app, vec![]);
        type_text(&mut app, "app=web");
        app.handle_pod_select_key(key(KeyCode::Enter));
        let request = app.take_pending_kube_pod_connects().remove(0);
        assert_eq!(request.pod_ref.pod, PodSelector::Labels("app=web".into()));
        assert_eq!(app.target_name, "app=web");
    }

    #[test]
    fn stale_pod_list_is_dropped() {
        let mut app = app_in_cluster_tab();
        app.open_pod_select();
        let select = app.kube_pod_select.clone().unwrap();
        app.handle_pod_select_key(key(KeyCode::Esc));
        app.complete_pod_list(select.session_id, select.select_id, Ok(vec![]));
        assert!(app.kube_pod_select.is_none());
    }

    #[test]
    fn ctrl_o_offers_enter_pod_only_in_a_cluster_tab() {
        let mut app = app_in_cluster_tab();
        assert_eq!(app.host_select_rows().first(), Some(&super::super::HostSelectRow::EnterPod));
        app.host_select_index = super::super::ENTER_POD_SELECTION;
        app.host_select_visible = true;
        app.handle_key(key(KeyCode::Enter));
        assert!(app.kube_pod_select.is_some(), "the row opens the chooser");
        assert!(app.in_cluster_tab(), "the cluster tab is not switched");

        let mut local = App::new("test".into(), CommandConfirmMode::Always);
        local.kube = None;
        assert!(!local
            .host_select_rows()
            .contains(&super::super::HostSelectRow::EnterPod));
    }

    fn connected_pod_tab() -> (App, SessionId, u64) {
        let mut app = app_in_cluster_tab();
        app.open_pod_tab(
            cluster(),
            "shop".into(),
            PodRef::by_selector("prod-eu", "shop", "app=web", None),
        );
        let request = app.take_pending_kube_pod_connects().remove(0);
        let limits = PodLimits {
            memory_limit: Some("512Mi".into()),
            run_as_non_root: Some(true),
            read_only_root_filesystem: Some(true),
            ..Default::default()
        };
        assert!(app.complete_kube_pod_connection(
            request.session_id,
            request.connection_id,
            "web-7f9c".into(),
            "main".into(),
            limits,
        ));
        (app, request.session_id, request.connection_id)
    }

    #[test]
    fn connection_fills_the_tab_and_the_agent_target() {
        let (app, _, _) = connected_pod_tab();
        assert_eq!(app.target_name, "web-7f9c");
        let target = app.pod_agent_target().expect("agent target");
        assert_eq!(target.pod, "web-7f9c");
        assert_eq!(target.container, "main");
        assert_eq!(target.context, "prod-eu-ctx");
        assert_eq!(target.tags, vec!["prod".to_string()]);
        assert!(matches!(app.messages.last(), Some(ChatBlock::System(m)) if m.contains("Подключено к поду web-7f9c")));
    }

    #[test]
    fn sending_before_the_shell_is_connected_is_refused() {
        let mut app = app_in_cluster_tab();
        app.open_pod_tab(cluster(), "shop".into(), PodRef::named("prod-eu", "shop", "web-1", None));
        assert!(app.pod_agent_target().is_none());
        app.input = "uptime".into();
        assert!(app.handle_pod_connecting_key(&key(KeyCode::Enter)));
        assert_eq!(app.input, "uptime", "input kept");
    }

    #[test]
    fn failed_connection_keeps_the_tab_on_the_cluster() {
        let mut app = app_in_cluster_tab();
        app.open_pod_tab(cluster(), "shop".into(), PodRef::named("prod-eu", "shop", "web-1", None));
        let request = app.take_pending_kube_pod_connects().remove(0);
        app.fail_kube_pod_connection(
            request.session_id,
            request.connection_id,
            "RBAC: no create permission on pods/exec in namespace shop".into(),
        );
        assert!(app.in_cluster_tab(), "back on the cluster");
        assert_eq!(app.target_name, "prod-eu");
        assert!(matches!(app.messages.last(), Some(ChatBlock::Error(m)) if m.contains("pods/exec") && m.contains("кластере prod-eu")));
        let reconnect = app.take_pending_kube_connects().remove(0);
        assert_eq!(reconnect.namespace.as_deref(), Some("shop"));
        assert!(!reconnect.discover);
        // A late result of the abandoned connection changes nothing.
        assert!(!app.complete_kube_pod_connection(
            request.session_id,
            request.connection_id,
            "web-1".into(),
            "main".into(),
            PodLimits::default(),
        ));
        assert!(app.in_cluster_tab());
    }

    #[test]
    fn pod_change_is_announced_and_updates_the_tab() {
        let (mut app, sid, connection_id) = connected_pod_tab();
        app.cwd = Some("/tmp".into());
        assert!(app.kube_pod_changed(sid, connection_id, "web-7f9c", "web-a1b2"));
        assert!(!app.kube_pod_changed(sid, connection_id + 100, "web-a1b2", "web-x"), "stale shell");
        assert_eq!(app.target_name, "web-a1b2");
        assert!(app.cwd.is_none(), "fresh shell, cwd unknown");
        assert_eq!(app.pod_agent_target().unwrap().pod, "web-a1b2");
        assert!(matches!(
            app.messages.last(),
            Some(ChatBlock::System(m)) if m == "Под web-7f9c пропал, вкладка теперь в web-a1b2 — cd и переменные сброшены"
        ));
    }

    #[test]
    fn limits_summary_lists_known_facts_only() {
        let limits = PodLimits {
            memory_limit: Some("512Mi".into()),
            run_as_non_root: Some(true),
            read_only_root_filesystem: Some(true),
            ..Default::default()
        };
        assert_eq!(
            pod_limits_summary(&limits, &Glyphs::UNICODE).as_deref(),
            Some("non-root · ro-fs · mem 512Mi")
        );
        let ascii = pod_limits_summary(&limits, &Glyphs::ASCII).unwrap();
        assert!(ascii.is_ascii(), "{ascii}");
        assert_eq!(pod_limits_summary(&PodLimits::default(), &Glyphs::UNICODE), None);
        let root = PodLimits {
            run_as_user: Some(0),
            ..Default::default()
        };
        assert_eq!(pod_limits_summary(&root, &Glyphs::UNICODE).as_deref(), Some("root"));
    }

    #[test]
    fn limits_details_cover_every_known_field() {
        let limits = PodLimits {
            memory_limit: Some("512Mi".into()),
            memory_request: Some("256Mi".into()),
            cpu_limit: Some("500m".into()),
            cpu_request: Some("100m".into()),
            run_as_user: Some(65534),
            run_as_non_root: Some(true),
            read_only_root_filesystem: Some(false),
            dropped_capabilities: vec!["ALL".into()],
        };
        let lines = pod_limits_details(&limits);
        assert_eq!(lines.len(), 8, "{lines:?}");
        assert!(lines.contains(&"user: 65534".to_string()));
        assert!(lines.contains(&"dropped capabilities: ALL".to_string()));
        assert_eq!(pod_limits_details(&PodLimits::default()).len(), 1);
    }

    #[test]
    fn confirm_target_line_names_context_namespace_pod_and_container() {
        let (app, _, _) = connected_pod_tab();
        let line = pod_confirm_target_line(app.kube.as_ref().unwrap(), &Glyphs::UNICODE).unwrap();
        assert_eq!(
            line,
            "context prod-eu-ctx · namespace shop · pod web-7f9c · container main"
        );
        assert!(pod_confirm_target_line(
            app_in_cluster_tab().kube.as_ref().unwrap(),
            &Glyphs::UNICODE
        )
        .is_none());
    }

    #[test]
    fn pod_tab_refuses_file_panels_with_a_pod_message() {
        let (mut app, _, _) = connected_pod_tab();
        app.open_file_panels();
        assert!(app.file_panels.is_none());
        assert!(matches!(app.messages.last(), Some(ChatBlock::System(m)) if m == KUBE_POD_PANELS_REFUSAL));
    }

    /// Ctrl+T in a connected pod tab asks the runner for a terminal (#577);
    /// before the shell is connected it waits, and nothing local opens.
    #[test]
    fn ctrl_t_opens_the_pod_terminal_only_once_connected() {
        let mut app = app_in_cluster_tab();
        app.open_pod_tab(cluster(), "shop".into(), PodRef::named("prod-eu", "shop", "web-1", None));
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(!app.take_toggle_interactive(), "not while connecting");
        assert!(matches!(app.messages.last(), Some(ChatBlock::System(m)) if m.contains("ещё подключается")));

        let (mut app, _, _) = connected_pod_tab();
        app.handle_key(KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL));
        assert!(app.take_toggle_interactive());
    }

    #[test]
    fn snapshot_keeps_the_selector_not_the_live_pod() {
        let (app, _, _) = connected_pod_tab();
        let snap = App::pod_snapshot(app.kube.as_ref().unwrap().pod.as_ref().unwrap());
        assert_eq!(snap.pod, PodSelector::Labels("app=web".into()));
        assert_eq!(snap.container.as_deref(), Some("main"));
    }

    #[test]
    fn restoring_a_saved_pod_tab_reconnects_by_its_selector() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.kube_clusters = vec![cluster()];
        app.restore_kube_snapshot(filar_core::KubeSessionSnapshot {
            target: "prod-eu".into(),
            namespace: Some("shop".into()),
            pod: Some(filar_core::KubePodSnapshot {
                pod: PodSelector::Labels("app=web".into()),
                container: Some("main".into()),
                debug_target: None,
            }),
        });
        assert!(app.in_pod_tab());
        let request = app.take_pending_kube_pod_connects().remove(0);
        assert_eq!(
            request.pod_ref,
            PodRef::by_selector("prod-eu", "shop", "app=web", Some("main"))
        );
    }

    #[test]
    fn reconnect_forgets_the_cwd_and_ignores_stale_shells() {
        let (mut app, sid, connection_id) = connected_pod_tab();
        app.cwd = Some("/srv".into());
        assert!(!app.kube_pod_identity(sid, connection_id + 1, "web-x".into(), "main".into(), PodLimits::default()));
        assert_eq!(app.cwd.as_deref(), Some("/srv"), "stale event changes nothing");
        assert!(app.kube_pod_identity(sid, connection_id, "web-7f9c".into(), "main".into(), PodLimits::default()));
        assert!(app.cwd.is_none(), "a fresh shell starts in its own cwd");
    }

    #[test]
    fn pod_tab_never_opens_the_local_path_picker() {
        let (mut app, _, _) = connected_pod_tab();
        app.handle_key(key(KeyCode::Char('/')));
        assert!(app.pending_path_picker.is_none());
        assert_eq!(app.input, "/", "just a character");
        app.handle_key(KeyEvent::new(
            KeyCode::Char('F'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
        assert!(app.pending_path_picker.is_none());
    }

    #[test]
    fn ctrl_e_is_ignored_outside_normal_mode() {
        let mut app = app_in_cluster_tab();
        app.mode = crate::app::AppMode::Confirming;
        assert!(!app.handle_pod_tab_key(&KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL)));
        assert!(app.kube_pod_select.is_none());
    }

    // --- #576: debug containers --------------------------------------------

    fn debug_cluster(images: &[&str]) -> filar_core::KubeCluster {
        let mut c = cluster();
        c.debug_images = images.iter().map(|i| i.to_string()).collect();
        c
    }

    fn cluster_tab_with(images: &[&str]) -> App {
        let mut app = app_in_cluster_tab();
        app.kube.as_mut().unwrap().target = debug_cluster(images);
        app
    }

    fn ctrl(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
    }

    #[test]
    fn debug_needs_configured_images() {
        let mut app = cluster_tab_with(&[]);
        app.handle_key(ctrl('b'));
        assert!(app.kube_pod_select.is_none());
        assert!(matches!(app.messages.last(), Some(ChatBlock::System(m)) if m.contains("debug_images")));
    }

    #[test]
    fn debug_from_a_cluster_tab_asks_pod_then_confirms_with_the_trace_text() {
        let mut app = cluster_tab_with(&["busybox:1.37"]);
        app.handle_key(ctrl('b'));
        let select = app.kube_pod_select.clone().expect("chooser");
        assert!(select.debug);
        app.take_pending_kube_pod_lists();
        app.complete_pod_list(select.session_id, select.select_id, Ok(vec![choice("web-1", true, &["main"])]));
        app.handle_pod_select_key(key(KeyCode::Enter));
        let proposal = app.debug_proposal.clone().expect("confirmation shown");
        assert_eq!(proposal.target_container, "main");
        assert!(proposal.name.starts_with("filar-debug-"));
        let text = proposal.confirmation_text();
        assert!(text.starts_with("В под shop/web-1 будет добавлен эфемерный контейнер filar-debug-"), "{text}");
        assert!(text.contains("(образ busybox:1.37, target main)"), "{text}");
        assert!(text.contains("останется в спецификации пода до его пересоздания; удалить его нельзя. Создать?"), "{text}");
        assert!(proposal.is_prod());
        // Enter alone never creates.
        app.handle_key(key(KeyCode::Enter));
        assert!(app.debug_proposal.is_some());
        assert!(app.take_pending_kube_debug_creates().is_empty());
        app.handle_key(key(KeyCode::Char('n')));
        assert!(app.debug_proposal.is_none());
        assert!(app.take_pending_kube_debug_creates().is_empty(), "cancelled");
    }

    #[test]
    fn several_images_are_chosen_and_yes_queues_the_creation() {
        let mut app = cluster_tab_with(&["busybox:1.37", "registry.local/tools/netshoot:v0.13"]);
        app.handle_key(ctrl('b'));
        let select = app.kube_pod_select.clone().unwrap();
        app.complete_pod_list(select.session_id, select.select_id, Ok(vec![choice("web-1", true, &["main", "sidecar"])]));
        app.handle_pod_select_key(key(KeyCode::Enter));
        app.handle_pod_select_key(key(KeyCode::Down));
        app.handle_pod_select_key(key(KeyCode::Enter));
        assert!(matches!(app.kube_pod_select.as_ref().unwrap().stage, PodSelectStage::Images { .. }));
        app.handle_pod_select_key(key(KeyCode::Down));
        app.handle_pod_select_key(key(KeyCode::Enter));
        let proposal = app.debug_proposal.clone().unwrap();
        assert_eq!(proposal.target_container, "sidecar");
        assert_eq!(proposal.image, "registry.local/tools/netshoot:v0.13");
        app.handle_key(key(KeyCode::Char('y')));
        let queued = app.take_pending_kube_debug_creates();
        assert_eq!(queued, vec![proposal]);
    }

    #[test]
    fn a_created_debug_container_opens_a_debug_tab_and_records_the_trace() {
        let mut app = cluster_tab_with(&["busybox:1.37"]);
        let origin = app.active_session().id;
        let proposal = DebugProposal {
            origin,
            cluster: debug_cluster(&["busybox:1.37"]),
            namespace: "shop".into(),
            pod: "distroless-1".into(),
            target_container: "main".into(),
            image: "busybox:1.37".into(),
            name: "filar-debug-ab12cd34".into(),
        };
        app.debug_container_created(proposal, "2026-11-01 16:00:00");
        let request = app.take_pending_kube_pod_connects().remove(0);
        assert_eq!(request.pod_ref, PodRef::named("prod-eu", "shop", "distroless-1", Some("filar-debug-ab12cd34")));
        assert_eq!(app.kube.as_ref().unwrap().pod.as_ref().unwrap().debug_target.as_deref(), Some("main"));
        let origin_tab = app.sessions.iter().find(|s| s.id == origin).unwrap();
        assert!(origin_tab.messages.iter().any(|m| matches!(m, ChatBlock::System(t) if t.contains("След в кластере") && t.contains("filar-debug-ab12cd34") && t.contains("2026-11-01 16:00:00"))));
        app.complete_kube_pod_connection(request.session_id, request.connection_id, "distroless-1".into(), "filar-debug-ab12cd34".into(), PodLimits::default());
        assert_eq!(app.pod_agent_target().unwrap().debug_target.as_deref(), Some("main"));
    }

    #[test]
    fn a_debug_container_that_got_into_the_spec_but_did_not_start_still_leaves_a_trace() {
        let mut app = cluster_tab_with(&["busybox:1.37"]);
        let origin = app.active_session().id;
        let proposal = DebugProposal {
            origin,
            cluster: debug_cluster(&["busybox:1.37"]),
            namespace: "shop".into(),
            pod: "distroless-1".into(),
            target_container: "main".into(),
            image: "busybox:1.37".into(),
            name: "filar-debug-ab12cd34".into(),
        };
        app.debug_container_failed(&proposal, "RBAC: forbidden", None);
        app.debug_container_failed(&proposal, "ImagePullBackOff", Some("2026-11-01 16:00:00"));
        let origin_tab = app.sessions.iter().find(|s| s.id == origin).unwrap();
        let traces = origin_tab
            .messages
            .iter()
            .filter(|m| matches!(m, ChatBlock::System(t) if t.contains("След в кластере")))
            .count();
        assert_eq!(traces, 1, "a refused patch leaves no trace, an added container does");
        assert!(origin_tab.messages.iter().any(|m| matches!(m, ChatBlock::Error(t) if t.contains("не создан") && t.contains("RBAC"))));
        assert!(origin_tab.messages.iter().any(|m| matches!(m, ChatBlock::Error(t) if t.contains("добавлен в спецификацию") && t.contains("не запустился"))));
        assert!(app.take_pending_kube_pod_connects().is_empty());
    }

    #[test]
    fn the_agent_has_no_way_to_create_a_debug_container() {
        let names: Vec<String> = filar_agent::kube_tools::kube_tool_definitions(CommandConfirmMode::Always)
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert!(names.iter().all(|n| !n.contains("debug") && !n.contains("ephemeral")), "{names:?}");
        let call = filar_agent::kube_tools::parse_kube_tool_call("k8s_debug", &serde_json::json!({}), Some("shop"));
        assert!(call.is_err());
    }

    #[test]
    fn a_debug_tab_survives_save_and_restore_with_its_debug_prompt() {
        let mut app = App::new("test".into(), CommandConfirmMode::Always);
        app.kube_clusters = vec![cluster()];
        let mut state = KubePodTabState::pending(PodRef::named("prod-eu", "shop", "distroless-1", Some("filar-debug-ab12cd34")));
        state.debug_target = Some("main".into());
        let snapshot = App::pod_snapshot(&state);
        assert_eq!(snapshot.debug_target.as_deref(), Some("main"));
        app.restore_kube_snapshot(filar_core::KubeSessionSnapshot {
            target: "prod-eu".into(),
            namespace: Some("shop".into()),
            pod: Some(snapshot),
        });
        assert_eq!(app.kube.as_ref().unwrap().pod.as_ref().unwrap().debug_target.as_deref(), Some("main"));
    }
}
