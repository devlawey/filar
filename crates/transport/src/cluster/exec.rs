//! `KubeExecutor` — a persistent shell inside a pod's container (#563).
//!
//! One `exec` of the container's shell (`/bin/sh` by default) with stdin,
//! stdout and stderr and **no TTY** stays open for the life of the tab; every
//! command goes through it with the same marker protocol as SSH
//! ([`crate::shell_protocol`]), so `cd` and `export` persist between commands.
//! A running command is interrupted with a second, short `exec` that signals
//! the shell's children; if that cannot be done the shell is recreated and the
//! caller is told that its state was lost.
//!
//! Nothing is written to the pod or the cluster (invariant 1): the shell comes
//! from the image, commands go through stdin, and this executor never creates
//! ephemeral containers — that is a separate explicit user action (#576).
//! Errors name the context and namespace, never a token, certificate or
//! kubeconfig contents.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use filar_core::{
    CoreError, KubeCluster, PodLimits, PodRef, PodSelector, Result, DEFAULT_COMMAND_TIMEOUT_SECS,
};
use k8s_openapi::api::core::v1::Pod;
use kube::api::{AttachParams, AttachedProcess};
use kube::{Api, Client};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, Mutex, RwLock};
use tracing::{debug, info, warn};

use super::client::{api_error, build_client, is_rbac_message, KubeClusterApi};
use super::{convert, ClusterApi, PodInfo};
use crate::shell_protocol::{
    command_marker_tag, command_payload, interrupt_command, read_shell_pid, recv_until_marker,
    recv_until_marker_simple, shell_pid_request, sync_request, ChannelEvent,
};
use crate::CommandResult;

/// Shell started in the container when the configuration names none.
pub const DEFAULT_POD_SHELL: &str = "/bin/sh";

/// Pod annotation naming the container `kubectl exec` enters by default.
const DEFAULT_CONTAINER_ANNOTATION: &str = "kubectl.kubernetes.io/default-container";

/// How long the shell may take to answer the startup sync marker.
const STARTUP_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the interrupting `exec` may take.
const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(5);

/// Message returned when a command could only be stopped by recreating the shell.
pub const SHELL_RECREATED_MESSAGE: &str = "прервано пересозданием шелла: cd и переменные сброшены";

/// Tunables of [`KubeExecutor`].
#[derive(Debug, Clone)]
pub struct KubeExecConfig {
    /// Shell executed in the container.
    pub shell: String,
    /// Per-command marker-wait timeout.
    pub command_timeout: Duration,
    /// One silent reconnect when the stream closed before a command was sent.
    pub auto_reconnect: bool,
}

impl Default for KubeExecConfig {
    fn default() -> Self {
        Self {
            shell: DEFAULT_POD_SHELL.into(),
            command_timeout: Duration::from_secs(DEFAULT_COMMAND_TIMEOUT_SECS),
            auto_reconnect: true,
        }
    }
}

/// What happened to a pod shell, for the UI (#565).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KubeExecEvent {
    /// The shell is open in `pod`/`container`; `limits` come from the pod spec.
    Connected {
        /// Pod name.
        pod: String,
        /// Container name.
        container: String,
        /// Limits and security settings of the container.
        limits: PodLimits,
    },
    /// The pod disappeared and the shell moved to another pod picked by the
    /// same selector; `cd` and variables were reset.
    PodChanged {
        /// Pod the shell was in.
        old: String,
        /// Pod the shell is in now.
        new: String,
    },
}

/// Pod, container and limits the shell currently runs in.
#[derive(Debug, Clone)]
struct ShellIdentity {
    pod: String,
    container: String,
    limits: PodLimits,
}

// ---------------------------------------------------------------------------
// Pure selection and classification (unit-tested)
// ---------------------------------------------------------------------------

/// First `Running` + `Ready` pod by name, or an error naming the selector,
/// namespace and context.
fn select_ready_pod(
    pods: &[PodInfo],
    selector: &str,
    namespace: &str,
    context: &str,
) -> Result<String> {
    let mut ready: Vec<&PodInfo> = pods
        .iter()
        .filter(|pod| pod.phase == "Running" && pod.ready)
        .collect();
    ready.sort_by(|a, b| a.name.cmp(&b.name));
    ready.first().map(|pod| pod.name.clone()).ok_or_else(|| {
        CoreError::NotFound(format!(
            "по селектору {selector} в {namespace} нет готовых подов (контекст {context})"
        ))
    })
}

/// An error when `pod` cannot take a shell: not `Running` + `Ready`.
fn ensure_ready(pod: &PodInfo, context: &str) -> Result<()> {
    if pod.phase == "Running" && pod.ready {
        return Ok(());
    }
    Err(CoreError::Unavailable(format!(
        "под {}/{} не готов (phase {}, Ready={}) (контекст {context})",
        pod.namespace, pod.name, pod.phase, pod.ready
    )))
}

