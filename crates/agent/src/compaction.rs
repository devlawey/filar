//! Summarising the head of a session so the conversation can continue in a
//! smaller context window (issue #377).
//!
//! The decision of *when* to compact and *where* to cut lives in
//! `filar_core::compaction`; this module only turns a transcript into a brief.
//! It is a single, self-contained LLM call with no tools: the summariser must
//! not be able to run anything on the host.

use crate::{ChatMessage, ChatRequest, LlmClient, MessageRole, TokenUsage};
use filar_core::{CoreError, Result};

/// System prompt for the summarising call.
///
/// The ordering is not cosmetic. Executed commands come first because the
/// failure that matters most is an agent re-running a destructive action it no
/// longer remembers performing; everything else degrades more gracefully.
///
/// Any change here is a change to a system prompt and requires an eval run —
/// see `AGENTS.md` and `docs/EVAL_METHODOLOGY.md`.
pub const COMPACTION_SYSTEM_PROMPT: &str = "\
You are compacting a system-administration session so it can continue within a smaller
context window. Produce a compact brief, not a narrative.

Preserve, in this order:
1. Commands already executed and their outcome — especially anything that changed state.
   The reader must never repeat a destructive action because it was omitted here.
2. Established facts about the system: versions, paths, service names, what was checked
   and ruled out.
3. The current hypothesis and what remains to be verified.
4. Constraints and preferences the user stated.

Drop: full command output, reasoning, repetition, pleasantries.
Do not speculate or add anything that is not in the transcript.
Write in the language the session is conducted in.";

/// Shortest summary treated as usable, in characters.
///
/// A summary that replaces many turns has to carry at least one executed
/// command and one established fact — the first two things
/// [`COMPACTION_SYSTEM_PROMPT`] asks for — and that cannot be done in under a
/// clause. The observed failure modes are all far below this: an empty string,
/// `OK`, `None.`, a bare refusal.
///
/// Deliberately low rather than generous. The cost of rejecting a real summary
/// is a warning and an uncompacted history, which for a genuinely tiny history
/// is harmless; the cost of accepting a non-summary is the silent loss of the
/// whole head. A one-line summary of even a trivial exchange — "User asked for
/// disk usage; agent ran df -h; root is 82% full." — clears it comfortably, so
/// the false-rejection risk stays near zero.
///
/// **This is a length check and nothing more.** A *wordy* refusal — "I cannot
/// summarize this conversation at this time." — is longer than the threshold
/// and passes. Catching that needs content matching, which the prompt above
/// makes language-dependent by asking the model to answer in the language of
/// the session, so it is a separate mechanism rather than a bigger number
/// here (raised in review of #390).
pub const MIN_SUMMARY_CHARS: usize = 40;

/// What a summarising call produced and what it cost.
///
/// The two are kept apart because they are owed to different places. The
/// summary decides whether the head is folded; the usage is owed to the
/// session's token and cost counters either way, since the request was billed
/// before anyone could judge the reply. Returning only the brief is what left
/// the reported cost short of every summary the session ever paid for (#387).
///
/// `usage` is `None` when the provider reported none, and when the call failed
/// before a response existed at all.
#[derive(Debug)]
pub struct SummaryOutcome {
    /// Token usage the summarising request itself consumed.
    pub usage: Option<TokenUsage>,
    /// The brief, or why there isn't one.
    pub summary: Result<String>,
}

