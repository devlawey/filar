//! Kubernetes agent loop for cluster tabs (#575).
//!
//! The loop executes only typed, read-only [`KubeToolCall`] values. It never
//! adapts Kubernetes operations to shell commands or `CommandExecutor`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use filar_core::{CommandConfirmMode, CoreError, KubeCluster, Result, TagPolicy};
use tokio_util::sync::CancellationToken;

use crate::arbiter::{self, ArbiterContext, KubernetesArbiterTarget, ARBITER_TIMEOUT_SECS, HISTORY_TAIL_EXCHANGES};
use crate::fleet_gate::{ApprovalScope, FleetGate};
use crate::fleet_view::{FleetStatus, FleetView};
use crate::{
    build_kube_system_prompt, kube_confirmation_decision, kube_tool_definitions,
    kube_tools::build_kube_fleet_system_prompt, parse_kube_tool_call, AgentEvent, ChatMessage,
    ChatRequest, CommandConfirmer, ConfirmDecision, EventSink, KubeFleet, KubeToolExecutor,
    LlmClient, ToolCall,
};

const MAX_ITERATIONS: usize = 50;
const MAX_MISSING_EXPLANATION_RETRIES: u32 = 2;
static NEXT_PROPOSAL_ID: AtomicU64 = AtomicU64::new(1);

/// The agent's confirmer with the arbiter in front of it (#620): the audit
/// runs, its verdict is emitted, then the user is asked. Wrapping the
/// confirmer reaches the log-source paths too, which confirm on their own.
struct AuditedConfirmer<'a> {
    agent: &'a KubeAgent,
}

#[async_trait::async_trait]
impl CommandConfirmer for AuditedConfirmer<'_> {
    async fn confirm(&self, command: &str, explanation: &str, destructive: bool) -> Result<bool> {
        self.agent.audit(command, explanation, destructive).await;
        self.agent
            .confirmer
            .confirm(command, explanation, destructive)
            .await
    }
}

/// Receives the person-facing view and final status of a fleet operation.
pub type KubeFleetObserver = Arc<dyn Fn(FleetView, FleetStatus) + Send + Sync>;

enum KubeExecution {
    Cluster {
        executor: KubeToolExecutor,
        target: KubeCluster,
        namespace: Option<String>,
    },
    Fleet {
        fleet: Arc<KubeFleet>,
        observer: Option<KubeFleetObserver>,
    },
}

/// LLM ↔ read-only Kubernetes API orchestration for a cluster or frozen fleet.
pub struct KubeAgent {
    llm: Arc<dyn LlmClient>,
    execution: KubeExecution,
    confirmer: Arc<dyn CommandConfirmer>,
    confirm_mode: CommandConfirmMode,
    tag_policies: Vec<TagPolicy>,
    event_sink: Option<EventSink>,
    cancellation: Option<CancellationToken>,
    /// Log sources for `query_logs` (#579); cluster tabs only.
    log_sources: Option<Arc<crate::logs::LogSources>>,
    /// Independent command arbiter (#620), as in [`crate::Agent`].
    arbiter: Option<KubeArbiter>,
    /// The conversation as of the call being confirmed — the arbiter's
    /// "recent session context".
    conversation: Mutex<Vec<ChatMessage>>,
}

/// The arbiter of a [`KubeAgent`] (#620).
struct KubeArbiter {
    llm: Arc<dyn LlmClient>,
    /// Arbiter profile name, for `CommandAudited` (see `Agent`).
    model_name: String,
}

impl KubeAgent {
    /// Create a cluster agent for the selected target and default namespace.
    pub fn new(
        llm: Arc<dyn LlmClient>,
        executor: KubeToolExecutor,
        target: KubeCluster,
        namespace: Option<String>,
        confirmer: Arc<dyn CommandConfirmer>,
        confirm_mode: CommandConfirmMode,
        tag_policies: Vec<TagPolicy>,
    ) -> Self {
        Self {
            llm,
            execution: KubeExecution::Cluster {
                executor,
                target,
                namespace,
            },
            confirmer,
            confirm_mode,
            tag_policies,
            event_sink: None,
            cancellation: None,
            log_sources: None,
            arbiter: None,
            conversation: Mutex::new(Vec::new()),
        }
    }

