//! Turning a finished session into a reusable runbook (issue #401).
//!
//! The decision of *when* to ask for a runbook — an explicit `Ctrl+S` in the
//! TUI — lives in the caller; this module only turns a transcript into a
//! procedure. Like [`crate::compaction`], it is a single, self-contained LLM
//! call with no tools: the model writing a document must not be able to run
//! anything on the host either.

use crate::{ChatMessage, ChatRequest, LlmClient, MessageRole, TokenUsage};
use filar_core::{CoreError, Result};

/// System prompt for the runbook call.
///
/// Two demands carry the feature. The runbook is a *procedure* — the user
/// already has the linear transcript next to it, and a second retelling adds
/// nothing. And it is *generalised* — it is meant to be re-run on a similar
/// system and handed to colleagues, so concrete hosts, accounts and secrets
/// must not survive in it. Anything that is not in the transcript must not
/// appear at all.
///
/// Any change here is a change to a system prompt and requires an eval run —
/// see `AGENTS.md`.
pub const RUNBOOK_SYSTEM_PROMPT: &str = "\
You are writing a runbook from a system-administration session: the session folded into
a reusable procedure that a colleague can follow on a similar system with the same symptom.

Write a procedure, not a retelling. Structure it:
1. Symptom / when to use — the signs that call for this procedure.
2. Preconditions — what system it applies to, what access is needed.
3. Steps in order: the command, what to look at in the output, how to read it.
4. Exit criteria — what means \"healthy\" and what means \"a problem was found\".
5. Next actions for each outcome.

Generalise everything. Replace concrete hostnames, IP addresses, usernames, paths and
service names that identify the specific machine with placeholders like <host>, <user>,
<service>. The procedure must be runnable on a similar system without editing beyond
those placeholders.
Never include passwords, tokens or keys in any form — write <secret> where a value would
be needed, and refer to credentials as something the operator obtains separately.
Do not speculate or add steps that are not in the transcript.
Write in the language the session is conducted in.";

/// Shortest runbook treated as usable, in characters.
///
/// A runbook covers symptom, preconditions, at least one step and an exit
/// criterion; none of the observed failure modes — an empty string, `OK`, a
/// bare refusal — comes close, and a genuine minimal procedure clears it many
/// times over. Deliberately a floor, not a quality bar: like the summary
/// threshold, this is a length check and nothing more.
pub const MIN_RUNBOOK_CHARS: usize = 80;

/// What a runbook call produced and what it cost.
///
/// The two are kept apart for the same reason as [`crate::SummaryOutcome`]:
/// the request was billed before anyone could judge the reply, so the usage
/// is owed to the session's counters whether or not a usable runbook came
/// back.
#[derive(Debug)]
pub struct RunbookOutcome {
    /// Token usage the runbook request itself consumed.
    pub usage: Option<TokenUsage>,
    /// The runbook text, or why there isn't one.
    pub runbook: Result<String>,
}

