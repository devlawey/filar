//! The fleet-operation layer: one operation over many hosts (#426).
//!
//! Until now every bit of working state lived in a session — one dialogue,
//! one executor, one host. A fleet question ("what kernel is each of these
//! twelve boxes running?") had nowhere to live: there was no entity *above*
//! a session to hold it.
//!
//! [`FleetOperation`] is that entity. It is the model half of the layer —
//! who takes part and where each of them stands — and it runs nothing:
//! fanning the work out across the members is the runner's job (#427), and
//! the states a finished host can be in arrive with the outcome set (#428).
//!
//! # One dialogue, N executors
//!
//! A fleet is **one** dialogue session — one context, one transcript, one
//! cost counter — with N executors under it. It is not N dialogues. So an
//! operation does not own a conversation of its own; it owns the host set a
//! single question is asked of, and the per-host progress of asking it.
//!
//! # The composition is frozen at open time
//!
//! [`FleetOperation::open`] resolves the group's tag rule **once** and keeps
//! a snapshot of the hosts it selected. Re-tagging a host afterwards, adding
//! a target that would have matched, or editing the group cannot change who
//! is in an operation that is already open.
//!
//! That is a convenience for the person reading the panel — the host list
//! under their eyes stays the one they approved — and a security property
//! besides: **output from a host can never change the set of hosts.** A
//! compromised member cannot talk its way into widening the blast radius,
//! because the only input to the composition is taken before the first
//! command goes out, and nothing recomputes it later.
//!
//! The group's limits and policy are frozen in the same snapshot, for the
//! same reason: the operation the runner executes is the one that was
//! opened, not whatever the config file says by the time it finishes.
//!
//! # A host in the fleet is still an ordinary host
//!
//! An operation claims nothing exclusively. The same host may sit in a
//! normal tab, in another operation, or in both, at the same time — the
//! operation holds its own snapshot and its own progress cell, so there is
//! no lock to contend for and nothing to reserve. Opening a fleet over a
//! host someone is already working on interactively is allowed on purpose:
//! fleet work is read-only (#419), and forbidding it would make the mode
//! refuse exactly when it is most useful.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use crate::config::{select_hosts_for_group, HostGroup, HostGroupPolicy, SshTarget};

/// Source of [`OperationId`] values, one per process.
static NEXT_OPERATION_ID: AtomicU64 = AtomicU64::new(1);

// ---------------------------------------------------------------------------
// OperationId
// ---------------------------------------------------------------------------

/// Identifier of a fleet operation, unique within one run of the process.
///
/// Ids are handed out in ascending order, so comparing two of them tells
/// which operation was opened first. They are not stable across restarts and
/// are not meant for storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct OperationId(u64);

impl OperationId {
    /// The next unused id.
    fn next() -> Self {
        Self(NEXT_OPERATION_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// The id as a plain number, for rendering and logging.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl std::fmt::Display for OperationId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// How far one member of an operation has got.
///
/// This is the *progress* of asking a host the question, not the answer:
/// whether the host agreed with the others, timed out or never picked up is
/// the outcome set, which lands with #428 and attaches to
/// [`Done`][HostProgress::Done].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum HostProgress {
    /// Selected, nothing sent yet. Every member starts here, including one
    /// the runner will never reach because the parallelism limit or a
    /// cancellation stops the operation first.
    #[default]
    Pending,
    /// A command is on its way to this host, or its answer is on its way
    /// back.
    Running,
    /// This host has stopped running and will not be asked again in this
    /// operation.
    Done,
}

// ---------------------------------------------------------------------------
// Members
// ---------------------------------------------------------------------------

/// Handle to one member of an operation.
///
/// Positional rather than by name, because target names are not guaranteed
/// unique — see [`FleetOperation::handle_for`] — and the composition never
/// changes, so a handle stays valid for the whole life of its operation.
///
/// It carries the id of the operation that handed it out, and the operation
/// checks it before touching a member. Found in review: with the index
/// alone, a handle to the second member of one operation addressed the
/// second member of *any* operation, so a runner holding handles from two
/// operations at once could have written progress into the wrong one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostHandle {
    operation: OperationId,
    index: usize,
}

impl HostHandle {
    /// The operation this handle belongs to.
    pub fn operation(&self) -> OperationId {
        self.operation
    }
}

/// One host taking part in an operation, with its progress.
#[derive(Debug, Clone)]
pub struct FleetMember {
    target: SshTarget,
    progress: HostProgress,
}

impl FleetMember {
    /// The host as it was configured when the operation opened.
    ///
    /// A snapshot: later edits to the config do not reach it.
    pub fn target(&self) -> &SshTarget {
        &self.target
    }