    /// Create an agent whose every approved typed call covers one frozen fleet.
    pub fn new_fleet(
        llm: Arc<dyn LlmClient>,
        fleet: Arc<KubeFleet>,
        confirmer: Arc<dyn CommandConfirmer>,
        confirm_mode: CommandConfirmMode,
        tag_policies: Vec<TagPolicy>,
    ) -> Self {
        Self {
            llm,
            execution: KubeExecution::Fleet {
                fleet,
                observer: None,
            },
            confirmer,
            confirm_mode,
            tag_policies,
            event_sink: None,
            cancellation: None,
            log_sources: None,
            arbiter: None,
            conversation: Mutex::new(Vec::new()),
        }
    }

    /// Send full per-member answers only to the frontend's fleet panel.
    pub fn fleet_observer(mut self, observer: KubeFleetObserver) -> Self {
        if let KubeExecution::Fleet { observer: slot, .. } = &mut self.execution {
            *slot = Some(observer);
        }
        self
    }

    /// Log sources (#579, #583): `query_logs` in a cluster tab, and
    /// `pod_logs_history` in a cluster tab and in a fleet (where it is one
    /// query for all pods, asked once). A fleet never gets `query_logs`.
    pub fn log_sources(mut self, sources: Option<Arc<crate::logs::LogSources>>) -> Self {
        self.log_sources = sources.filter(|s| !s.is_empty());
        self
    }

    fn cluster_log_sources(&self) -> Option<&Arc<crate::logs::LogSources>> {
        match self.execution {
            KubeExecution::Cluster { .. } => self.log_sources.as_ref(),
            KubeExecution::Fleet { .. } => None,
        }
    }

    /// Audit every request this agent asks the user to confirm with an
    /// independent model (#620): typed `k8s_*` calls, `query_logs`,
    /// `pod_logs_history`, in a cluster tab and in a fleet. The verdict is
    /// emitted as [`AgentEvent::CommandAudited`] before the confirmation; it
    /// never blocks or approves anything, and a failed audit is reported as
    /// unavailable. `None` or `enabled = false` turns it off.
    pub fn arbiter(
        mut self,
        llm: Option<Arc<dyn LlmClient>>,
        enabled: bool,
        model_name: impl Into<String>,
    ) -> Self {
        let model_name = model_name.into();
        self.arbiter = llm
            .filter(|_| enabled)
            .map(|llm| KubeArbiter { llm, model_name });
        self
    }

    /// What the arbiter is told about the target.
    fn arbiter_target(&self) -> String {
        match &self.execution {
            KubeExecution::Cluster {
                target, namespace, ..
            } => arbiter::target_description(&ArbiterContext {
                is_local: false,
                ssh_info: None,
                kubernetes: Some(KubernetesArbiterTarget::new(target, namespace.as_deref())),
            }),
            KubeExecution::Fleet { fleet, .. } => format!(
                "Kubernetes fleet {} (read-only API, one approval for every member; {})",
                fleet.fleet().group_name(),
                fleet.confirmation_radius()
            ),
        }
    }

