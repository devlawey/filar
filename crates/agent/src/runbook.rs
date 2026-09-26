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

/// Appended to [`RUNBOOK_SYSTEM_PROMPT`] when the session was a fleet
/// dialogue (#443).
///
/// A fleet session asked one read-only question of a whole group at a time
/// and read back who agreed with whom, so its procedure is one for the
/// *group*: which checks to run across it, in which order, and what counts
/// as a divergence — not a sequence of steps on one machine applied by hand
/// to each. Host names are not the model's to keep: they are scrubbed from
/// the transcript before the call and from the reply after it
/// ([`FleetIdentifiers`]), and this block says why the placeholders are
/// there.
///
/// Any change here is a change to a system prompt and requires an eval run —
/// see `AGENTS.md`.
pub const FLEET_RUNBOOK_PROMPT: &str = "\
This session was run over a fleet: every command went to a whole group of hosts at once,
and the answers came back as a comparison of which hosts agreed and which differed.
Write the procedure for the group, not for one machine:
- In Steps, each step is a check run across the whole group: the command, what a host's
  answer should look like, and what counts as a divergence between hosts.
- Put the checks in the order the session ran them, and say which divergence leads to
  which next check.
- In Exit criteria, say what a healthy group looks like (all hosts agree, or which
  differences are expected) and what a problem looks like (which hosts differ, how).
- Hosts that did not answer, were skipped or did not apply are an outcome to handle,
  not a failure of the procedure.
Host names, addresses and accounts have already been replaced with <host> and <user>;
refer to hosts as <host>, to the group as \"the group\", and never name a host.";

/// Everything that identifies the hosts of a fleet session (#443): their
/// configured names, their addresses and the accounts used on them.
///
/// A fleet runbook is a procedure for the group; none of these may appear in
/// it. The prompt asks for that, but a prompt is a request — so the
/// identifiers are replaced in code, in the transcript before the call and
/// in the reply after it, and the guarantee does not depend on the model.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FleetIdentifiers {
    /// Host names and addresses, replaced with `<host>`.
    pub hosts: Vec<String>,
    /// Account names, replaced with `<user>`.
    pub users: Vec<String>,
}

impl FleetIdentifiers {
    /// Identifiers of `targets`: each one's name, address and user.
    pub fn from_targets<'a>(targets: impl IntoIterator<Item = &'a filar_core::SshTarget>) -> Self {
        let mut ids = Self::default();
        for target in targets {
            ids.hosts.push(target.name.clone());
            ids.hosts.push(target.host.clone());
            ids.users.push(target.user.clone());
        }
        ids
    }

    /// Replace every identifier in `text` with its placeholder.
    ///
    /// An identifier is replaced only where it stands on its own — not
    /// inside a longer word — so `web-1` does not eat into `web-10` and the
    /// account `admin` leaves `administrator` alone. At each position the
    /// longest identifier wins, so a host's FQDN is replaced whole before its
    /// short name.
    ///
    /// One pass over the input: a placeholder already written is never read
    /// again, and one already in the input is kept whole, so a host
    /// literally named `host` cannot turn `<host>` into `<<host>>` — neither
    /// in its own output nor in a reply that repeats the scrubbed
    /// transcript. Found in review — the first version replaced identifier
    /// by identifier over its own output.
    pub fn scrub(&self, text: &str) -> String {
        let mut pairs: Vec<(&str, &str)> = self
            .hosts
            .iter()
            .map(|h| (h.as_str(), "<host>"))
            .chain(self.users.iter().map(|u| (u.as_str(), "<user>")))
            .filter(|(id, _)| !id.trim().is_empty())
            .collect();
        // Longest first; equal identifiers end up adjacent, so the dedup is
        // complete (review).
        pairs.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(b.0)));
        pairs.dedup_by(|a, b| a.0 == b.0);

        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        while at < text.len() {
            // A placeholder in the input — the model repeating what the
            // scrubbed transcript showed it — is copied whole: scrubbing the
            // reply a second time must not turn `<host>` into `<<host>>`.
            if let Some(kept) = PLACEHOLDERS.iter().find(|p| text[at..].starts_with(**p)) {
                out.push_str(kept);
                at += kept.len();
                continue;
            }
            let before = text[..at].chars().next_back();
            let found = (!before.is_some_and(is_name_char))
                .then(|| {
                    pairs.iter().find(|(id, _)| {
                        text[at..].starts_with(id)
                            && !text[at + id.len()..].chars().next().is_some_and(is_name_char)
                    })
                })
                .flatten();
            match found {
                Some((id, placeholder)) => {
                    out.push_str(placeholder);
                    at += id.len();
                }
                None => {
                    // Advance one character, keeping to char boundaries.
                    let ch = text[at..].chars().next().unwrap_or_default();
                    out.push(ch);
                    at += ch.len_utf8().max(1);
                }
            }
        }
        out
    }
}

/// What [`FleetIdentifiers::scrub`] writes in place of an identifier.
const PLACEHOLDERS: [&str; 2] = ["<host>", "<user>"];