    /// The host's configured name, as the panel shows it.
    pub fn name(&self) -> &str {
        &self.target.name
    }

    /// How far this host has got.
    pub fn progress(&self) -> HostProgress {
        self.progress
    }
}

// ---------------------------------------------------------------------------
// FleetOperation
// ---------------------------------------------------------------------------

/// One question asked of a fixed set of hosts: `operation → { host →
/// progress }`.
///
/// Built by [`open`][Self::open], which is the only moment the group's tag
/// rule is consulted.
#[derive(Debug, Clone)]
pub struct FleetOperation {
    id: OperationId,
    group: HostGroup,
    members: Vec<FleetMember>,
}

impl FleetOperation {
    /// Open an operation over the hosts `group` selects out of `targets`.
    ///
    /// The selection uses [`select_hosts_for_group`] and is made here, once.
    /// Both the hosts and the group definition are copied into the
    /// operation, so nothing that happens to the config afterwards can
    /// change who takes part or under which limits.
    ///
    /// An empty selection opens an empty operation rather than failing: a
    /// group whose rule currently matches nobody is a state to show, not an
    /// error (#418). The runner has nothing to do and the summary says so.
    ///
    /// Hosts keep the order they have in `targets`, which is the order the
    /// config file lists them in, so two operations over the same group read
    /// the same way.
    pub fn open(group: &HostGroup, targets: &[SshTarget]) -> Self {
        let members = select_hosts_for_group(group, targets)
            .into_iter()
            .map(|target| FleetMember {
                target: target.clone(),
                progress: HostProgress::default(),
            })
            .collect();
        Self {
            id: OperationId::next(),
            group: group.clone(),
            members,
        }
    }

    /// Rebuild an operation over a composition saved earlier (#442), not
    /// over whatever `group`'s tag rule selects today.
    ///
    /// `hosts` are the member names in their saved order. Each is looked up
    /// by name in `targets` — the connection details come from the config
    /// as it is now, the *membership* from the session — and the tag rule is
    /// not consulted at all: a host retagged since keeps its place, a host
    /// newly tagged does not gain one. That is the frozen-composition rule
    /// of [`open`][Self::open] carried across a restart.
    ///
    /// A name with no target of that name any more is returned in the second
    /// element, in saved order, rather than silently dropped: the caller has
    /// to be able to say that the fleet it restored is smaller than the one
    /// that was saved. When several targets share a name, saved occurrences
    /// take them in config order, one each.
    pub fn restore(group: &HostGroup, hosts: &[String], targets: &[SshTarget]) -> (Self, Vec<String>) {
        let mut used = vec![false; targets.len()];
        let mut members = Vec::with_capacity(hosts.len());
        let mut missing = Vec::new();
        for name in hosts {
            let found = targets
                .iter()
                .enumerate()
                .find(|(i, target)| !used[*i] && &target.name == name);
            match found {
                Some((i, target)) => {
                    used[i] = true;
                    members.push(FleetMember {
                        target: target.clone(),
                        progress: HostProgress::default(),
                    });
                }
                None => missing.push(name.clone()),
            }
        }
        let op = Self {
            id: OperationId::next(),
            group: group.clone(),
            members,
        };
        (op, missing)
    }

    /// A fresh operation over exactly this one's hosts and group (#435).
    ///
    /// Every question asked of a fleet is its own operation — a member that
    /// is [`Done`][HostProgress::Done] "will not be asked again in this
    /// operation" — but the composition was frozen when the fleet opened and
    /// must stay so. This copies the snapshot instead of calling
    /// [`open`][Self::open] again, which would re-resolve the tag rule
    /// against whatever the config says now. New id, all progress `Pending`.
    pub fn reopen(&self) -> Self {
        Self {
            id: OperationId::next(),
            group: self.group.clone(),
            members: self
                .members
                .iter()
                .map(|m| FleetMember {
                    target: m.target.clone(),
                    progress: HostProgress::default(),
                })
                .collect(),
        }
    }

    /// This operation's id.
    pub fn id(&self) -> OperationId {
        self.id
    }

    /// The group definition as it was when the operation opened.
    pub fn group(&self) -> &HostGroup {
        &self.group
    }

    /// The group's name, as the status bar shows it.
    pub fn group_name(&self) -> &str {
        &self.group.name
    }

    /// The policy frozen with the group.
    ///
    /// Fleet work is read-only whatever this says — the guarantee is
    /// `filar_transport::ReadOnlyExecutor` (#419), not a field — but the
    /// policy the operation was opened under is part of the record.
    pub fn policy(&self) -> HostGroupPolicy {
        self.group.policy
    }