    /// Run the arbiter on a request about to be shown for confirmation.
    async fn audit(&self, command: &str, explanation: &str, destructive: bool) {
        if self.confirm_mode == CommandConfirmMode::Never {
            return;
        }
        let Some(audit_by) = &self.arbiter else {
            return;
        };
        let history_tail = {
            let conversation = self
                .conversation
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            arbiter::history_tail_from_messages(&conversation, HISTORY_TAIL_EXCHANGES)
        };
        let messages = arbiter::build_audit_messages(
            command,
            explanation,
            destructive,
            &self.arbiter_target(),
            &history_tail,
        );
        let audit = arbiter::run_audit(
            audit_by.llm.as_ref(),
            messages,
            Duration::from_secs(ARBITER_TIMEOUT_SECS),
            self.cancellation.as_ref(),
        )
        .await;
        self.emit(AgentEvent::CommandAudited {
            verdict: audit.verdict.label().to_string(),
            reason: audit.reason.clone(),
            arbiter_model: Some(audit_by.model_name.clone()),
            unavailable: audit.unavailable,
        });
        if audit.tokens_in > 0 || audit.tokens_out > 0 {
            self.emit(AgentEvent::TokenUsage {
                tokens_in: audit.tokens_in,
                tokens_out: audit.tokens_out,
                cost: audit.cost,
                model: audit.model,
                arbiter: true,
            });
        }
    }

    /// Deliver progress and tool events to a frontend.
    pub fn event_sink(mut self, sink: EventSink) -> Self {
        self.event_sink = Some(sink);
        self
    }

    /// Allow Ctrl+Z to cancel model, confirmation and API waits.
    pub fn cancellation(mut self, cancellation: CancellationToken) -> Self {
        self.cancellation = Some(cancellation);
        self
    }

    fn emit(&self, event: AgentEvent) {
        if let Some(sink) = &self.event_sink {
            sink(event);
        }
    }

    fn cancelled(&self) -> bool {
        self.cancellation
            .as_ref()
            .is_some_and(CancellationToken::is_cancelled)
    }

    async fn cancellable<F, T>(&self, future: F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        if let Some(token) = &self.cancellation {
            tokio::select! {
                _ = token.cancelled() => Err(CoreError::Other("cancelled".into())),
                result = future => result,
            }
        } else {
            future.await
        }
    }

    /// Run one user turn, including any approved Kubernetes API calls.
    pub async fn run(&self, user_prompt: &str, history: &[ChatMessage]) -> Result<String> {
        self.emit(AgentEvent::Started);
        let result = self.run_loop(user_prompt, history).await;
        match result {
            Ok(text) => {
                self.emit(AgentEvent::Finished(text.clone()));
                Ok(text)
            }
            Err(error) => {
                if self.cancelled() || error.to_string() == "cancelled" {
                    self.emit(AgentEvent::Cancelled);
                } else {
                    self.emit(AgentEvent::Error(error.to_string()));
                }
                Err(error)
            }
        }
    }

