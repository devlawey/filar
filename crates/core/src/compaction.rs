//! Context compaction: deciding when the model's context is full enough to
//! warrant compacting the history, and where that history may safely be cut.
//!
//! This module is deliberately free of I/O and of any TUI types: both decisions
//! are pure functions of the conversation and a threshold, so they can be
//! tested directly.
//!
//! Only the measurement and the boundary live here. Producing the summary that
//! replaces the compacted head is a separate step (see issue #377).

use crate::ChatBlock;

/// How many complete turns are kept verbatim at the end of the history.
///
/// Recent context matters most and does not survive being paraphrased, so the
/// tail is never summarised. Four to six turns is the useful range; five is the
/// default.
pub const DEFAULT_KEEP_TURNS: usize = 5;

/// Whether the history should be compacted before the next request.
///
/// `last_prompt_tokens` must be the `prompt_tokens` figure the provider
/// reported for the **most recent** request — that is the measured size of the
/// context that was actually sent. A cumulative per-session counter is the
/// wrong input: it sums every request in the session and would cross any
/// threshold long before the context itself does.
///
/// `None` means the provider reported no usage, so the context size is unknown
/// and the threshold cannot fire; the reactive path (issue #378) covers that
/// case. A `compact_at_tokens` of `0` disables compaction entirely.
pub fn should_compact(last_prompt_tokens: Option<u64>, compact_at_tokens: u64) -> bool {
    if compact_at_tokens == 0 {
        return false;
    }
    matches!(last_prompt_tokens, Some(used) if used >= compact_at_tokens)
}

/// Index in `blocks` where the verbatim tail begins.
///
/// Everything before the returned index is the head, which may be replaced by a
/// summary; everything from it onward is kept word for word. `0` means there is
/// nothing to compact — the whole history is already within the tail.
///
/// The boundary always lands on a [`ChatBlock::User`], because a turn is a user
/// message plus every agent and command block that answers it. Cutting inside a
/// turn would leave commands in the tail with no request they belong to, and
/// their `Command:`/`Output:` text would read as if the agent had run them
/// unprompted.
///
/// Note for future changes: the history the TUI hands to the model is built by
/// flattening [`ChatBlock`]s into plain assistant messages, so there are no
/// `tool` role messages and no `tool_call_id` values in it. If that
/// representation ever changes to real tool messages, this boundary must also
/// never fall between a tool call and its result — an orphaned `tool_call_id`
/// is rejected by the provider.
pub fn compaction_boundary(blocks: &[ChatBlock], keep_turns: usize) -> usize {
    // `keep_turns == 0` means nothing is kept verbatim: the whole history is
    // the head.
    if keep_turns == 0 {
        return blocks.len();
    }

    let turn_starts: Vec<usize> = blocks
        .iter()
        .enumerate()
        .filter(|(_, b)| matches!(b, ChatBlock::User(_)))
        .map(|(i, _)| i)
        .collect();

    if turn_starts.len() <= keep_turns {
        return 0;
    }
    turn_starts[turn_starts.len() - keep_turns]
}

/// Replace the head of `blocks` with a single summary block.
///
/// Everything from `boundary` onward is kept byte for byte: the tail is what
/// the model still needs verbatim, and paraphrasing it is exactly the loss
/// compaction is meant to avoid. `boundary` is expected to come from
/// [`compaction_boundary`].
///
/// A `boundary` of `0` (nothing to compact) returns the history unchanged, so
/// the caller does not have to special-case a short conversation. Any earlier
/// summary in the head is folded into the new one along with everything else —
/// summaries compound rather than accumulate.
pub fn compact_history(blocks: &[ChatBlock], boundary: usize, summary: &str) -> Vec<ChatBlock> {
    if boundary == 0 || boundary > blocks.len() {
        return blocks.to_vec();
    }

    let mut out = Vec::with_capacity(blocks.len() - boundary + 1);
    out.push(ChatBlock::Summary {
        text: summary.to_string(),
        replaced_blocks: boundary,
    });
    out.extend_from_slice(&blocks[boundary..]);
    out
}

