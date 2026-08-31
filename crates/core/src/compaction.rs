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