    async fn run_loop(&self, user_prompt: &str, history: &[ChatMessage]) -> Result<String> {
        let prompt = match &self.execution {
            KubeExecution::Cluster {
                target, namespace, ..
            } => build_kube_system_prompt(target, namespace.as_deref()),
            KubeExecution::Fleet { fleet, .. } => build_kube_fleet_system_prompt(fleet.fleet()),
        };
        let mut prompt = prompt;
        if let Some(sources) = self.cluster_log_sources() {
            prompt.push_str(&sources.prompt_section());
        }
        if self.log_sources.is_some() {
            prompt.push('\n');
            prompt.push_str(crate::logs::k8s::POD_LOGS_HISTORY_PROMPT);
            prompt.push('\n');
        }
        let mut messages = vec![ChatMessage::system(prompt)];
        messages.extend_from_slice(history);
        messages.push(ChatMessage::user(user_prompt));
        let mut tools = kube_tool_definitions(self.confirm_mode);
        // A fleet fans calls out to its members; one pod's HTTP endpoint is
        // a cluster-tab tool (#578).
        if matches!(self.execution, KubeExecution::Fleet { .. }) {
            tools.retain(|tool| tool.name != crate::pod_http::TOOL_POD_HTTP_GET);
        }
        if let Some(sources) = self.cluster_log_sources() {
            tools.push(sources.tool_definition(self.confirm_mode));
        }
        if self.log_sources.is_some() {
            let fleet = matches!(self.execution, KubeExecution::Fleet { .. });
            tools.push(crate::logs::k8s::pod_logs_history_definition(self.confirm_mode, fleet));
        }
        let mut missing_explanations = 0;

        for _ in 0..MAX_ITERATIONS {
            let request = ChatRequest {
                messages: messages.clone(),
                tools: tools.clone(),
            };
            let response = if let Some(sink) = &self.event_sink {
                let sink = sink.clone();
                let on_delta = move |delta: String| sink(AgentEvent::TextDelta(delta));
                self.cancellable(self.llm.chat_stream(&request, &on_delta))
                    .await?
            } else {
                self.cancellable(self.llm.chat(&request)).await?
            };

            if let Some(usage) = &response.usage {
                self.emit(AgentEvent::TokenUsage {
                    tokens_in: usage.prompt_tokens.unwrap_or(0),
                    tokens_out: usage.completion_tokens.unwrap_or(0),
                    cost: usage.cost,
                    model: response.model.clone(),
                    arbiter: false,
                });
            }

            if response.tool_calls.is_empty() {
                return Ok(response.text);
            }
            messages.push(ChatMessage::assistant_with_tools(
                &response.text,
                response.tool_calls.clone(),
            ));

            if self.confirm_mode == CommandConfirmMode::Explain {
                let errors = response
                    .tool_calls
                    .iter()
                    .filter_map(|call| {
                        explanation_error(call).map(|error| (call.id.clone(), error))
                    })
                    .collect::<Vec<_>>();
                if !errors.is_empty() {
                    missing_explanations += 1;
                    if missing_explanations > MAX_MISSING_EXPLANATION_RETRIES {
                        return Err(CoreError::Other(
                            "Agent repeatedly proposed Kubernetes calls without the required explanation. Stopping."
                                .into(),
                        ));
                    }
                    messages.extend(explanation_retry_messages(&response.tool_calls, &errors));
                    continue;
                }
                missing_explanations = 0;
            }

            for call in &response.tool_calls {
                *self
                    .conversation
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner()) = messages.clone();
                messages.push(self.process_tool_call(call).await?);
            }
        }