/// The container to enter: the requested one, else the only one, else the
/// `kubectl.kubernetes.io/default-container` annotation, else an error that
/// lists the containers.
fn select_container(pod: &Pod, requested: Option<&str>) -> Result<String> {
    let names: Vec<&str> = pod
        .spec
        .as_ref()
        .map(|spec| spec.containers.iter().map(|c| c.name.as_str()).collect())
        .unwrap_or_default();
    let pod_name = pod.metadata.name.as_deref().unwrap_or("?");
    let listed = || names.join(", ");
    if let Some(requested) = requested {
        return if names.contains(&requested) {
            Ok(requested.to_string())
        } else {
            Err(CoreError::NotFound(format!(
                "в поде {pod_name} нет контейнера {requested}; есть: {}",
                listed()
            )))
        };
    }
    if let [only] = names.as_slice() {
        return Ok((*only).to_string());
    }
    if let Some(default) = pod
        .metadata
        .annotations
        .as_ref()
        .and_then(|annotations| annotations.get(DEFAULT_CONTAINER_ANNOTATION))
        .filter(|name| names.contains(&name.as_str()))
    {
        return Ok(default.clone());
    }
    Err(CoreError::Other(format!(
        "в поде {pod_name} несколько контейнеров — выберите один: {}",
        listed()
    )))
}

/// [`PodLimits`] of `container` from the pod spec: resources of the container,
/// `runAsUser`/`runAsNonRoot` of the container falling back to the pod,
/// `readOnlyRootFilesystem` and dropped capabilities of the container.
fn pod_limits(pod: &Pod, container: &str) -> PodLimits {
    let Some(spec) = pod.spec.as_ref() else {
        return PodLimits::default();
    };
    let Some(c) = spec.containers.iter().find(|c| c.name == container) else {
        return PodLimits::default();
    };
    let quantity = |map: Option<&std::collections::BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>>,
                    key: &str| map.and_then(|m| m.get(key)).map(|q| q.0.clone());
    let resources = c.resources.as_ref();
    let limits = resources.and_then(|r| r.limits.as_ref());
    let requests = resources.and_then(|r| r.requests.as_ref());
    let csc = c.security_context.as_ref();
    let psc = spec.security_context.as_ref();
    PodLimits {
        memory_limit: quantity(limits, "memory"),
        memory_request: quantity(requests, "memory"),
        cpu_limit: quantity(limits, "cpu"),
        cpu_request: quantity(requests, "cpu"),
        run_as_user: csc
            .and_then(|s| s.run_as_user)
            .or_else(|| psc.and_then(|s| s.run_as_user)),
        run_as_non_root: csc
            .and_then(|s| s.run_as_non_root)
            .or_else(|| psc.and_then(|s| s.run_as_non_root)),
        read_only_root_filesystem: csc.and_then(|s| s.read_only_root_filesystem),
        dropped_capabilities: csc
            .and_then(|s| s.capabilities.as_ref())
            .and_then(|caps| caps.drop.clone())
            .unwrap_or_default(),
    }
}

/// `(verb, resource)` from an RBAC denial such as
/// `… cannot create resource "pods/exec" in API group "" …`.
fn rbac_verb_and_resource(message: &str) -> Option<(String, String)> {
    let after = message.split(" cannot ").nth(1)?;
    let verb = after.split_whitespace().next()?.to_string();
    let resource = after.split("resource \"").nth(1)?.split('"').next()?.to_string();
    Some((verb, resource))
}

/// A 403 from the API as a user-facing message: RBAC names the missing verb
/// and resource; anything else is an admission policy, whose own text says
/// why. The requesting user's name is never repeated.
///
/// `requested` is the `(verb, resource)` of the refused request, used when the
/// API text cannot be parsed — it is what was asked for, never a guess.
fn forbidden_error(
    message: &str,
    requested: (&str, &str),
    namespace: &str,
    context: &str,
) -> CoreError {
    if is_rbac_message(message) {
        let (verb, resource) = rbac_verb_and_resource(message)
            .unwrap_or_else(|| (requested.0.to_string(), requested.1.to_string()));
        CoreError::AccessDenied(format!(
            "нет права {verb} на {resource} в namespace {namespace} (контекст {context})"
        ))
    } else {
        CoreError::AccessDenied(format!("отказала политика допуска: {message}"))
    }
}

/// Whether an exec failure text says the shell binary is missing.
fn is_missing_shell(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("executable file not found") || lower.contains("no such file or directory")
}

/// The error for a shell that could not be started in the container.
fn missing_shell_error(shell: &str, namespace: &str, pod: &str, context: &str) -> CoreError {
    CoreError::Unavailable(format!(
        "в контейнере нет {shell} — войти можно через эфемерный debug-контейнер (#576) \
         (под {namespace}/{pod}, контекст {context})"
    ))
}

// ---------------------------------------------------------------------------
// One exec stream
// ---------------------------------------------------------------------------

/// Where a pod shell runs; enough to open it again or to send an interrupt.
#[derive(Clone)]
struct ExecPlace {
    pods: Api<Pod>,
    client: Client,
    namespace: String,
    pod: String,
    container: String,
    shell: String,
    context: String,
}

