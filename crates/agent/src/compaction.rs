//! Summarising the head of a session so the conversation can continue in a
//! smaller context window (issue #377).
//!
//! The decision of *when* to compact and *where* to cut lives in
//! `filar_core::compaction`; this module only turns a transcript into a brief.
//! It is a single, self-contained LLM call with no tools: the summariser must
//! not be able to run anything on the host.

use crate::{ChatMessage, ChatRequest, LlmClient, MessageRole};
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

/// Ask the model to summarise `transcript`.
///
/// Returns the brief that will replace the compacted head. The call carries no
/// tool definitions, so the summariser cannot propose commands, and it is a
/// plain [`LlmClient::chat`] rather than a streaming call: nothing renders the
/// partial text, and the caller only needs the finished brief.
///
/// An empty reply is reported as an error rather than silently accepted —
/// replacing the head with an empty summary would lose it outright.
pub async fn summarise_history(llm: &dyn LlmClient, transcript: &str) -> Result<String> {
    let request = ChatRequest {
        messages: vec![
            ChatMessage::new(MessageRole::System, COMPACTION_SYSTEM_PROMPT),
            ChatMessage::new(MessageRole::User, transcript),
        ],
        tools: Vec::new(),
    };

    let response = llm.chat(&request).await?;
    let text = response.text.trim().to_string();
    if text.is_empty() {
        return Err(CoreError::Other(
            "the model returned an empty summary".into(),
        ));
    }
    Ok(text)
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
}