/// The transcript handed to the summarising model, as plain text.
///
/// Command output is included: what a command actually printed is the evidence
/// the summary has to preserve. The caller decides how much of the history to
/// pass — normally the head being replaced.
pub fn transcript_for_summary(blocks: &[ChatBlock]) -> String {
    let mut out = String::new();
    for block in blocks {
        match block {
            ChatBlock::User(text) => out.push_str(&format!("User: {text}\n")),
            ChatBlock::Agent(text) => out.push_str(&format!("Agent: {text}\n")),
            ChatBlock::Command {
                command,
                output,
                approved,
                ..
            } => {
                let outcome = match (approved, output) {
                    (false, _) => "(denied by user)".to_string(),
                    (true, None) => "(no output)".to_string(),
                    (true, Some(o)) => o.clone(),
                };
                out.push_str(&format!("Command: {command}\nOutput: {outcome}\n"));
            }
            ChatBlock::Error(text) => out.push_str(&format!("Error: {text}\n")),
            // Feed-only chrome: never part of what the model was told.
            ChatBlock::System(_) => {}
            ChatBlock::Summary { text, .. } => {
                out.push_str(&format!("Summary of earlier turns: {text}\n"))
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(s: &str) -> ChatBlock {
        ChatBlock::User(s.into())
    }
    fn agent(s: &str) -> ChatBlock {
        ChatBlock::Agent(s.into())
    }
    fn command(s: &str) -> ChatBlock {
        ChatBlock::Command {
            command: s.into(),
            explanation: String::new(),
            output: Some("ok".into()),
            approved: true,
        }
    }
    fn system(s: &str) -> ChatBlock {
        ChatBlock::System(s.into())
    }

    #[test]
    fn compaction_replaces_the_head_and_keeps_the_tail_byte_for_byte() {
        let blocks = vec![
            user("first"),
            agent("a1"),
            command("systemctl restart nginx"),
            user("second"),
            agent("a2"),
            user("third"),
            agent("a3"),
        ];
        let boundary = compaction_boundary(&blocks, 2);
        assert_eq!(boundary, 3, "tail starts at the third user turn");

        let compacted = compact_history(&blocks, boundary, "restarted nginx, still 502");

        // One summary in place of the whole head…
        assert_eq!(compacted.len(), 1 + (blocks.len() - boundary));
        match &compacted[0] {
            ChatBlock::Summary {
                text,
                replaced_blocks,
            } => {
                assert_eq!(text, "restarted nginx, still 502");
                assert_eq!(*replaced_blocks, 3);
            }
            other => panic!("expected a summary, got {other:?}"),
        }

        // …and the tail unchanged, block for block.
        let original_tail = serde_json::to_string(&blocks[boundary..]).unwrap();
        let kept_tail = serde_json::to_string(&compacted[1..]).unwrap();
        assert_eq!(kept_tail, original_tail, "the tail must not be rewritten");
    }

    #[test]
    fn compaction_with_nothing_to_compact_is_a_no_op() {
        let blocks = vec![user("only"), agent("turn")];
        let before = serde_json::to_string(&blocks).unwrap();
        let after = serde_json::to_string(&compact_history(&blocks, 0, "unused")).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn compacting_twice_folds_the_earlier_summary_in_rather_than_stacking() {
        let blocks = vec![
            ChatBlock::Summary {
                text: "first summary".into(),
                replaced_blocks: 4,
            },
            user("second"),
            agent("a2"),
            user("third"),
            agent("a3"),
        ];
        let compacted = compact_history(&blocks, 3, "second summary");
        assert_eq!(
            compacted
                .iter()
                .filter(|b| matches!(b, ChatBlock::Summary { .. }))
                .count(),
            1,
            "summaries must not accumulate"
        );
    }

    #[test]
    fn the_transcript_carries_commands_and_their_output_but_not_feed_chrome() {
        let blocks = vec![
            user("why is it 502"),
            command("systemctl restart nginx"),
            system("context is filling up"),
        ];
        let text = transcript_for_summary(&blocks);
        assert!(text.contains("why is it 502"));
        assert!(text.contains("Command: systemctl restart nginx"));
        assert!(text.contains("Output: ok"), "outcome is the evidence");
        assert!(
            !text.contains("context is filling up"),
            "feed-only system lines were never sent to the model"
        );
    }

    #[test]
    fn a_denied_command_reads_as_denied_not_as_executed() {
        let blocks = vec![ChatBlock::Command {
            command: "rm -rf /var/log".into(),
            explanation: String::new(),
            output: None,
            approved: false,
        }];
        let text = transcript_for_summary(&blocks);
        assert!(text.contains("(denied by user)"));
    }

    #[test]
    fn threshold_fires_on_the_last_request_not_on_a_running_total() {
        // The session has burned 300k tokens in total, but the context that was
        // actually sent last time was 20k. Compaction must not fire.
        let cumulative_would_be = 300_000;
        assert!(
            should_compact(Some(cumulative_would_be), 200_000),
            "sanity: the cumulative figure would cross the threshold"
        );
        assert!(
            !should_compact(Some(20_000), 200_000),
            "the real context size is well below the threshold"
        );
    }

    #[test]
    fn threshold_fires_at_and_above_the_limit() {
        assert!(!should_compact(Some(199_999), 200_000));
        assert!(should_compact(Some(200_000), 200_000));
        assert!(should_compact(Some(200_001), 200_000));
    }

    #[test]
    fn zero_threshold_disables_compaction() {
        assert!(!should_compact(Some(u64::MAX), 0));
    }

    #[test]
    fn unknown_context_size_never_fires() {
        assert!(!should_compact(None, 1));
    }

    #[test]
    fn boundary_lands_on_a_user_block() {
        let blocks = vec![
            user("q1"),
            agent("a1"),
            user("q2"),
            command("ls"),
            agent("a2"),
            user("q3"),
            agent("a3"),
        ];
        let cut = compaction_boundary(&blocks, 2);
        assert_eq!(cut, 2, "the tail must start at the second-to-last turn");
        assert!(matches!(blocks[cut], ChatBlock::User(_)));
    }

    #[test]
    fn boundary_never_separates_a_command_from_its_user_message() {
        // The tail length lands mid-turn: turn 2 is user + two commands. A
        // naive "keep the last N blocks" cut would start at a Command.
        let blocks = vec![
            user("q1"),
            agent("a1"),
            user("q2"),
            command("systemctl restart nginx"),
            command("systemctl status nginx"),
            agent("a2"),
        ];
        let cut = compaction_boundary(&blocks, 1);
        assert_eq!(cut, 2);
        assert!(
            matches!(blocks[cut], ChatBlock::User(_)),
            "the tail must open with the request the commands belong to"
        );
        assert!(
            !blocks[cut..]
                .iter()
                .any(|b| matches!(b, ChatBlock::Command { .. }))
                || matches!(blocks[cut], ChatBlock::User(_)),
            "no command may appear in the tail before its user message"
        );
    }

    #[test]
    fn nothing_to_compact_when_history_fits_in_the_tail() {
        let blocks = vec![user("q1"), agent("a1"), user("q2"), agent("a2")];
        assert_eq!(compaction_boundary(&blocks, 5), 0);
        assert_eq!(
            compaction_boundary(&blocks, 2),
            0,
            "exactly `keep_turns` turns is still nothing to compact"
        );
    }

    #[test]
    fn system_blocks_do_not_start_a_turn() {
        // System notices are dropped when the history is converted for the
        // model, so they must not be mistaken for a turn boundary.
        let blocks = vec![
            system("session started"),
            user("q1"),
            agent("a1"),
            system("switched profile"),
            user("q2"),
            agent("a2"),
        ];
        let cut = compaction_boundary(&blocks, 1);
        assert_eq!(cut, 4, "the tail starts at the last user block, not the notice");
    }

    #[test]
    fn history_without_user_blocks_is_never_cut() {
        let blocks = vec![system("session started"), agent("greeting")];
        assert_eq!(compaction_boundary(&blocks, 1), 0);
    }

    #[test]
    fn keeping_no_turns_makes_the_whole_history_the_head() {
        let blocks = vec![user("q1"), agent("a1")];
        assert_eq!(compaction_boundary(&blocks, 0), blocks.len());
    }
}