impl ExecPlace {
    /// Map an `exec` failure to a message. A refused WebSocket upgrade does not
    /// carry the API's `Status`, so a 403 there is probed once more as a plain
    /// request (which the API authorizes and admits the same way but refuses
    /// to upgrade — nothing runs) to tell RBAC from an admission policy.
    async fn exec_error(&self, error: kube::Error, command: &[String]) -> CoreError {
        match error {
            kube::Error::Api(status) if status.code == 403 => {
                forbidden_error(&status.message, ("create", "pods/exec"), &self.namespace, &self.context)
            }
            kube::Error::UpgradeConnection(kube::client::UpgradeConnectionError::ProtocolSwitch(
                code,
            )) if code.as_u16() == 403 => match self.probe_denial(command).await {
                Some(message) => {
                    forbidden_error(&message, ("create", "pods/exec"), &self.namespace, &self.context)
                }
                None => CoreError::AccessDenied(format!(
                    "нет права create на pods/exec в namespace {} (контекст {})",
                    self.namespace, self.context
                )),
            },
            kube::Error::UpgradeConnection(kube::client::UpgradeConnectionError::ProtocolSwitch(
                code,
            )) if code.as_u16() == 404 => CoreError::NotFound(format!(
                "под {}/{} не найден (контекст {})",
                self.namespace, self.pod, self.context
            )),
            other => {
                let fallback = KubeCluster {
                    name: String::new(),
                    context: self.context.clone(),
                    ..KubeCluster::default()
                };
                api_error(&fallback, other, "create", "pods/exec", Some(&self.namespace))
            }
        }
    }

    /// Repeat a refused exec as a plain `POST` to read the API's denial text.
    async fn probe_denial(&self, command: &[String]) -> Option<String> {
        let params = self.attach_params(true);
        let mut request = kube::core::Request::new(format!(
            "/api/v1/namespaces/{}/pods",
            self.namespace
        ))
        .exec(&self.pod, command.to_vec(), &params)
        .ok()?;
        if let Ok(method) = "POST".parse() {
            *request.method_mut() = method;
        }
        match self.client.request_text(request).await {
            Err(kube::Error::Api(status)) if status.code == 403 => Some(status.message),
            _ => None,
        }
    }

    fn attach_params(&self, stdin: bool) -> AttachParams {
        AttachParams::default()
            .container(self.container.clone())
            .stdin(stdin)
            .stdout(true)
            .stderr(true)
            .tty(false)
    }

    /// Run the interrupt for shell `pid` in a second exec and report whether
    /// the host confirmed it (exit 0 within [`INTERRUPT_TIMEOUT`]).
    async fn interrupt(&self, pid: u32) -> bool {
        let command = vec![self.shell.clone(), "-c".into(), interrupt_command(pid)];
        let attempt = async {
            let mut process = self
                .pods
                .exec(&self.pod, command, &self.attach_params(false))
                .await
                .ok()?;
            let status = process.take_status()?;
            status.await
        };
        match tokio::time::timeout(INTERRUPT_TIMEOUT, attempt).await {
            Ok(Some(status)) => status.status.as_deref() == Some("Success"),
            _ => false,
        }
    }
}

/// Decode a byte stream as UTF-8 without splitting a character across reads.
#[derive(Default)]
struct Utf8Stream {
    pending: Vec<u8>,
}

impl Utf8Stream {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let keep = match std::str::from_utf8(&self.pending) {
            Ok(_) => 0,
            // An incomplete sequence at the end waits for the next read.
            Err(e) if e.error_len().is_none() => self.pending.len() - e.valid_up_to(),
            Err(_) => 0,
        };
        let split = self.pending.len() - keep;
        let text = String::from_utf8_lossy(&self.pending[..split]).into_owned();
        self.pending.drain(..split);
        text
    }
}

/// Forward stdout and stderr of the exec as [`ChannelEvent`]s; mark the
/// stream closed and send [`ChannelEvent::Closed`] when both end.
async fn read_streams(
    mut stdout: impl AsyncRead + Unpin,
    mut stderr: impl AsyncRead + Unpin,
    events: mpsc::UnboundedSender<ChannelEvent>,
    closed: Arc<AtomicBool>,
) {
    let (mut out_buf, mut err_buf) = (vec![0u8; 8192], vec![0u8; 8192]);
    let (mut out_text, mut err_text) = (Utf8Stream::default(), Utf8Stream::default());
    let (mut out_open, mut err_open) = (true, true);
    while out_open || err_open {
        tokio::select! {
            read = stdout.read(&mut out_buf), if out_open => match read {
                Ok(0) | Err(_) => out_open = false,
                Ok(n) => {
                    let text = out_text.push(&out_buf[..n]);
                    if !text.is_empty() && events.send(ChannelEvent::Data(text)).is_err() {
                        return;
                    }
                }
            },
            read = stderr.read(&mut err_buf), if err_open => match read {
                Ok(0) | Err(_) => err_open = false,
                Ok(n) => {
                    let text = err_text.push(&err_buf[..n]);
                    if !text.is_empty() && events.send(ChannelEvent::Stderr(text)).is_err() {
                        return;
                    }
                }
            },
        }
    }
    closed.store(true, Ordering::Relaxed);
    debug!("pod shell streams closed");
    let _ = events.send(ChannelEvent::Closed);
}