        Err(CoreError::Other(format!(
            "Kubernetes agent loop exceeded maximum iterations ({MAX_ITERATIONS})"
        )))
    }

    async fn process_tool_call(&self, call: &ToolCall) -> Result<ChatMessage> {
        // Every confirmation of this call — typed request or log query —
        // goes through the arbiter first (#620).
        let confirmer = AuditedConfirmer { agent: self };
        if call.name == crate::logs::k8s::TOOL_POD_LOGS_HISTORY {
            let Some(sources) = &self.log_sources else {
                return Ok(ChatMessage::tool(&call.id, "Error: no log sources are configured."));
            };
            let emit = |event: AgentEvent| self.emit(event);
            let proposal_id = NEXT_PROPOSAL_ID.fetch_add(1, Ordering::Relaxed);
            return match &self.execution {
                KubeExecution::Cluster { target, namespace, .. } => {
                    // The cluster's own tag policies tighten the gate as for
                    // every other request of this tab.
                    let mode = filar_core::tag_policy_floor(self.confirm_mode, &self.tag_policies, &target.tags)
                        .unwrap_or(self.confirm_mode);
                    sources
                        .process_pod_logs_history(
                            call,
                            &target.name,
                            namespace.as_deref().or(target.default_namespace.as_deref()),
                            None,
                            mode,
                            &confirmer,
                            &emit,
                            self.cancellation.as_ref(),
                            proposal_id,
                        )
                        .await
                }
                KubeExecution::Fleet { fleet, .. } => {
                    let members = fleet_log_members(fleet.fleet());
                    sources
                        .process_fleet_pod_logs(
                            call,
                            &members,
                            &confirmer,
                            &emit,
                            self.cancellation.as_ref(),
                            proposal_id,
                        )
                        .await
                }
            };
        }
        if call.name == crate::logs::TOOL_QUERY_LOGS {
            let Some(sources) = self.cluster_log_sources() else {
                return Ok(ChatMessage::tool(
                    &call.id,
                    "Error: no log sources are available here.",
                ));
            };
            let emit = |event: AgentEvent| self.emit(event);
            return sources
                .process(
                    call,
                    self.confirm_mode,
                    &confirmer,
                    &emit,
                    self.cancellation.as_ref(),
                    NEXT_PROPOSAL_ID.fetch_add(1, Ordering::Relaxed),
                )
                .await;
        }
        let default_namespace = match &self.execution {
            KubeExecution::Cluster { namespace, .. } => namespace.as_deref(),
            KubeExecution::Fleet { fleet, .. } => fleet
                .fleet()
                .group()
                .kube
                .as_ref()
                .map(|kube| kube.namespace.as_str()),
        };
        let parsed = match parse_kube_tool_call(&call.name, &call.arguments, default_namespace) {
            Ok(parsed) => parsed,
            Err(error) => {
                return Ok(ChatMessage::tool(
                    &call.id,
                    format!("Error: invalid Kubernetes tool call: {error}"),
                ));
            }
        };
        let display = parsed.display_command();
        let explanation = call
            .arguments
            .get("explanation")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Read-only Kubernetes API request")
            .to_string();
        let (confirmation_explanation, decision) = match &self.execution {
            KubeExecution::Cluster {
                target, namespace, ..
            } => {
                let target_namespace = parsed.namespace().or(namespace.as_deref());
                (
                    format!(
                        "{explanation}\n\n{}\nTarget: {} / {}",
                        parsed.confirmation_context(target),
                        target.name,
                        target_namespace.unwrap_or("all / cluster-scoped"),
                    ),
                    kube_confirmation_decision(self.confirm_mode, target, &self.tag_policies),
                )
            }
            KubeExecution::Fleet { fleet, .. } => {
                let decision = match fleet.gate(&parsed) {
                    FleetGate::Ask(ApprovalScope::Once) => ConfirmDecision::NeedsConfirmation,
                    FleetGate::Ask(ApprovalScope::PerHost) => ConfirmDecision::Blocked(
                        "Kubernetes fleets require one fleet-wide approval".into(),
                    ),
                    FleetGate::Refuse(reason) => ConfirmDecision::Blocked(reason),
                };
                (
                    format!(
                        "{explanation}\n\nTool: {}\nArguments: {}\nFleet: {} / {}",
                        parsed.tool_name(),
                        parsed.arguments(),
                        fleet.fleet().group_name(),
                        default_namespace.unwrap_or("all / cluster-scoped"),
                    ),
                    decision,
                )
            }
        };

        self.emit(AgentEvent::CommandProposed {
            proposal_id: NEXT_PROPOSAL_ID.fetch_add(1, Ordering::Relaxed),
            command: display.clone(),
            explanation: explanation.clone(),
            destructive: false,
            tool: filar_core::ToolInvocation {
                name: call.name.clone(),
                arguments: call.arguments.clone(),
            },
        });

        match decision {
            ConfirmDecision::NeedsConfirmation => {
                let approved = self
                    .cancellable(
                        confirmer.confirm(&display, &confirmation_explanation, false),
                    )
                    .await?;
                if !approved {
                    self.emit(AgentEvent::CommandFinished {
                        command: display,
                        output: String::new(),
                        denied: true,
                    });
                    return Ok(ChatMessage::tool(
                        &call.id,
                        "User denied this Kubernetes API request. Do not retry the same request; choose another approach or answer with what is known.",
                    ));
                }
            }
            ConfirmDecision::AutoApproved => {}
            ConfirmDecision::Blocked(reason) => {
                return Ok(ChatMessage::tool(&call.id, format!("Error: {reason}")));
            }
        }

        let outcome = match &self.execution {
            KubeExecution::Cluster { executor, .. } => self
                .cancellable(executor.execute(&parsed))
                .await
                .map(|result| result.content),
            KubeExecution::Fleet { fleet, observer } => {
                let approval = fleet.approve(&parsed)?;
                let cancellation = self
                    .cancellation
                    .clone()
                    .unwrap_or_else(CancellationToken::new);
                fleet
                    .run_until(&parsed, approval, &cancellation)
                    .await
                    .map(|report| {
                        if let Some(observer) = observer {
                            observer(report.view(&display), report.status());
                        }
                        report.render()
                    })
            }
        };
        match outcome {
            Ok(output) => {
                self.emit(AgentEvent::CommandFinished {
                    command: display,
                    output: output.clone(),
                    denied: false,
                });
                Ok(ChatMessage::tool(&call.id, output))
            }
            Err(error) if error.to_string() == "cancelled" => Err(error),
            Err(error) => {
                let output = format!("Kubernetes API request failed: {error}");
                self.emit(AgentEvent::CommandFinished {
                    command: display,
                    output: output.clone(),
                    denied: false,
                });
                Ok(ChatMessage::tool(&call.id, output))
            }
        }
    }
}