    /// Upper bound on hosts worked on at the same time, from the group.
    pub fn max_parallel(&self) -> u32 {
        self.group.max_parallel
    }

    /// Deadline for one command on one host, from the group.
    ///
    /// Per host, not per operation: one wedged machine must not hold up the
    /// summary for the other eleven (#427).
    pub fn per_host_timeout(&self) -> Duration {
        Duration::from_secs(self.group.per_host_timeout_secs)
    }

    /// LLM profile for the fleet dialogue; `None` = the session's own.
    pub fn llm_profile(&self) -> Option<&str> {
        self.group.llm_profile.as_deref()
    }

    /// Every member, in composition order.
    pub fn members(&self) -> &[FleetMember] {
        &self.members
    }

    /// How many hosts take part.
    pub fn len(&self) -> usize {
        self.members.len()
    }

    /// Whether the group selected nobody.
    pub fn is_empty(&self) -> bool {
        self.members.is_empty()
    }

    /// Handles to every member, in composition order.
    pub fn handles(&self) -> impl Iterator<Item = HostHandle> {
        let operation = self.id;
        (0..self.members.len()).map(move |index| HostHandle { operation, index })
    }

    /// The member `handle` refers to.
    ///
    /// `None` exactly for a handle from a different operation — one this
    /// operation handed out never goes stale, because the composition never
    /// changes.
    pub fn member(&self, handle: HostHandle) -> Option<&FleetMember> {
        if handle.operation != self.id {
            return None;
        }
        self.members.get(handle.index)
    }

    /// Handle of the first member named `name`, if any.
    ///
    /// First, not only: `ssh_targets` does not enforce unique names, and
    /// this matches `Config::ssh_target`, which resolves the same ambiguity
    /// the same way. Code that must address every member uses
    /// [`handles`][Self::handles].
    pub fn handle_for(&self, name: &str) -> Option<HostHandle> {
        self.members
            .iter()
            .position(|m| m.target.name == name)
            .map(|index| HostHandle {
                operation: self.id,
                index,
            })
    }

    /// Whether a host of this name takes part.
    pub fn contains(&self, name: &str) -> bool {
        self.handle_for(name).is_some()
    }

    /// Record that `handle` has moved to `progress`.
    ///
    /// Progress is written by the runner, which owns the operation while it
    /// works; nothing here enforces an order, because a cancelled host goes
    /// from `Running` straight to `Done` and a host the runner never reaches
    /// stays `Pending`. A handle from another operation is ignored.
    pub fn set_progress(&mut self, handle: HostHandle, progress: HostProgress) {
        if handle.operation != self.id {
            return;
        }
        if let Some(member) = self.members.get_mut(handle.index) {
            member.progress = progress;
        }
    }

    /// How many members are at `progress`.
    pub fn count_at(&self, progress: HostProgress) -> usize {
        self.members
            .iter()
            .filter(|m| m.progress == progress)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HostKeyPolicy, SshAuth};

    fn group(name: &str, tags: &[&str]) -> HostGroup {
        HostGroup {
            name: name.into(),
            match_tags: tags.iter().map(|t| (*t).to_string()).collect(),
            max_parallel: 3,
            per_host_timeout_secs: 30,
            ..HostGroup::default()
        }
    }

    fn target(name: &str, tags: &[&str]) -> SshTarget {
        SshTarget {
            name: name.into(),
            host: format!("{name}.example.test"),
            port: 22,
            user: "admin".into(),
            auth: SshAuth::default(),
            host_key_policy: HostKeyPolicy::default(),
            tags: tags.iter().map(|t| (*t).to_string()).collect(),
        }
    }

    fn names(op: &FleetOperation) -> Vec<&str> {
        op.members().iter().map(FleetMember::name).collect()
    }

    #[test]
    fn open_selects_the_group_members_in_config_order() {
        let targets = vec![
            target("web-1", &["work", "prod"]),
            target("lab-1", &["work", "test"]),
            target("web-2", &["work", "prod"]),
        ];

        let op = FleetOperation::open(&group("prod", &["work", "prod"]), &targets);

        assert_eq!(names(&op), vec!["web-1", "web-2"]);
        assert_eq!(op.len(), 2);
        assert!(!op.is_empty());
        assert!(op.contains("web-2"));
        assert!(!op.contains("lab-1"));
    }