/// A live shell in one container.
struct PodShell {
    place: ExecPlace,
    write_tx: mpsc::Sender<Vec<u8>>,
    event_rx: Mutex<mpsc::UnboundedReceiver<ChannelEvent>>,
    run_lock: Mutex<()>,
    req_counter: AtomicU64,
    shell_pid: Option<u32>,
    closed: Arc<AtomicBool>,
    /// The exec itself, kept so the stream can be torn down (`abort`).
    process: StdMutex<Option<AttachedProcess>>,
    command_timeout: Duration,
}

impl PodShell {
    async fn open(place: ExecPlace, command_timeout: Duration) -> Result<Self> {
        let command = vec![place.shell.clone()];
        let mut process = match place
            .pods
            .exec(&place.pod, command.clone(), &place.attach_params(true))
            .await
        {
            Ok(process) => process,
            Err(error) => return Err(place.exec_error(error, &command).await),
        };
        let (Some(stdin), Some(stdout), Some(stderr)) =
            (process.stdin(), process.stdout(), process.stderr())
        else {
            return Err(CoreError::Other("pod exec did not provide stdin/stdout/stderr".into()));
        };

        let closed = Arc::new(AtomicBool::new(false));
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        tokio::spawn(read_streams(stdout, stderr, event_tx, closed.clone()));

        let (write_tx, mut write_rx) = mpsc::channel::<Vec<u8>>(16);
        tokio::spawn(async move {
            let mut stdin = stdin;
            while let Some(bytes) = write_rx.recv().await {
                if stdin.write_all(&bytes).await.is_err() || stdin.flush().await.is_err() {
                    warn!("pod shell: failed to write to stdin");
                    break;
                }
            }
        });

        let exit_status = Arc::new(StdMutex::new(None));
        if let Some(status) = process.take_status() {
            let slot = exit_status.clone();
            tokio::spawn(async move {
                if let Some(status) = status.await {
                    if status.status.as_deref() != Some("Success") {
                        let message = status
                            .message
                            .or(status.reason)
                            .unwrap_or_else(|| "exec failed".into());
                        *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(message);
                    }
                }
            });
        }

        // Sync: the shell answers a marker, or it never started.
        let (sync_cmd, sync_marker) = sync_request();
        let synced = match write_tx.send(sync_cmd.into_bytes()).await {
            Ok(()) => recv_until_marker_simple(&mut event_rx, &sync_marker, STARTUP_TIMEOUT).await,
            Err(_) => Err(CoreError::Other("pod shell stdin closed".into())),
        };
        if let Err(error) = synced {
            // The exec's status may trail the stream closure slightly.
            let deadline = Instant::now() + Duration::from_millis(500);
            let failure = loop {
                let failure = exit_status.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if failure.is_some() || Instant::now() >= deadline {
                    break failure;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            };
            return Err(match failure {
                Some(message) if is_missing_shell(&message) => {
                    missing_shell_error(&place.shell, &place.namespace, &place.pod, &place.context)
                }
                Some(_) | None => CoreError::Unavailable(format!(
                    "шелл {} в поде {}/{} не ответил: {error} (контекст {})",
                    place.shell, place.namespace, place.pod, place.context
                )),
            });
        }

        let (pid_cmd, pid_tag) = shell_pid_request();
        let shell_pid = match write_tx.send(pid_cmd.into_bytes()).await {
            Ok(()) => read_shell_pid(&mut event_rx, &pid_tag, Duration::from_secs(10)).await,
            Err(_) => None,
        };
        if shell_pid.is_none() {
            warn!("pod shell did not report its PID — an interrupt will recreate the shell");
        }
        info!(namespace = %place.namespace, pod = %place.pod, container = %place.container, "pod shell ready");

        Ok(Self {
            place,
            write_tx,
            event_rx: Mutex::new(event_rx),
            run_lock: Mutex::new(()),
            req_counter: AtomicU64::new(0),
            shell_pid,
            closed,
            process: StdMutex::new(Some(process)),
            command_timeout,
        })
    }

    async fn run(&self, command: &str) -> Result<CommandResult> {
        let _guard = self.run_lock.lock().await;
        let marker_tag = command_marker_tag(self.req_counter.fetch_add(1, Ordering::Relaxed));
        let payload = command_payload(command, &marker_tag);
        let start = Instant::now();

        let mut event_rx = self.event_rx.lock().await;
        while let Ok(Some(event)) =
            tokio::time::timeout(Duration::from_millis(10), event_rx.recv()).await
        {
            // Metadata only — output may contain secrets.
            let (kind, len) = match &event {
                ChannelEvent::Data(text) => ("stdout", text.len()),
                ChannelEvent::Stderr(text) => ("stderr", text.len()),
                ChannelEvent::Closed => ("closed", 0),
            };
            debug!(kind, len, "draining stale pod shell event");
        }

        // The stream is gone and nothing has been sent: the one point where a
        // reconnect and retry are safe.
        if self.closed.load(Ordering::Relaxed) {
            return Err(CoreError::ConnectionLost("pod shell stream closed".into()));
        }
        self.write_tx
            .send(payload.into_bytes())
            .await
            .map_err(|_| CoreError::ConnectionLost("pod shell stdin closed".into()))?;

        match recv_until_marker(&mut *event_rx, &marker_tag, self.command_timeout).await {
            Ok((stdout, stderr, exit_code, cwd)) => Ok(CommandResult {
                stdout,
                stderr,
                exit_code,
                duration: start.elapsed(),
                cwd,
            }),
            Err(error) if self.closed.load(Ordering::Relaxed) => Err(error),
            Err(error) => {
                warn!(error = %error, "pod command timed out, interrupting it and resyncing");
                if let Err(e) = self.cancel().await {
                    warn!(error = %e, "could not interrupt the timed-out pod command");
                    return Err(error);
                }
                let (sync_cmd, sync_marker) = sync_request();
                let _ = self.write_tx.send(sync_cmd.into_bytes()).await;
                let _ =
                    recv_until_marker_simple(&mut *event_rx, &sync_marker, Duration::from_secs(10))
                        .await;
                Err(error)
            }
        }
    }

    /// Interrupt the running command; when that cannot be confirmed, tear the
    /// stream down so the command dies with the shell and report that the
    /// shell's state is lost. Takes no lock that `run` holds.
    async fn cancel(&self) -> Result<()> {
        if let Some(pid) = self.shell_pid {
            if self.place.interrupt(pid).await {
                return Ok(());
            }
        }
        warn!("pod interrupt failed — recreating the shell");
        self.abort();
        Err(CoreError::Other(SHELL_RECREATED_MESSAGE.into()))
    }

    fn abort(&self) {
        self.closed.store(true, Ordering::Relaxed);
        if let Some(process) = self.process.lock().unwrap_or_else(|e| e.into_inner()).take() {
            process.abort();
        }
    }
}

impl Drop for PodShell {
    fn drop(&mut self) {
        self.abort();
    }
}

// ---------------------------------------------------------------------------
// KubeExecutor
// ---------------------------------------------------------------------------

/// [`CommandExecutor`](crate::CommandExecutor) with a persistent shell in a
/// pod's container, reached through the Kubernetes API `exec` (#563).
///
/// The trait contract is the same as for SSH. Kubernetes-specific facts —
/// the current pod and container, [`PodLimits`], pod changes — are exposed
/// as methods of this type and as [`KubeExecEvent`]s, not through the trait.
pub struct KubeExecutor {
    cluster: KubeCluster,
    target: PodRef,
    config: KubeExecConfig,
    api: KubeClusterApi,
    stream_client: Client,
    shell: RwLock<PodShell>,
    identity: StdMutex<ShellIdentity>,
    pod_change: StdMutex<Option<(String, String)>>,
    events: broadcast::Sender<KubeExecEvent>,
    last_cwd: StdMutex<Option<String>>,
    pending_cwd: StdMutex<Option<String>>,
}

impl KubeExecutor {
    /// Open a shell in the pod `target` points at, in `cluster` (its explicit
    /// context; kubeconfig, proxy and auth plugins as for
    /// [`KubeClusterApi`]), with the default [`KubeExecConfig`].
    pub async fn connect(cluster: &KubeCluster, target: &PodRef) -> Result<Self> {
        Self::connect_with_config(cluster, target, KubeExecConfig::default()).await
    }