/// Ask the model to summarise `transcript`.
///
/// Returns the brief that will replace the compacted head, together with what
/// the call cost. The call carries no tool definitions, so the summariser
/// cannot propose commands, and it is a plain [`LlmClient::chat`] rather than a
/// streaming call: nothing renders the partial text, and the caller only needs
/// the finished brief.
///
/// A reply shorter than [`MIN_SUMMARY_CHARS`] — an empty string included — is
/// reported as an error rather than silently accepted: replacing the head with
/// a non-summary would lose it outright (#378). Such a reply still carries its
/// usage back, because it was paid for like any other.
pub async fn summarise_history(llm: &dyn LlmClient, transcript: &str) -> SummaryOutcome {
    let request = ChatRequest {
        messages: vec![
            ChatMessage::new(MessageRole::System, COMPACTION_SYSTEM_PROMPT),
            ChatMessage::new(MessageRole::User, transcript),
        ],
        tools: Vec::new(),
    };

    let response = match llm.chat(&request).await {
        Ok(response) => response,
        // No response, so nothing was billed that we know of.
        Err(e) => return SummaryOutcome { usage: None, summary: Err(e) },
    };
    let usage = response.usage.clone();
    let text = response.text.trim().to_string();
    if text.chars().count() < MIN_SUMMARY_CHARS {
        // Treated exactly like an outright failure by the caller: the history
        // is left alone and the user is warned (#378).
        return SummaryOutcome {
            usage,
            summary: Err(CoreError::Other(format!(
                "the model returned a summary too short to be usable ({} chars)",
                text.chars().count()
            ))),
        };
    }
    SummaryOutcome { usage, summary: Ok(text) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_prompt_states_the_four_things_to_preserve_in_order() {
        // The order is load-bearing: executed commands first. A reordering here
        // is a behavioural change and should fail this test rather than pass
        // review unnoticed.
        let p = COMPACTION_SYSTEM_PROMPT;
        let commands = p.find("Commands already executed").expect("commands");
        let facts = p.find("Established facts").expect("facts");
        let hypothesis = p.find("current hypothesis").expect("hypothesis");
        let constraints = p.find("Constraints and preferences").expect("constraints");
        assert!(commands < facts && facts < hypothesis && hypothesis < constraints);
    }

    #[test]
    fn the_prompt_forbids_inventing_content() {
        assert!(COMPACTION_SYSTEM_PROMPT.contains("Do not speculate"));
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

    async fn summarise(reply: &str) -> Result<String> {
        summarise_with_usage(reply, None).await.summary
    }

    async fn summarise_with_usage(reply: &str, usage: Option<TokenUsage>) -> SummaryOutcome {
        let llm = FixedLlm(reply.to_string(), usage);
        summarise_history(&llm, "User: hi\nAgent: hello\n").await
    }

    #[tokio::test]
    async fn an_empty_or_too_short_reply_is_a_failure_not_a_summary() {
        // Folding the head into any of these would destroy it while looking
        // like a successful compaction (#378).
        for reply in ["", "   ", "OK", "None.", "I cannot summarize this."] {
            assert!(
                summarise(reply).await.is_err(),
                "must be rejected as a summary: {reply:?}"
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_summary_still_reports_what_it_cost() {
        // The request was billed before the reply could be judged unusable.
        // Dropping its usage here is how the session's cost went short (#387).
        let outcome = summarise_with_usage("OK", Some(usage(9_000, 5))).await;
        assert!(outcome.summary.is_err(), "too short to be a summary");
        let u = outcome.usage.expect("usage must survive the rejection");
        assert_eq!(u.prompt_tokens, Some(9_000));
        assert_eq!(u.completion_tokens, Some(5));
        assert_eq!(u.cost, Some(0.5));
    }

    #[tokio::test]
    async fn an_accepted_summary_reports_what_it_cost() {
        let real = "User asked for disk usage; agent ran df -h; root is 82% full.";
        let outcome = summarise_with_usage(real, Some(usage(9_000, 120))).await;
        assert_eq!(outcome.summary.unwrap(), real);
        let u = outcome.usage.expect("usage must come back with the summary");
        assert_eq!(u.prompt_tokens, Some(9_000));
        assert_eq!(u.completion_tokens, Some(120));
    }

    #[tokio::test]
    async fn a_call_that_never_returned_a_response_reports_no_usage() {
        // Nothing to attribute: inventing a zero here would be indistinguishable
        // from a provider that reports no usage.
        let outcome = summarise_history(&FailingLlm, "User: hi\n").await;
        assert!(outcome.summary.is_err());
        assert!(outcome.usage.is_none());
    }

    #[tokio::test]
    async fn a_short_but_real_summary_is_accepted() {
        // The threshold has to clear a genuine one-line summary of a trivial
        // history, or compaction would break on short sessions.
        let real = "User asked for disk usage; agent ran df -h; root is 82% full.";
        assert!(real.chars().count() >= MIN_SUMMARY_CHARS);
        assert_eq!(summarise(real).await.unwrap(), real);
    }
}
