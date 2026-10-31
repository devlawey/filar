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
        }
    }

    /// Send full per-member answers only to the frontend's fleet panel.
    pub fn fleet_observer(mut self, observer: KubeFleetObserver) -> Self {
        if let KubeExecution::Fleet { observer: slot, .. } = &mut self.execution {
            *slot = Some(observer);
        }
        self
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
        let mut messages = vec![ChatMessage::system(prompt)];
        messages.extend_from_slice(history);
        messages.push(ChatMessage::user(user_prompt));
        let tools = kube_tool_definitions(self.confirm_mode);
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