    /// [`connect`](Self::connect) with explicit tunables.
    pub async fn connect_with_config(
        cluster: &KubeCluster,
        target: &PodRef,
        config: KubeExecConfig,
    ) -> Result<Self> {
        let api = KubeClusterApi::connect(cluster).await?;
        let stream_client = build_client(cluster, true).await?;
        let (shell, identity) =
            open_shell(&api, &stream_client, cluster, target, &config, None).await?;
        let (events, _) = broadcast::channel(16);
        let executor = Self {
            cluster: cluster.clone(),
            target: target.clone(),
            config,
            api,
            stream_client,
            shell: RwLock::new(shell),
            identity: StdMutex::new(identity.clone()),
            pod_change: StdMutex::new(None),
            events,
            last_cwd: StdMutex::new(None),
            pending_cwd: StdMutex::new(None),
        };
        let _ = executor.events.send(KubeExecEvent::Connected {
            pod: identity.pod,
            container: identity.container,
            limits: identity.limits,
        });
        Ok(executor)
    }

    /// Pod the shell runs in now.
    pub fn pod(&self) -> String {
        self.identity().pod
    }

    /// Container the shell runs in.
    pub fn container(&self) -> String {
        self.identity().container
    }

    /// Limits and security settings of the container, from the pod spec.
    pub fn pod_limits(&self) -> PodLimits {
        self.identity().limits
    }

    /// The target as the agent should see it, via
    /// [`KubeCluster::describe_pod`].
    pub fn describe(&self) -> String {
        let identity = self.identity();
        self.cluster
            .describe_pod(&self.target.namespace, &identity.pod, &identity.container)
    }