    #[test]
    fn composition_is_frozen_at_open_time() {
        let mut targets = vec![
            target("web-1", &["work", "prod"]),
            target("lab-1", &["work", "test"]),
        ];
        let rule = group("prod", &["work", "prod"]);

        let op = FleetOperation::open(&rule, &targets);
        assert_eq!(names(&op), vec!["web-1"]);

        // Re-tag a member out of the group, tag an outsider into it, and add
        // a brand-new matching host — everything that would change the
        // selection if it were recomputed.
        targets[0].tags = vec!["work".into(), "test".into()];
        targets[1].tags = vec!["work".into(), "prod".into()];
        targets.push(target("web-9", &["work", "prod"]));

        // The open operation is unmoved, while a freshly opened one over the
        // same rule sees the new world — proving the difference is the
        // snapshot and not the rule.
        assert_eq!(names(&op), vec!["web-1"]);
        assert_eq!(
            names(&FleetOperation::open(&rule, &targets)),
            vec!["lab-1", "web-9"]
        );
    }

    #[test]
    fn reopen_keeps_the_frozen_composition_with_fresh_progress() {
        let mut targets = vec![
            target("web-1", &["prod"]),
            target("web-2", &["prod"]),
        ];
        let rule = group("prod", &["prod"]);
        let mut op = FleetOperation::open(&rule, &targets);
        let first = op.handles().next().expect("two members");
        op.set_progress(first, HostProgress::Done);

        // The config moves on; a reopened operation must not follow it.
        targets[1].tags.clear();
        targets.push(target("web-9", &["prod"]));

        let again = op.reopen();
        assert_eq!(names(&again), vec!["web-1", "web-2"]);
        assert_ne!(again.id(), op.id(), "a new question is a new operation");
        assert_eq!(again.count_at(HostProgress::Pending), 2);
        assert_eq!(again.group_name(), "prod");
        assert!(again.member(first).is_none(), "old handles do not carry over");
    }

    #[test]
    fn group_limits_are_frozen_with_the_composition() {
        let targets = vec![target("web-1", &["prod"])];
        let mut rule = group("prod", &["prod"]);
        rule.llm_profile = Some("cheap".into());

        let op = FleetOperation::open(&rule, &targets);

        rule.max_parallel = 99;
        rule.per_host_timeout_secs = 1;
        rule.name = "renamed".into();

        assert_eq!(op.max_parallel(), 3);
        assert_eq!(op.per_host_timeout(), Duration::from_secs(30));
        assert_eq!(op.group_name(), "prod");
        assert_eq!(op.llm_profile(), Some("cheap"));
        assert_eq!(op.policy(), HostGroupPolicy::ReadOnly);
    }

    #[test]
    fn a_group_matching_nobody_opens_an_empty_operation() {
        let targets = vec![target("web-1", &["work", "prod"])];

        let op = FleetOperation::open(&group("staging", &["staging"]), &targets);

        assert!(op.is_empty());
        assert_eq!(op.len(), 0);
        assert_eq!(op.handles().count(), 0);
    }

    #[test]
    fn a_host_can_be_in_two_operations_at_once() {
        // Развилка 4: fleet membership is not exclusive. The same host in
        // two operations (and, by the same token, in a normal tab beside
        // them) gets an independent snapshot and an independent progress
        // cell in each.
        let targets = vec![target("web-1", &["work", "prod"])];
        let first = FleetOperation::open(&group("work", &["work"]), &targets);
        let mut second = FleetOperation::open(&group("prod", &["prod"]), &targets);

        assert!(first.contains("web-1"));
        assert!(second.contains("web-1"));
        assert_ne!(first.id(), second.id());

        let handle = second.handle_for("web-1").expect("member");
        second.set_progress(handle, HostProgress::Running);

        assert_eq!(second.count_at(HostProgress::Running), 1);
        assert_eq!(first.count_at(HostProgress::Pending), 1);
        assert_eq!(first.count_at(HostProgress::Running), 0);
    }

    #[test]
    fn members_start_pending_and_progress_is_recorded_per_host() {
        let targets = vec![
            target("web-1", &["prod"]),
            target("web-2", &["prod"]),
            target("web-3", &["prod"]),
        ];
        let mut op = FleetOperation::open(&group("prod", &["prod"]), &targets);

        assert_eq!(op.count_at(HostProgress::Pending), 3);

        let first = op.handle_for("web-1").expect("member");
        let second = op.handle_for("web-2").expect("member");
        op.set_progress(first, HostProgress::Running);
        op.set_progress(second, HostProgress::Running);
        op.set_progress(second, HostProgress::Done);

        assert_eq!(
            op.member(first).map(FleetMember::progress),
            Some(HostProgress::Running)
        );
        assert_eq!(
            op.member(second).map(FleetMember::progress),
            Some(HostProgress::Done)
        );
        assert_eq!(op.count_at(HostProgress::Pending), 1);
        assert_eq!(op.count_at(HostProgress::Running), 1);
        assert_eq!(op.count_at(HostProgress::Done), 1);
    }

