//! Kubernetes agent loop for cluster tabs (#575).
//!
//! The loop executes only typed, read-only [`KubeToolCall`] values. It never
//! adapts Kubernetes operations to shell commands or `CommandExecutor`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use filar_core::{CommandConfirmMode, CoreError, KubeCluster, Result, TagPolicy};
use tokio_util::sync::CancellationToken;

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
                messages.push(self.process_tool_call(call).await?);
            }
        }

        Err(CoreError::Other(format!(
            "Kubernetes agent loop exceeded maximum iterations ({MAX_ITERATIONS})"
        )))
    }

    async fn process_tool_call(&self, call: &ToolCall) -> Result<ChatMessage> {
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
                            self.confirmer.as_ref(),
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
                            self.confirmer.as_ref(),
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
                    self.confirmer.as_ref(),
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
                        self.confirmer
                            .confirm(&display, &confirmation_explanation, false),
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
}