/// The fleet's Kubernetes members as `pod_logs_history` needs them; SSH
/// members have no pod logs and are left out.
fn fleet_log_members(fleet: &filar_core::FleetOperation) -> Vec<crate::logs::k8s::FleetLogMember> {
    fleet
        .members()
        .iter()
        .filter_map(|member| match member.target() {
            filar_core::FleetTarget::KubePod(pod) => Some(crate::logs::k8s::FleetLogMember {
                cluster: pod.cluster.clone(),
                namespace: pod.namespace.clone(),
                pod: Some(pod.pod.clone()),
            }),
            filar_core::FleetTarget::KubeCluster(scope) => Some(crate::logs::k8s::FleetLogMember {
                cluster: scope.cluster.clone(),
                namespace: scope.namespace.clone(),
                pod: None,
            }),
            filar_core::FleetTarget::Ssh(_) => None,
        })
        .collect()
}

fn explanation_retry_messages(calls: &[ToolCall], errors: &[(String, String)]) -> Vec<ChatMessage> {
    calls
        .iter()
        .map(|call| {
            let content = errors
                .iter()
                .find(|(id, _)| id == &call.id)
                .map(|(_, error)| error.clone())
                .unwrap_or_else(|| {
                    "Error: not executed because another call in this response lacked the required explanation. Repeat it with an explanation."
                        .to_string()
                });
            ChatMessage::tool(&call.id, content)
        })
        .collect()
}