    #[test]
    fn a_handle_from_another_operation_touches_nothing() {
        let wide = vec![target("web-1", &["prod"]), target("web-2", &["prod"])];
        let narrow = vec![target("web-1", &["prod"])];
        let rule = group("prod", &["prod"]);

        let big = FleetOperation::open(&rule, &wide);
        let mut small = FleetOperation::open(&rule, &narrow);

        let foreign = big.handles().last().expect("second member");
        assert!(small.member(foreign).is_none());

        small.set_progress(foreign, HostProgress::Done);
        assert_eq!(small.count_at(HostProgress::Done), 0);
        assert_eq!(small.count_at(HostProgress::Pending), 1);
    }

    #[test]
    fn a_foreign_handle_with_a_valid_index_touches_nothing() {
        // The case a size mismatch hides, and the one a runner juggling two
        // operations would actually hit: both hold a member at index 0, so
        // an index-only handle would have resolved in the wrong operation.
        let targets = vec![target("web-1", &["prod"])];
        let rule = group("prod", &["prod"]);

        let mut first = FleetOperation::open(&rule, &targets);
        let mut second = FleetOperation::open(&rule, &targets);

        let mine = first.handles().next().expect("member");
        let theirs = second.handles().next().expect("member");
        assert_ne!(mine, theirs);
        assert_eq!(mine.operation(), first.id());

        assert!(first.member(theirs).is_none());
        assert!(second.member(mine).is_none());

        first.set_progress(theirs, HostProgress::Done);
        second.set_progress(mine, HostProgress::Running);

        assert_eq!(first.count_at(HostProgress::Pending), 1);
        assert_eq!(second.count_at(HostProgress::Pending), 1);

        // The operation's own handle still works, so the guard rejects only
        // what it should.
        first.set_progress(mine, HostProgress::Done);
        assert_eq!(first.count_at(HostProgress::Done), 1);
    }

    #[test]
    fn handle_for_a_duplicated_name_resolves_to_the_first_member() {
        let targets = vec![target("web-1", &["prod"]), target("web-1", &["prod"])];
        let mut op = FleetOperation::open(&group("prod", &["prod"]), &targets);

        assert_eq!(op.len(), 2);
        let handle = op.handle_for("web-1").expect("member");
        op.set_progress(handle, HostProgress::Done);

        assert_eq!(handle, op.handles().next().expect("first member"));
        assert_eq!(op.count_at(HostProgress::Done), 1);
    }

    #[test]
    fn ids_are_unique_and_ordered_by_opening() {
        let targets = vec![target("web-1", &["prod"])];
        let rule = group("prod", &["prod"]);

        let first = FleetOperation::open(&rule, &targets);
        let second = FleetOperation::open(&rule, &targets);

        // Strict ordering only, never `+ 1`: the whole test binary shares
        // the counter, and a test running in parallel may take an id
        // between these two.
        assert!(first.id() < second.id());
        assert_eq!(
            second.id().to_string(),
            format!("#{}", second.id().as_u64())
        );
    }

    #[test]
    fn a_restored_operation_keeps_the_saved_composition_not_the_tags() {
        // Since the save: web-2 lost its tag, web-9 gained it, web-3 is gone.
        let targets = vec![target("web-1", &["web"]), target("web-2", &[]), target("web-9", &["web"])];
        let saved = vec!["web-1".to_string(), "web-2".to_string(), "web-3".to_string()];

        let (op, missing) = FleetOperation::restore(&group("web", &["web"]), &saved, &targets);
        assert_eq!(names(&op), ["web-1", "web-2"], "membership comes from the session");
        assert_eq!(missing, ["web-3"], "a vanished host is reported, not dropped silently");
        assert_eq!(op.group_name(), "web");
        assert!(!op.contains("web-9"), "a newly tagged host does not join");
    }

    #[test]
    fn duplicate_names_are_restored_one_target_each() {
        let mut a = target("dup", &[]);
        a.host = "a".into();
        let mut b = target("dup", &[]);
        b.host = "b".into();
        let saved = vec!["dup".to_string(), "dup".to_string(), "dup".to_string()];
        let (op, missing) = FleetOperation::restore(&group("g", &[]), &saved, &[a, b]);
        let hosts: Vec<_> = op.members().iter().map(|m| m.target().host.as_str()).collect();
        assert_eq!(hosts, ["a", "b"]);
        assert_eq!(missing, ["dup"]);
    }
}
