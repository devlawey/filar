//! Compile check for the snippets in `docs/ENGINE_API.md` (#393).
//!
//! Markdown in `docs/` is not compiled by anything, so documentation that talks
//! about a signature can drift out of date without a single test going red —
//! which is how `ENGINE_API.md` came to describe none of the three contract
//! changes in 1.0.6. Keeping the snippets here as an example means `cargo build
//! --workspace` fails when the API they describe moves.
//!
//! Keep this in step with the document. It is not a usage sample.

use filar_agent::{ChatMessage, LlmClient};
use filar_core::{ChatBlock, Session};

/// "`ChatBlock::Summary` — this one goes to the model".
pub fn flatten(block: &ChatBlock) -> Option<ChatMessage> {
    match block {
        ChatBlock::Summary { text, .. } => Some(ChatMessage::user(format!(
            "Summary of earlier turns in this session:\n{text}"
        ))),
        _ => None,
    }
}

/// "`Session::folded_history` — where the folded turns live".
pub fn whole_conversation(session: &Session) -> Vec<ChatBlock> {
    session
        .folded_history
        .iter()
        .chain(session.messages.iter())
        .cloned()
        .collect()
}

/// "`summarise_history` — asking the model for the summary".
pub async fn summarise(llm: &dyn LlmClient, blocks: &[ChatBlock], boundary: usize) {
    let transcript = filar_core::transcript_for_summary(&blocks[..boundary]);
    let outcome = filar_agent::summarise_history(llm, &transcript).await;
    if let Ok(summary) = outcome.summary {
        let _compacted = filar_core::compact_history(blocks, boundary, &summary);
    }
    let _usage = outcome.usage;
}

fn main() {}