/// Ask the model to turn `transcript` into a runbook.
///
/// A streaming call even though nothing renders the partial text: the
/// non-streaming path is bounded by the client's *total* timeout, and a
/// runbook over a long transcript routinely outlives it — observed on a
/// 77 KB session at the default `[timeouts].llm_secs` (#405). The streaming
/// path bounds only the silence between chunks, so a long generation
/// survives while it keeps sending. The call carries no tool definitions, so
/// the runbook writer cannot propose commands.
///
/// A reply shorter than [`MIN_RUNBOOK_CHARS`] — an empty string included — is
/// reported as an error rather than written out; its usage still comes back,
/// because it was paid for like any other.
pub async fn generate_runbook(llm: &dyn LlmClient, transcript: &str) -> RunbookOutcome {
    let request = ChatRequest {
        messages: vec![
            ChatMessage::new(MessageRole::System, RUNBOOK_SYSTEM_PROMPT),
            ChatMessage::new(MessageRole::User, transcript),
        ],
        tools: Vec::new(),
    };

    let response = match llm.chat_stream(&request, &|_| {}).await {
        Ok(response) => response,
        // No response, so nothing was billed that we know of.
        Err(e) => return RunbookOutcome { usage: None, runbook: Err(e) },
    };
    let usage = response.usage.clone();
    let text = response.text.trim().to_string();
    if text.chars().count() < MIN_RUNBOOK_CHARS {
        return RunbookOutcome {
            usage,
            runbook: Err(CoreError::Other(format!(
                "the model returned a runbook too short to be usable ({} chars)",
                text.chars().count()
            ))),
        };
    }
    RunbookOutcome { usage, runbook: Ok(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_states_the_sections_in_order() {
        // A procedure has the symptom first and the next actions last; a
        // reordering here is a behavioural change and should fail this test
        // rather than pass review unnoticed.
        let p = RUNBOOK_SYSTEM_PROMPT;
        let symptom = p.find("Symptom / when to use").expect("symptom");
        let preconditions = p.find("Preconditions").expect("preconditions");
        let steps = p.find("Steps in order").expect("steps");
        let exit = p.find("Exit criteria").expect("exit criteria");
        let next = p.find("Next actions").expect("next actions");
        assert!(symptom < preconditions && preconditions < steps && steps < exit && exit < next);
    }

    #[test]
    fn the_prompt_demands_generalisation_and_forbids_secrets() {
        let p = RUNBOOK_SYSTEM_PROMPT;
        assert!(p.contains("Generalise"), "generalisation must be explicit");
        assert!(p.contains("<host>"), "placeholder example for hosts");
        assert!(
            p.contains("Never include passwords, tokens or keys"),
            "the secret ban must be explicit"
        );
    }

    #[test]
    fn the_prompt_forbids_inventing_content() {
        assert!(RUNBOOK_SYSTEM_PROMPT.contains("Do not speculate"));
    }

    struct FixedLlm(String, Option<TokenUsage>);

    #[async_trait::async_trait]
    impl crate::LlmClient for FixedLlm {
        async fn chat(&self, _request: &crate::ChatRequest) -> Result<crate::ChatResponse> {
            let mut response = crate::ChatResponse::text(self.0.clone());
            response.usage = self.1.clone();
            Ok(response)
        }
    }

    struct FailingLlm;

    #[async_trait::async_trait]
    impl crate::LlmClient for FailingLlm {
        async fn chat(&self, _request: &crate::ChatRequest) -> Result<crate::ChatResponse> {
            Err(CoreError::Other("provider is down".into()))
        }
    }

    fn usage(prompt: u64, completion: u64) -> TokenUsage {
        TokenUsage {
            prompt_tokens: Some(prompt),
            completion_tokens: Some(completion),
            total_tokens: Some(prompt + completion),
            cost: Some(0.5),
        }
    }

    const REAL_RUNBOOK: &str = "## Symptom\nReplica lag grows without a matching write burst.\n\n\
        ## Steps\n1. Run `SHOW REPLICA STATUS` and check Seconds_Behind_Source.";

    #[tokio::test]
    async fn an_empty_or_too_short_reply_is_a_failure_not_a_runbook() {
        for reply in ["", "   ", "OK", "None.", "I cannot write this runbook."] {
            let llm = FixedLlm(reply.to_string(), None);
            let outcome = generate_runbook(&llm, "User: hi\nAgent: hello\n").await;
            assert!(
                outcome.runbook.is_err(),
                "must be rejected as a runbook: {reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_runbook_still_reports_what_it_cost() {
        let llm = FixedLlm("OK".to_string(), Some(usage(9_000, 5)));
        let outcome = generate_runbook(&llm, "User: hi\n").await;
        assert!(outcome.runbook.is_err(), "too short to be a runbook");
        let u = outcome.usage.expect("usage must survive the rejection");
        assert_eq!(u.prompt_tokens, Some(9_000));
        assert_eq!(u.completion_tokens, Some(5));
        assert_eq!(u.cost, Some(0.5));
    }

    #[tokio::test]
    async fn an_accepted_runbook_reports_what_it_cost() {
        let llm = FixedLlm(REAL_RUNBOOK.to_string(), Some(usage(9_000, 400)));
        let outcome = generate_runbook(&llm, "User: replica lag\n").await;
        assert_eq!(outcome.runbook.unwrap(), REAL_RUNBOOK);
        let u = outcome.usage.expect("usage must come back with the runbook");
        assert_eq!(u.prompt_tokens, Some(9_000));
        assert_eq!(u.completion_tokens, Some(400));
    }

    #[tokio::test]
    async fn a_call_that_never_returned_a_response_reports_no_usage() {
        let outcome = generate_runbook(&FailingLlm, "User: hi\n").await;
        assert!(outcome.runbook.is_err());
        assert!(outcome.usage.is_none());
    }

    /// Fails every non-streaming call: a runbook must go through the
    /// streaming path, where the timeout bounds chunk silence rather than the
    /// total generation (#405).
    struct StreamOnlyLlm(String);

    #[async_trait::async_trait]
    impl crate::LlmClient for StreamOnlyLlm {
        async fn chat(&self, _request: &crate::ChatRequest) -> Result<crate::ChatResponse> {
            Err(CoreError::Other(
                "runbook generation must use the streaming path".into(),
            ))
        }

        async fn chat_stream(
            &self,
            _request: &crate::ChatRequest,
            _on_delta: &(dyn Fn(String) + Send + Sync),
        ) -> Result<crate::ChatResponse> {
            Ok(crate::ChatResponse::text(self.0.clone()))
        }
    }

    #[tokio::test]
    async fn a_runbook_is_generated_through_the_streaming_path() {
        // Regression test for #405: with a non-streaming call, the total
        // request timeout capped the whole generation and a runbook over a
        // long transcript died at `[timeouts].llm_secs`.
        let llm = StreamOnlyLlm(REAL_RUNBOOK.to_string());
        let outcome = generate_runbook(&llm, "User: replica lag\n").await;
        assert_eq!(
            outcome
                .runbook
                .expect("the streaming call must produce the runbook"),
            REAL_RUNBOOK
        );
    }
}
