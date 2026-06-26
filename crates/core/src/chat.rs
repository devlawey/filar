//! Chat block types shared between the TUI and the session persistence layer.
//!
//! [`ChatBlock`] represents a single visually-distinct entry in the chat
//! history. It was originally defined in the `filar-tui` crate but has been
//! moved here so that the session persistence layer ([`crate::session`]) can
//! serialise and deserialise it without depending on the TUI.

use serde::{Deserialize, Serialize};

/// A block in the chat history — visually distinct message type.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChatBlock {
    /// A user message.
    User(String),
    /// An agent text response.
    Agent(String),
    /// A command proposed/executed by the agent.
    Command {
        command: String,
        explanation: String,
        output: Option<String>,
        approved: bool,
    },
    /// An error message.
    Error(String),
    /// A system message (e.g. "agent started").
    System(String),
}

impl ChatBlock {
    /// Return a short preview of the block's text content (for session lists).
    pub fn preview(&self) -> String {
        match self {
            ChatBlock::User(s) => format!("You: {}", truncate(s, 60)),
            ChatBlock::Agent(s) => format!("Agent: {}", truncate(s, 60)),
            ChatBlock::Command { command, .. } => format!("$ {}", truncate(command, 60)),
            ChatBlock::Error(s) => format!("Error: {}", truncate(s, 60)),
            ChatBlock::System(s) => truncate(s, 60),
        }
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(max).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_block_serialize_roundtrip() {
        let blocks = vec![
            ChatBlock::User("hello".into()),
            ChatBlock::Agent("hi there".into()),
            ChatBlock::Command {
                command: "ls -la".into(),
                explanation: "list files".into(),
                output: Some("total 0".into()),
                approved: true,
            },
            ChatBlock::Error("something failed".into()),
            ChatBlock::System("connected".into()),
        ];

        let json = serde_json::to_string(&blocks).unwrap();
        let decoded: Vec<ChatBlock> = serde_json::from_str(&json).unwrap();

        assert_eq!(blocks.len(), decoded.len());
        assert!(matches!(&decoded[0], ChatBlock::User(s) if s == "hello"));
        assert!(matches!(&decoded[2], ChatBlock::Command { command, .. } if command == "ls -la"));
    }

    #[test]
    fn chat_block_preview() {
        assert_eq!(ChatBlock::User("hi".into()).preview(), "You: hi");
        assert_eq!(ChatBlock::Agent("ok".into()).preview(), "Agent: ok");
        let long = "x".repeat(100);
        let preview = ChatBlock::User(long).preview();
        assert!(preview.ends_with('…'));
        assert!(preview.len() < 80);
    }
}
