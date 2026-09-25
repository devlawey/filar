//! The confirmation gate over a fleet (#435).
//!
//! Решения по развилкам 14, 20, 10. There is no writing in a fleet (#419),
//! so what the user approves is not "this command on twelve machines" but
//! "read this on these twelve" — **one** approval covers the whole radius.
//!
//! # Scope follows what the command does, not how many hosts there are
//!
//! [`approval_scope`] is the model: a command the transport's read-only
//! allowlist accepts ([`filar_transport::check_read_only`]) needs one
//! approval ([`ApprovalScope::Once`]); anything else would need an approval
//! **per host** ([`ApprovalScope::PerHost`]). That second path is written
//! into the model now so a future fleet write cannot arrive as "one click
//! for twelve hosts" — but today it is closed: [`fleet_gate`] refuses such
//! a command before the user is asked, whatever the answer would have
//! been.
//!
//! # Two layers, on purpose
//!
//! The gate decides whether the user is asked. Each host's
//! `ReadOnlyExecutor` decides, independently, that a write never reaches
//! it. The gate reuses the same allowlist, so the two cannot disagree on
//! what a write is, and the refusal comes before a dialog that could never
//! lead anywhere.

use filar_transport::readonly::check_read_only;

/// How many approvals a fleet command needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalScope {
    /// Read-only: one approval covers every host of the radius.
    Once,
    /// Changes state: every host must be approved on its own. No such
    /// command is currently allowed to run in a fleet.
    PerHost,
}

impl ApprovalScope {
    /// Approvals required to run over `hosts` hosts.
    pub fn approvals_needed(self, hosts: usize) -> usize {
        match self {
            ApprovalScope::Once => hosts.min(1),
            ApprovalScope::PerHost => hosts,
        }
    }

    /// Whether `approvals` affirmative answers cover `hosts` hosts.
    ///
    /// For [`PerHost`](Self::PerHost), fewer than one per host is not
    /// enough — one approval can never stand for the others.
    pub fn is_covered(self, hosts: usize, approvals: usize) -> bool {
        approvals >= self.approvals_needed(hosts)
    }
}

/// The scope a command falls under in a fleet.
pub fn approval_scope(command: &str) -> ApprovalScope {
    if check_read_only(command).is_ok() {
        ApprovalScope::Once
    } else {
        ApprovalScope::PerHost
    }
}

/// The reason shown to the user when the gate refuses a write: they were
/// never asked, so the feed says who said no and why.
pub const FLEET_WRITE_DENIED: &str = "the fleet is read-only, nothing was sent to any host";

/// What the gate does with a command proposed in a fleet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetGate {
    /// Ask the user, with this scope.
    Ask(ApprovalScope),
    /// Refuse without asking; the text is the tool result for the model.
    Refuse(String),
}

/// Decide what the gate does with `command`.
///
/// Read-only → [`FleetGate::Ask`] with [`ApprovalScope::Once`]. Everything
/// else → [`FleetGate::Refuse`]: the per-host approval path exists in the
/// model and is closed, so no approval, however given, lets it through.
pub fn fleet_gate(command: &str) -> FleetGate {
    match check_read_only(command) {
        Ok(()) => FleetGate::Ask(ApprovalScope::Once),
        Err(reason) => FleetGate::Refuse(format!(
            "Error: the fleet is read-only, so this command runs on no host: {reason}. \
             Changing state across a fleet would need approval on every host and is \
             not available; use read-only commands, or ask the user to open one host \
             in its own tab."
        )),
    }
}

/// One line naming the radius, for the Explain (F2) explanation and the
/// transcript: `on 3 hosts: web-1, web-2, db-1`.
pub fn radius_line(hosts: &[String]) -> String {
    let noun = if hosts.len() == 1 { "host" } else { "hosts" };
    format!("on {} {noun}: {}", hosts.len(), hosts.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_read_only_command_needs_one_approval_for_the_whole_radius() {
        assert_eq!(approval_scope("uname -r"), ApprovalScope::Once);
        assert_eq!(fleet_gate("uname -r"), FleetGate::Ask(ApprovalScope::Once));
        assert_eq!(ApprovalScope::Once.approvals_needed(12), 1);
        assert!(ApprovalScope::Once.is_covered(12, 1));
        assert!(!ApprovalScope::Once.is_covered(12, 0));
        assert_eq!(ApprovalScope::Once.approvals_needed(0), 0);
    }

    #[test]
    fn a_write_needs_an_approval_per_host_and_that_path_is_closed() {
        for cmd in ["rm -rf /tmp/x", "systemctl restart nginx", "uname -r; reboot", "echo x > /etc/motd"] {
            assert_eq!(approval_scope(cmd), ApprovalScope::PerHost, "{cmd}");
            match fleet_gate(cmd) {
                FleetGate::Refuse(text) => {
                    assert!(text.starts_with("Error: the fleet is read-only"), "{text}");
                }
                other => panic!("{cmd}: expected a refusal, got {other:?}"),
            }
        }
        // One approval never stands for twelve hosts.
        assert_eq!(ApprovalScope::PerHost.approvals_needed(12), 12);
        assert!(!ApprovalScope::PerHost.is_covered(12, 1));
        assert!(!ApprovalScope::PerHost.is_covered(12, 11));
        assert!(ApprovalScope::PerHost.is_covered(12, 12));
    }

    #[test]
    fn radius_line_names_every_host() {
        let hosts = vec!["web-1".to_string(), "web-2".to_string()];
        assert_eq!(radius_line(&hosts), "on 2 hosts: web-1, web-2");
        assert_eq!(radius_line(&hosts[..1]), "on 1 host: web-1");
    }
}