/// Whether `c` can be part of a host or account name — the characters that
/// make an occurrence part of a longer word rather than the name itself.
fn is_name_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '-'
}

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
    generate_with(llm, RUNBOOK_SYSTEM_PROMPT, transcript).await
}

/// [`generate_runbook`] for a fleet session (#443): a procedure for the
/// group, with every host identifier in `ids` scrubbed from the transcript
/// before the call and from the reply after it.
///
/// The same rules as the single-host runbook hold — no tools, the length
/// floor, usage reported either way.
pub async fn generate_fleet_runbook(
    llm: &dyn LlmClient,
    transcript: &str,
    ids: &FleetIdentifiers,
) -> RunbookOutcome {
    let prompt = format!("{RUNBOOK_SYSTEM_PROMPT}\n\n{FLEET_RUNBOOK_PROMPT}");
    let mut outcome = generate_with(llm, &prompt, &ids.scrub(transcript)).await;
    outcome.runbook = outcome.runbook.map(|text| ids.scrub(&text));
    outcome
}

async fn generate_with(llm: &dyn LlmClient, system_prompt: &str, transcript: &str) -> RunbookOutcome {
    let request = ChatRequest {
        messages: vec![
            ChatMessage::new(MessageRole::System, system_prompt),
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

    // ── Fleet runbook (#443) ───────────────────────────────────────

    fn fleet_ids() -> FleetIdentifiers {
        FleetIdentifiers {
            hosts: vec!["web-1".into(), "10.0.0.11".into(), "web-10".into(), "web-10.prod.example".into()],
            users: vec!["admin".into()],
        }
    }

    #[test]
    fn scrubbing_replaces_whole_names_only() {
        let text = "ssh admin@web-10.prod.example; web-1 differs from web-10 (10.0.0.11); \
                    the administrator of web-1x";
        assert_eq!(
            fleet_ids().scrub(text),
            "ssh <user>@<host>; <host> differs from <host> (<host>); the administrator of web-1x"
        );
    }

    #[test]
    fn a_placeholder_already_written_is_never_scrubbed_again() {
        let ids = FleetIdentifiers {
            hosts: vec!["host".into(), "db.host".into()],
            users: vec!["user".into()],
        };
        assert_eq!(
            ids.scrub("user@db.host and host, но не hostname и не <host>"),
            "<user>@<host> and <host>, но не hostname и не <host>"
        );
    }

    #[test]
    fn the_fleet_prompt_asks_for_a_group_procedure() {
        let p = FLEET_RUNBOOK_PROMPT;
        assert!(p.contains("for the group, not for one machine"));
        assert!(p.contains("what counts as a divergence"));
        assert!(p.contains("never name a host"));
    }

    /// Echoes the prompt and the transcript back, so a test sees exactly
    /// what the model was sent — and what it could repeat.
    struct EchoLlm;

    #[async_trait::async_trait]
    impl crate::LlmClient for EchoLlm {
        async fn chat(&self, request: &crate::ChatRequest) -> Result<crate::ChatResponse> {
            let text = request
                .messages
                .iter()
                .map(|m| m.content.clone())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(crate::ChatResponse::text(text))
        }
    }

    #[tokio::test]
    async fn a_fleet_runbook_never_names_a_host_address_or_account() {
        let transcript = "## Fleet: web\nHosts: web-1, web-10\n\
            `uname -r` → web-1 (10.0.0.11) differs; login admin@web-10.prod.example\n";
        let outcome = generate_fleet_runbook(&EchoLlm, transcript, &fleet_ids()).await;
        let text = outcome.runbook.expect("the echo is long enough");
        for leaked in ["web-1", "web-10", "10.0.0.11", "prod.example", "admin@"] {
            assert!(!text.contains(leaked), "{leaked:?} reached the runbook:\n{text}");
        }
        assert!(text.contains("for the group, not for one machine"), "the fleet block was sent");
    }

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl crate::LlmClient for Named {
        async fn chat(&self, _request: &crate::ChatRequest) -> Result<crate::ChatResponse> {
            Ok(crate::ChatResponse::text(self.0.to_string()))
        }
    }

    #[tokio::test]
    async fn a_host_the_model_names_anyway_is_scrubbed_from_the_reply() {
        let reply = "## Steps\n1. Run `df -h` across the group; web-10 at 10.0.0.11 is usually the odd one. \
                     Log in as admin if needed.";
        let outcome = generate_fleet_runbook(&Named(reply), "t", &fleet_ids()).await;
        let text = outcome.runbook.expect("long enough");
        assert!(!text.contains("web-10") && !text.contains("10.0.0.11") && !text.contains("admin "), "{text}");
    }

    #[tokio::test]
    async fn a_fleet_runbook_keeps_the_single_host_rules() {
        let outcome = generate_fleet_runbook(&FixedLlm("OK".into(), Some(usage(10, 1))), "t", &fleet_ids()).await;
        assert!(outcome.runbook.is_err(), "the length floor still applies");
        assert!(outcome.usage.is_some(), "usage is reported either way");
        let outcome = generate_fleet_runbook(&FailingLlm, "t", &fleet_ids()).await;
        assert!(outcome.runbook.is_err() && outcome.usage.is_none());
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