fn explanation_error(call: &ToolCall) -> Option<String> {
    match call.arguments.get("explanation").and_then(serde_json::Value::as_str) {
        Some(text) if !text.trim().is_empty() => None,
        _ => Some(format!(
            "Error: {} requires a non-empty explanation in Explain mode; no Kubernetes request was executed.",
            call.name
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KubeResourceKind;

    #[test]
    fn get_display_is_command_like_and_typed() {
        let call = parse_kube_tool_call(
            "k8s_get",
            &serde_json::json!({"kind": "Pod", "label_selector": "app=web"}),
            None,
        )
        .unwrap();
        assert_eq!(call.display_command(), "k8s_get pod -l app=web --limit 50");
    }

    #[test]
    fn explain_retry_answers_every_tool_call_in_the_batch() {
        let calls = vec![
            ToolCall {
                id: "valid".into(),
                name: "k8s_get".into(),
                arguments: serde_json::json!({"kind": "Pod", "explanation": "Inspect pods"}),
            },
            ToolCall {
                id: "invalid".into(),
                name: "k8s_events".into(),
                arguments: serde_json::json!({}),
            },
        ];
        let messages =
            explanation_retry_messages(&calls, &[("invalid".into(), "missing explanation".into())]);

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].tool_call_id.as_deref(), Some("valid"));
        assert!(messages[0].content.contains("not executed"));
        assert_eq!(messages[1].tool_call_id.as_deref(), Some("invalid"));
        assert_eq!(messages[1].content, "missing explanation");
    }

    #[test]
    fn explain_mode_rejects_an_empty_explanation_before_execution() {
        let call = ToolCall {
            id: "1".into(),
            name: "k8s_get".into(),
            arguments: serde_json::json!({"kind": KubeResourceKind::Pod.as_str(), "explanation": " "}),
        };
        assert!(explanation_error(&call)
            .unwrap()
            .contains("no Kubernetes request was executed"));
    }

    // ── Arbiter (#620) ─────────────────────────────────────────────────

    use crate::kube_tools::tests::{executor, target, MockCluster};
    use crate::ChatResponse;

    /// Answers a scripted sequence; records every request.
    struct ScriptedLlm {
        answers: Mutex<Vec<ChatResponse>>,
        requests: Mutex<Vec<ChatRequest>>,
    }

    impl ScriptedLlm {
        fn new(mut answers: Vec<ChatResponse>) -> Arc<Self> {
            answers.reverse();
            Arc::new(Self {
                answers: Mutex::new(answers),
                requests: Mutex::new(Vec::new()),
            })
        }
    }

    #[async_trait::async_trait]
    impl LlmClient for ScriptedLlm {
        async fn chat(&self, request: &ChatRequest) -> Result<ChatResponse> {
            self.requests.lock().unwrap().push(request.clone());
            Ok(self
                .answers
                .lock()
                .unwrap()
                .pop()
                .unwrap_or_else(|| ChatResponse::text("Done.")))
        }
    }

    /// Approves, and records whether the audit was already emitted.
    struct RecordingConfirmer {
        events: Arc<Mutex<Vec<AgentEvent>>>,
        audited_before: Mutex<Vec<bool>>,
    }

    #[async_trait::async_trait]
    impl CommandConfirmer for RecordingConfirmer {
        async fn confirm(&self, _: &str, _: &str, _: bool) -> Result<bool> {
            let audited = self
                .events
                .lock()
                .unwrap()
                .iter()
                .any(|e| matches!(e, AgentEvent::CommandAudited { .. }));
            self.audited_before.lock().unwrap().push(audited);
            Ok(true)
        }
    }

    fn get_pods() -> ChatResponse {
        ChatResponse::tool_calls(
            "",
            vec![ToolCall {
                id: "call_1".into(),
                name: "k8s_get".into(),
                arguments: serde_json::json!({"kind": "Pod", "explanation": "List the pods in shop"}),
            }],
        )
    }

    async fn run_cluster_agent(
        mode: CommandConfirmMode,
        arbiter: Option<Arc<ScriptedLlm>>,
        enabled: bool,
    ) -> (Vec<AgentEvent>, Vec<bool>) {
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let confirmer = Arc::new(RecordingConfirmer {
            events: Arc::clone(&events),
            audited_before: Mutex::new(Vec::new()),
        });
        let sink_events = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |event| sink_events.lock().unwrap().push(event));
        let agent = KubeAgent::new(
            ScriptedLlm::new(vec![get_pods(), ChatResponse::text("Done.")]),
            executor(MockCluster::default()),
            target(),
            Some("shop".into()),
            Arc::clone(&confirmer) as Arc<dyn CommandConfirmer>,
            mode,
            Vec::new(),
        )
        .event_sink(sink)
        .arbiter(arbiter.map(|a| a as Arc<dyn LlmClient>), enabled, "judge");
        agent.run("show pods", &[]).await.unwrap();
        let events = events.lock().unwrap().clone();
        let audited_before = confirmer.audited_before.lock().unwrap().clone();
        (events, audited_before)
    }

    #[tokio::test]
    async fn a_cluster_request_is_audited_before_it_is_confirmed() {
        let arbiter = ScriptedLlm::new(vec![ChatResponse::text(
            r#"{"verdict":"MISMATCH","reason":"lists all namespaces"}"#,
        )]);
        let (events, audited_before) =
            run_cluster_agent(CommandConfirmMode::Always, Some(Arc::clone(&arbiter)), true).await;
        assert_eq!(audited_before, vec![true], "the verdict comes before the question");
        assert!(
            events.iter().any(|e| matches!(
                e,
                AgentEvent::CommandAudited { verdict, arbiter_model: Some(model), unavailable: false, .. }
                    if verdict == "MISMATCH" && model == "judge"
            )),
            "{events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(e, AgentEvent::CommandFinished { denied: false, .. })),
            "a verdict never denies: {events:?}"
        );
        let requests = arbiter.requests.lock().unwrap();
        let audit = &requests[0].messages[1].content;
        assert!(audit.contains("k8s_get pod"), "{audit}");
        assert!(audit.contains("Kubernetes cluster prod-eu (namespace shop, tags: prod)"), "{audit}");
        assert!(audit.contains("List the pods in shop"), "{audit}");
    }

    #[tokio::test]
    async fn a_fleet_request_is_audited_once_with_the_fleet_as_target() {
        use crate::kube_fleet::tests as fleet;
        let (eu, _) = fleet::cluster("prod-eu", vec![fleet::pod("web-1"), fleet::pod("web-2")]);
        let kube_fleet = KubeFleet::open(&fleet::group(&["prod-eu"], None), vec![eu])
            .await
            .unwrap();
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::new(Mutex::new(Vec::new()));
        let confirmer = Arc::new(RecordingConfirmer {
            events: Arc::clone(&events),
            audited_before: Mutex::new(Vec::new()),
        });
        let sink_events = Arc::clone(&events);
        let sink: EventSink = Arc::new(move |event| sink_events.lock().unwrap().push(event));
        let arbiter = ScriptedLlm::new(vec![ChatResponse::text(r#"{"verdict":"OK"}"#)]);
        let describe = ChatResponse::tool_calls(
            "",
            vec![ToolCall {
                id: "call_1".into(),
                name: "k8s_describe".into(),
                arguments: serde_json::json!({"kind": "Pod", "name": "x", "explanation": "Describe every pod"}),
            }],
        );
        KubeAgent::new_fleet(
            ScriptedLlm::new(vec![describe, ChatResponse::text("Done.")]),
            Arc::new(kube_fleet),
            Arc::clone(&confirmer) as Arc<dyn CommandConfirmer>,
            CommandConfirmMode::Always,
            Vec::new(),
        )
        .event_sink(sink)
        .arbiter(Some(Arc::clone(&arbiter) as Arc<dyn LlmClient>), true, "judge")
        .run("describe pods", &[])
        .await
        .unwrap();
        assert_eq!(*confirmer.audited_before.lock().unwrap(), vec![true]);
        let requests = arbiter.requests.lock().unwrap();
        assert_eq!(requests.len(), 1, "one audit for the one fleet-wide approval");
        let audit = &requests[0].messages[1].content;
        assert!(audit.contains("Kubernetes fleet web-prod"), "{audit}");
    }

    #[tokio::test]
    async fn no_audit_without_an_arbiter_when_disabled_or_in_never_mode() {
        for (mode, enabled) in [
            (CommandConfirmMode::Always, false),
            (CommandConfirmMode::Never, true),
        ] {
            let arbiter = ScriptedLlm::new(vec![ChatResponse::text(r#"{"verdict":"OK"}"#)]);
            let (events, _) = run_cluster_agent(mode, Some(Arc::clone(&arbiter)), enabled).await;
            assert!(
                !events.iter().any(|e| matches!(e, AgentEvent::CommandAudited { .. })),
                "{mode:?}/{enabled}: {events:?}"
            );
            assert!(arbiter.requests.lock().unwrap().is_empty());
        }
        let (events, audited) = run_cluster_agent(CommandConfirmMode::Always, None, true).await;
        assert_eq!(audited, vec![false]);
        assert!(!events.iter().any(|e| matches!(e, AgentEvent::CommandAudited { .. })));
    }
}