    /// The last pod change `(old, new)`, cleared by reading it.
    pub fn take_pod_change(&self) -> Option<(String, String)> {
        self.pod_change.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// Receive [`KubeExecEvent`]s from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<KubeExecEvent> {
        self.events.subscribe()
    }

    /// Close the shell.
    pub async fn close(&self) -> Result<()> {
        self.shell.read().await.abort();
        Ok(())
    }

    fn identity(&self) -> ShellIdentity {
        self.identity.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Open a fresh shell (same pod if it is still ready; for a selector,
    /// another ready pod otherwise) and install it.
    async fn reopen(&self) -> Result<()> {
        let mut guard = self.shell.write().await;
        let current = self.identity();
        let (fresh, identity) = open_shell(
            &self.api,
            &self.stream_client,
            &self.cluster,
            &self.target,
            &self.config,
            Some(&current),
        )
        .await?;
        *guard = fresh;
        drop(guard);
        *self.identity.lock().unwrap_or_else(|e| e.into_inner()) = identity.clone();
        if identity.pod != current.pod {
            warn!(
                "под {} пропал, вкладка теперь в {} — cd и переменные сброшены",
                current.pod, identity.pod
            );
            *self.pod_change.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((current.pod.clone(), identity.pod.clone()));
            let _ = self.events.send(KubeExecEvent::PodChanged {
                old: current.pod,
                new: identity.pod.clone(),
            });
        } else {
            warn!(
                "шелл в поде {} переоткрыт — cd и переменные сброшены",
                identity.pod
            );
        }
        let _ = self.events.send(KubeExecEvent::Connected {
            pod: identity.pod,
            container: identity.container,
            limits: identity.limits,
        });
        Ok(())
    }

    fn prefixed_command(&self, command: &str) -> String {
        let pending = lock(&self.pending_cwd).clone();
        match pending.as_deref().and_then(crate::posix_cd_command) {
            Some(cd) => format!("{cd} && {command}"),
            None => command.to_string(),
        }
    }

    fn settle(&self, result: &Result<CommandResult>) {
        if !matches!(result, Err(CoreError::ConnectionLost(_))) {
            *lock(&self.pending_cwd) = None;
        }
        if let Ok(r) = result {
            if let Some(cwd) = r.cwd.as_deref().filter(|p| crate::is_safe_cwd(p)) {
                *lock(&self.last_cwd) = Some(cwd.to_string());
            }
        }
    }
}

fn lock<T>(m: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Resolve the pod and container for `target` and open a shell there.
/// `current` is the identity being replaced on a reconnect.
async fn open_shell(
    api: &KubeClusterApi,
    stream_client: &Client,
    cluster: &KubeCluster,
    target: &PodRef,
    config: &KubeExecConfig,
    current: Option<&ShellIdentity>,
) -> Result<(PodShell, ShellIdentity)> {
    let namespace = target.namespace.as_str();
    let context = cluster.context.as_str();
    let pods: Api<Pod> = Api::namespaced(api.client().clone(), namespace);
    let get = |name: String| {
        let pods = pods.clone();
        async move {
            pods.get(&name)
                .await
                .map_err(|error| pod_api_error(cluster, error, "get", namespace))
        }
    };

    let pod = match &target.pod {
        PodSelector::Name(name) => get(name.clone()).await?,
        PodSelector::Labels(selector) => {
            // Stay in the current pod while it is still ready.
            let kept = match current {
                Some(current) => match get(current.pod.clone()).await {
                    Ok(pod) if ensure_ready(&convert::pod_info(pod.clone()), context).is_ok() => {
                        Some(pod)
                    }
                    _ => None,
                },
                None => None,
            };
            match kept {
                Some(pod) => pod,
                None => {
                    let listed = api
                        .list_pods(Some(namespace), Some(selector), None)
                        .await?;
                    get(select_ready_pod(&listed, selector, namespace, context)?).await?
                }
            }
        }
    };
    ensure_ready(&convert::pod_info(pod.clone()), context)?;
    let pod_name = pod.metadata.name.clone().unwrap_or_default();
    let requested = target
        .container
        .as_deref()
        .or(current.map(|c| c.container.as_str()));
    let container = select_container(&pod, requested)?;
    let limits = pod_limits(&pod, &container);

    let place = ExecPlace {
        pods: Api::namespaced(stream_client.clone(), namespace),
        client: stream_client.clone(),
        namespace: namespace.to_string(),
        pod: pod_name.clone(),
        container: container.clone(),
        shell: config.shell.clone(),
        context: context.to_string(),
    };
    let shell = PodShell::open(place, config.command_timeout).await?;
    Ok((
        shell,
        ShellIdentity {
            pod: pod_name,
            container,
            limits,
        },
    ))
}

/// A pods API error with RBAC and policy denials told apart.
fn pod_api_error(cluster: &KubeCluster, error: kube::Error, verb: &str, namespace: &str) -> CoreError {
    match error {
        kube::Error::Api(status) if status.code == 403 => {
            forbidden_error(&status.message, (verb, "pods"), namespace, &cluster.context)
        }
        other => api_error(cluster, other, verb, "pods", Some(namespace)),
    }
}

#[async_trait::async_trait]
impl crate::CommandExecutor for KubeExecutor {
    async fn run(&self, command: &str) -> Result<CommandResult> {
        let command = self.prefixed_command(command);
        let first = { self.shell.read().await.run(&command).await };
        if !(self.config.auto_reconnect && matches!(first, Err(CoreError::ConnectionLost(_)))) {
            self.settle(&first);
            return first;
        }
        info!("pod shell closed before dispatch — attempting one silent reconnect");
        if let Err(error) = self.reopen().await {
            warn!(error = %error, "pod shell reconnect failed");
            return Err(error);
        }
        let retry = self.shell.read().await.run(&command).await;
        self.settle(&retry);
        retry
    }

    async fn cancel(&self) -> Result<()> {
        let result = self.shell.read().await.cancel().await;
        if result.is_err() {
            // The old shell is torn down; open a new one so the tab stays
            // usable. A running `run` releases its guard once its stream ends.
            match tokio::time::timeout(Duration::from_secs(10), self.reopen()).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => warn!(error = %error, "could not reopen the pod shell"),
                Err(_) => warn!("reopening the pod shell timed out; the next command will retry"),
            }
        }
        result
    }

    async fn set_cwd(&self, path: &str) -> Result<()> {
        let _ = crate::posix_cd_command(path)
            .ok_or_else(|| CoreError::Other("invalid cwd".into()))?;
        let path = path.trim().to_string();
        *lock(&self.pending_cwd) = Some(path.clone());
        *lock(&self.last_cwd) = Some(path);
        Ok(())
    }

    async fn current_cwd(&self) -> Option<String> {
        lock(&self.last_cwd).clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::core::v1::{
        Capabilities, Container, PodSecurityContext, PodSpec, ResourceRequirements,
        SecurityContext,
    };
    use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
    use std::collections::BTreeMap;

    fn info(name: &str, phase: &str, ready: bool) -> PodInfo {
        PodInfo {
            namespace: "shop".into(),
            name: name.into(),
            phase: phase.into(),
            ready,
            ready_containers: 0,
            container_count: 1,
            restarts: 0,
            last_termination: None,
            node: None,
            age_seconds: None,
            containers: Vec::new(),
        }
    }

    fn pod(containers: &[&str], annotation: Option<&str>) -> Pod {
        let mut pod = Pod::default();
        pod.metadata.name = Some("web-1".into());
        if let Some(default) = annotation {
            pod.metadata.annotations = Some(BTreeMap::from([(
                DEFAULT_CONTAINER_ANNOTATION.to_string(),
                default.to_string(),
            )]));
        }
        pod.spec = Some(PodSpec {
            containers: containers
                .iter()
                .map(|name| Container {
                    name: (*name).into(),
                    ..Container::default()
                })
                .collect(),
            ..PodSpec::default()
        });
        pod
    }

    #[test]
    fn the_first_ready_running_pod_by_name_is_chosen() {
        let pods = [
            info("web-c", "Running", true),
            info("web-a", "Running", false),
            info("web-b", "Pending", true),
            info("web-d", "Running", true),
        ];
        assert_eq!(select_ready_pod(&pods, "app=web", "shop", "prod-eu").unwrap(), "web-c");
    }

    #[test]
    fn no_ready_pod_names_selector_namespace_and_context() {
        let pods = [info("web-a", "Running", false)];
        let error = select_ready_pod(&pods, "app=web", "shop", "prod-eu").unwrap_err();
        assert_eq!(
            error.to_string(),
            CoreError::NotFound(
                "по селектору app=web в shop нет готовых подов (контекст prod-eu)".into()
            )
            .to_string()
        );
        assert!(select_ready_pod(&[], "app=web", "shop", "prod-eu").is_err());
    }

    #[test]
    fn a_pod_that_is_not_ready_is_refused() {
        assert!(ensure_ready(&info("web-a", "Running", true), "ctx").is_ok());
        let error = ensure_ready(&info("web-a", "Pending", false), "ctx").unwrap_err().to_string();
        assert!(error.contains("shop/web-a") && error.contains("не готов"), "{error}");
    }

    #[test]
    fn container_choice_follows_target_single_annotation_then_error() {
        assert_eq!(select_container(&pod(&["main", "side"], None), Some("side")).unwrap(), "side");
        let missing = select_container(&pod(&["main"], None), Some("nope")).unwrap_err().to_string();
        assert!(missing.contains("nope") && missing.contains("main"), "{missing}");
        assert_eq!(select_container(&pod(&["main"], None), None).unwrap(), "main");
        assert_eq!(
            select_container(&pod(&["main", "side"], Some("side")), None).unwrap(),
            "side"
        );
        let ambiguous = select_container(&pod(&["main", "side"], None), None)
            .unwrap_err()
            .to_string();
        assert!(ambiguous.contains("main, side"), "{ambiguous}");
        // An annotation naming no real container is ignored.
        assert!(select_container(&pod(&["main", "side"], Some("ghost")), None).is_err());
    }

    #[test]
    fn rbac_and_policy_denials_have_distinct_messages() {
        let rbac = "pods \"web-1\" is forbidden: User \"system:serviceaccount:shop:no-exec\" cannot create resource \"pods/exec\" in API group \"\" in the namespace \"shop\"";
        let error = forbidden_error(rbac, ("create", "pods/exec"), "shop", "prod-eu").to_string();
        assert!(
            error.contains("нет права create на pods/exec в namespace shop (контекст prod-eu)"),
            "{error}"
        );
        assert!(!error.contains("no-exec"), "the user is not repeated: {error}");

        let list = "pods is forbidden: User \"u\" cannot list resource \"pods\" in API group \"\" in the namespace \"shop\"";
        assert!(forbidden_error(list, ("get", "pods"), "shop", "c")
            .to_string()
            .contains("нет права list на pods"));
        // An RBAC text in an unexpected shape falls back to the refused request.
        let odd = "pods/log is forbidden: User \"u\" cannot get this resource in the namespace";
        assert!(forbidden_error(odd, ("get", "pods/log"), "shop", "c")
            .to_string()
            .contains("нет права get на pods/log в namespace shop"));

        let policy = "admission webhook \"validate.kyverno.svc\" denied the request: exec into pods is not allowed";
        let error = forbidden_error(policy, ("create", "pods/exec"), "shop", "prod-eu").to_string();
        assert!(error.contains("отказала политика допуска: "), "{error}");
        assert!(error.contains("exec into pods is not allowed"), "{error}");
    }

    #[test]
    fn a_missing_shell_is_recognised_and_points_to_debug_containers() {
        assert!(is_missing_shell(
            "OCI runtime exec failed: exec failed: unable to start container process: exec: \"/bin/sh\": stat /bin/sh: no such file or directory: unknown"
        ));
        assert!(is_missing_shell("executable file not found in $PATH"));
        assert!(!is_missing_shell("command terminated with non-zero exit code"));
        let error = missing_shell_error("/bin/sh", "shop", "distroless-1", "prod-eu").to_string();
        assert!(error.contains("в контейнере нет /bin/sh") && error.contains("#576"), "{error}");
        assert!(error.contains("shop/distroless-1") && error.contains("prod-eu"), "{error}");
    }

    #[test]
    fn errors_never_carry_credentials() {
        let cluster = KubeCluster {
            name: "prod-eu".into(),
            context: "prod-eu".into(),
            ..KubeCluster::default()
        };
        let source = std::io::Error::other("token=super-secret certificate=PRIVATE-KEY");
        let error =
            pod_api_error(&cluster, kube::Error::Service(source.into()), "get", "shop").to_string();
        assert!(!error.contains("super-secret") && !error.contains("PRIVATE-KEY"), "{error}");
        assert!(error.contains("prod-eu"), "{error}");
    }

    fn quantities(pairs: &[(&str, &str)]) -> Option<BTreeMap<String, Quantity>> {
        Some(
            pairs
                .iter()
                .map(|(k, v)| ((*k).to_string(), Quantity((*v).to_string())))
                .collect(),
        )
    }

    #[test]
    fn pod_limits_come_from_the_container_and_inherit_the_pod_security_context() {
        let mut p = pod(&["main", "side"], None);
        let spec = p.spec.as_mut().unwrap();
        spec.security_context = Some(PodSecurityContext {
            run_as_user: Some(65534),
            run_as_non_root: Some(true),
            ..PodSecurityContext::default()
        });
        spec.containers[0].resources = Some(ResourceRequirements {
            limits: quantities(&[("memory", "32Mi")]),
            requests: quantities(&[("cpu", "10m"), ("memory", "16Mi")]),
            ..ResourceRequirements::default()
        });
        spec.containers[0].security_context = Some(SecurityContext {
            read_only_root_filesystem: Some(true),
            capabilities: Some(Capabilities {
                drop: Some(vec!["ALL".into()]),
                add: None,
            }),
            ..SecurityContext::default()
        });
        spec.containers[1].security_context = Some(SecurityContext {
            run_as_user: Some(1000),
            run_as_non_root: Some(false),
            ..SecurityContext::default()
        });

        assert_eq!(
            pod_limits(&p, "main"),
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
        let side = pod_limits(&p, "side");
        assert_eq!(side.run_as_user, Some(1000), "the container overrides the pod");
        assert_eq!(side.run_as_non_root, Some(false));
        assert_eq!(side.memory_limit, None);
        assert!(side.dropped_capabilities.is_empty());
        assert_eq!(pod_limits(&pod(&["main"], None), "main"), PodLimits::default());
        assert_eq!(pod_limits(&p, "ghost"), PodLimits::default());
    }

    #[test]
    fn utf8_split_across_reads_is_reassembled() {
        let mut stream = Utf8Stream::default();
        let bytes = "дом".as_bytes();
        assert_eq!(stream.push(&bytes[..3]), "д");
        assert_eq!(stream.push(&bytes[3..]), "ом");
        assert_eq!(stream.push(&[0xff, b'a']), "\u{fffd}a");
    }
}
