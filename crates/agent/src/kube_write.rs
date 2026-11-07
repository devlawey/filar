//! The gate of writes to a Kubernetes cluster (#639): what a write call is,
//! its dry run and diff before the confirmation window, and what is left
//! after it — a trace for the feed and a redacted answer for the model.
//!
//! Every write is confirmed, in every mode ([`kube_write_confirmation_decision`]).
//! Before the window the call runs on the API server with `dryRun=All`
//! ([`preview`]); a refused dry run never opens a window. The window shows
//! per object the action and a diff of the live object against the dry-run
//! result, without bookkeeping noise. `Secret` values never reach this
//! module ([`filar_transport::ClusterWriteApi`] hides them): a Secret diff
//! shows its keys and whether each value changes.
//!
//! The write tools themselves (`k8s_apply`, `k8s_scale`, `k8s_rollout`,
//! `k8s_delete`) come with #640–#642; [`kube_write_tool_definitions`] lists
//! the ones that exist. Fleets never get them.

use filar_core::{CommandConfirmMode, CoreError, KubeCluster, Result, TagPolicy};
use filar_transport::{
    AppliedObject, ClusterWriteApi, DeleteOptions, DeleteOutcome, DeleteTarget, DeletedObject,
    ObjectKind, ObjectRef, RestartKind, ScaleKind, SecretKeyState,
};
use serde_json::{Map, Value};

use crate::{security::ConfirmDecision, ToolDef};

/// Longest diff shown in the confirmation window, in lines.
pub const MAX_DIFF_LINES: usize = 80;

/// Most objects of a delete listed by name in the window.
const MAX_LISTED_OBJECTS: usize = 25;

/// One write to a cluster, typed. The tools of #640–#642 parse into it.
#[derive(Debug, Clone, PartialEq)]
pub enum KubeWriteCall {
    /// Server-side apply of a manifest (`k8s_apply`, #640).
    Apply {
        /// YAML or JSON, one or more documents.
        manifest: String,
        /// Namespace for objects without `metadata.namespace`.
        namespace: Option<String>,
    },
    /// Set replicas (`k8s_scale`, #641).
    Scale {
        /// Workload kind.
        kind: ScaleKind,
        /// Namespace.
        namespace: String,
        /// Workload name.
        name: String,
        /// New `spec.replicas`.
        replicas: i32,
    },
    /// `kubectl rollout restart` (`k8s_rollout`, #641).
    RolloutRestart {
        /// Workload kind.
        kind: RestartKind,
        /// Namespace.
        namespace: String,
        /// Workload name.
        name: String,
    },
    /// `kubectl rollout undo` of a Deployment (`k8s_rollout`, #641).
    RolloutUndo {
        /// Namespace.
        namespace: String,
        /// Deployment name.
        name: String,
        /// Revision to return to; `None` = the one before the current.
        to_revision: Option<i64>,
    },
    /// Delete objects (`k8s_delete`, #642).
    Delete {
        /// Kind of the objects.
        kind: ObjectKind,
        /// Namespace; ignored for cluster-scoped kinds.
        namespace: Option<String>,
        /// Name or selector. After the dry run the real delete gets
        /// exactly the names the dry run listed.
        target: DeleteTarget,
        /// Propagation, grace period, force.
        options: DeleteOptions,
    },
}

impl KubeWriteCall {
    /// The tool this call comes from.
    pub fn tool_name(&self) -> &'static str {
        match self {
            Self::Apply { .. } => "k8s_apply",
            Self::Scale { .. } => "k8s_scale",
            Self::RolloutRestart { .. } | Self::RolloutUndo { .. } => "k8s_rollout",
            Self::Delete { .. } => "k8s_delete",
        }
    }

    /// The namespace the call names, if any.
    pub fn namespace(&self) -> Option<&str> {
        match self {
            Self::Apply { namespace, .. } | Self::Delete { namespace, .. } => namespace.as_deref(),
            Self::Scale { namespace, .. }
            | Self::RolloutRestart { namespace, .. }
            | Self::RolloutUndo { namespace, .. } => Some(namespace),
        }
    }

    /// Command-like label for the feed, the window and the safety rating.
    /// Display only; the write itself is the typed call. A manifest is shown
    /// as its kinds and names, never its content: it may hold Secret values.
    pub fn display_command(&self) -> String {
        let ns = |namespace: Option<&str>| {
            namespace
                .map(|namespace| format!(" -n {namespace}"))
                .unwrap_or_default()
        };
        match self {
            Self::Apply {
                manifest,
                namespace,
            } => {
                // Kinds and names only: values may be Secret data.
                let objects = filar_transport::parse_manifest(manifest)
                    .map(|objects| {
                        objects
                            .iter()
                            .map(|object| {
                                format!(
                                    "{}/{}",
                                    object["kind"].as_str().unwrap_or_default(),
                                    object["metadata"]["name"].as_str().unwrap_or_default()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_else(|_| "-f <manifest>".into());
                format!("k8s_apply {objects}{}", ns(namespace.as_deref()))
            }
            Self::Scale {
                kind,
                namespace,
                name,
                replicas,
            } => format!(
                "k8s_scale {}/{name} --replicas={replicas}{}",
                scale_kind(*kind),
                ns(Some(namespace))
            ),
            Self::RolloutRestart {
                kind,
                namespace,
                name,
            } => format!(
                "k8s_rollout restart {}/{name}{}",
                restart_kind(*kind),
                ns(Some(namespace))
            ),
            Self::RolloutUndo {
                namespace,
                name,
                to_revision,
            } => format!(
                "k8s_rollout undo deployment/{name}{}{}",
                to_revision
                    .map(|r| format!(" --to-revision={r}"))
                    .unwrap_or_default(),
                ns(Some(namespace))
            ),
            Self::Delete {
                kind,
                namespace,
                target,
                options,
            } => {
                let what = match target {
                    DeleteTarget::Name(name) => format!("{}/{name}", kind.kind),
                    DeleteTarget::Names(names) => format!("{} {}", kind.kind, names.join(" ")),
                    DeleteTarget::Selector { labels, fields } => {
                        let mut out = kind.kind.clone();
                        if let Some(labels) = labels {
                            out.push_str(&format!(" -l {labels}"));
                        }
                        if let Some(fields) = fields {
                            out.push_str(&format!(" --field-selector {fields}"));
                        }
                        out
                    }
                };
                let force = if options.force { " --force" } else { "" };
                format!("k8s_delete {what}{}{force}", ns(namespace.as_deref()))
            }
        }
    }
}

fn scale_kind(kind: ScaleKind) -> &'static str {
    match kind {
        ScaleKind::Deployment => "deployment",
        ScaleKind::StatefulSet => "statefulset",
        ScaleKind::ReplicaSet => "replicaset",
    }
}

fn restart_kind(kind: RestartKind) -> &'static str {
    match kind {
        RestartKind::Deployment => "deployment",
        RestartKind::StatefulSet => "statefulset",
        RestartKind::DaemonSet => "daemonset",
    }
}

/// The write tools offered in a cluster tab with write access. Empty until
/// the tools of #640–#642 are added; a fleet never gets them.
pub fn kube_write_tool_definitions(_mode: CommandConfirmMode) -> Vec<ToolDef> {
    Vec::new()
}

/// Tool-call arguments as the feed and the session may keep them: a
/// manifest is replaced by its size, since it may hold Secret values.
pub fn redacted_arguments(arguments: &Value) -> Value {
    let mut arguments = arguments.clone();
    if let Some(Value::String(manifest)) = arguments.get("manifest") {
        let note = format!("<manifest, {} bytes, not shown>", manifest.len());
        arguments["manifest"] = Value::String(note);
    }
    arguments
}

/// Whether `name` is a write tool (#640–#642).
pub fn is_kube_write_tool(name: &str) -> bool {
    matches!(
        name,
        "k8s_apply" | "k8s_scale" | "k8s_rollout" | "k8s_delete"
    )
}

/// Parse a write tool call. The tools come with #640–#642; until then every
/// name is unknown.
pub fn parse_kube_write_call(
    name: &str,
    _args: &Value,
    _default_namespace: Option<&str>,
) -> Result<KubeWriteCall> {
    Err(CoreError::Other(format!(
        "unknown Kubernetes write tool {name}"
    )))
}

/// The confirmation policy of a cluster write (#639): always asked, in every
/// mode — `allowlist` and `never` included. Tag policies can only tighten,
/// and there is nothing tighter than always.
pub fn kube_write_confirmation_decision(
    _mode: CommandConfirmMode,
    _target: &KubeCluster,
    _policies: &[TagPolicy],
) -> ConfirmDecision {
    ConfirmDecision::NeedsConfirmation
}

/// What a dry run showed: the window text and whether it is destructive.
#[derive(Debug, Clone, PartialEq)]
pub struct WritePreview {
    /// Per object: action and diff, ready for the confirmation window.
    pub text: String,
    /// Deletes and scaling to 0 are destructive: the window says so.
    pub destructive: bool,
    /// For a selector delete: the names the dry run listed. The real delete
    /// gets exactly these.
    pub names: Option<Vec<String>>,
}

/// What an executed write left: a feed trace per object and the answer for
/// the model.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteReport {
    /// `<action> <kind> <ns>/<name>` per object, for the trace line.
    pub changes: Vec<String>,
    /// Summary for the model. Holds no Secret values: the transport never
    /// returns them.
    pub summary: String,
}

/// Run `call` as a dry run (`dryRun=All`) and describe it for the window.
pub async fn preview(api: &dyn ClusterWriteApi, call: &KubeWriteCall) -> Result<WritePreview> {
    let mut text = String::new();
    let mut destructive = false;
    let mut names = None;
    match call {
        KubeWriteCall::Apply {
            manifest,
            namespace,
        } => {
            let applied = api.apply(manifest, namespace.as_deref(), true).await?;
            for object in &applied {
                text.push_str(&applied_text(object));
            }
        }
        KubeWriteCall::Scale {
            kind,
            namespace,
            name,
            replicas,
        } => {
            let outcome = api.scale(*kind, namespace, name, *replicas, true).await?;
            destructive = outcome.to == 0 && outcome.from > 0;
            text.push_str(&format!(
                "scale {}: replicas {} → {}\n",
                object_label(&outcome.object),
                outcome.from,
                outcome.to
            ));
        }
        KubeWriteCall::RolloutRestart {
            kind,
            namespace,
            name,
        } => {
            let outcome = api.rollout_restart(*kind, namespace, name, true).await?;
            text.push_str(&format!(
                "restart {}: every pod is replaced\n",
                object_label(&outcome.object)
            ));
            text.push_str(&diff(Some(&outcome.live), &outcome.result));
        }
        KubeWriteCall::RolloutUndo {
            namespace,
            name,
            to_revision,
        } => {
            let outcome = api
                .rollout_undo(namespace, name, *to_revision, true)
                .await?;
            text.push_str(&format!(
                "undo {}: revision {} → template of revision {}\n",
                object_label(&outcome.write.object),
                outcome.from_revision,
                outcome.to_revision
            ));
            text.push_str(&diff(Some(&outcome.write.live), &outcome.write.result));
        }
        KubeWriteCall::Delete {
            kind,
            namespace,
            target,
            options,
        } => {
            destructive = true;
            let listed = api
                .delete(kind, namespace.as_deref(), target, options, true)
                .await?;
            if listed.is_empty() {
                return Err(CoreError::NotFound(
                    "nothing matches: no object would be deleted".into(),
                ));
            }
            text.push_str(&deleted_text(&listed, true));
            if matches!(target, DeleteTarget::Selector { .. }) {
                names = Some(listed.iter().map(|d| d.object.name.clone()).collect());
            }
        }
    }
    Ok(WritePreview {
        text,
        destructive,
        names,
    })
}

/// Run `call` for real. A selector delete gets the names of its dry run,
/// so nothing beyond what the window showed is deleted.
pub async fn execute(
    api: &dyn ClusterWriteApi,
    call: &KubeWriteCall,
    previewed_names: Option<Vec<String>>,
) -> Result<WriteReport> {
    let mut changes = Vec::new();
    let mut summary = String::new();
    match call {
        KubeWriteCall::Apply {
            manifest,
            namespace,
        } => {
            for object in api.apply(manifest, namespace.as_deref(), false).await? {
                let line = format!(
                    "{} {}",
                    object.action.as_str(),
                    object_label(&object.object)
                );
                summary.push_str(&line);
                if !object.secret_keys.is_empty() {
                    let keys: Vec<_> = object
                        .secret_keys
                        .iter()
                        .map(|key| format!("{} ({})", key.key, key_state(key.state)))
                        .collect();
                    summary.push_str(&format!("; keys: {} (values hidden)", keys.join(", ")));
                }
                summary.push('\n');
                if object.action != filar_transport::ApplyAction::Unchanged {
                    changes.push(line);
                }
            }
        }
        KubeWriteCall::Scale {
            kind,
            namespace,
            name,
            replicas,
        } => {
            let outcome = api.scale(*kind, namespace, name, *replicas, false).await?;
            let line = format!(
                "scaled {} {} → {}",
                object_label(&outcome.object),
                outcome.from,
                outcome.to
            );
            summary.push_str(&line);
            summary.push('\n');
            changes.push(line);
        }
        KubeWriteCall::RolloutRestart {
            kind,
            namespace,
            name,
        } => {
            let outcome = api.rollout_restart(*kind, namespace, name, false).await?;
            let line = format!("restarted {}", object_label(&outcome.object));
            summary.push_str(&line);
            summary.push('\n');
            changes.push(line);
        }
        KubeWriteCall::RolloutUndo {
            namespace,
            name,
            to_revision,
        } => {
            let outcome = api
                .rollout_undo(namespace, name, *to_revision, false)
                .await?;
            let line = format!(
                "rolled back {} from revision {} to the template of revision {}",
                object_label(&outcome.write.object),
                outcome.from_revision,
                outcome.to_revision
            );
            summary.push_str(&line);
            summary.push('\n');
            changes.push(line);
        }
        KubeWriteCall::Delete {
            kind,
            namespace,
            target,
            options,
        } => {
            let target = match previewed_names {
                Some(names) => DeleteTarget::Names(names),
                None => target.clone(),
            };
            let deleted = api
                .delete(kind, namespace.as_deref(), &target, options, false)
                .await?;
            summary.push_str(&deleted_text(&deleted, false));
            changes.extend(
                deleted
                    .iter()
                    .filter(|d| d.outcome != DeleteOutcome::AlreadyGone)
                    .map(|d| format!("deleted {}", object_label(&d.object))),
            );
        }
    }
    Ok(WriteReport { changes, summary })
}

/// The feed trace of one change: «Изменение в кластере: … (контекст …, время)».
pub fn trace_line(change: &str, target: &KubeCluster, at: &str) -> String {
    format!(
        "Изменение в кластере: {change} (контекст {}, {at})",
        target.context
    )
}

fn object_label(object: &ObjectRef) -> String {
    match &object.namespace {
        Some(namespace) => format!("{} {namespace}/{}", object.kind, object.name),
        None => format!("{} {}", object.kind, object.name),
    }
}

fn key_state(state: SecretKeyState) -> &'static str {
    match state {
        SecretKeyState::Added => "added",
        SecretKeyState::Changed => "value changed",
        SecretKeyState::Unchanged => "unchanged",
        SecretKeyState::Removed => "removed",
    }
}

fn applied_text(object: &AppliedObject) -> String {
    let mut text = format!(
        "{} {}\n",
        object.action.as_str(),
        object_label(&object.object)
    );
    if object.action == filar_transport::ApplyAction::Unchanged {
        return text;
    }
    for key in &object.secret_keys {
        text.push_str(&format!(
            "  secret key {}: {}\n",
            key.key,
            key_state(key.state)
        ));
    }
    text.push_str(&diff(object.live.as_ref(), &object.result));
    text
}

fn deleted_text(objects: &[DeletedObject], dry_run: bool) -> String {
    let mut text = String::new();
    if dry_run {
        text.push_str(&format!("delete {} object(s):\n", objects.len()));
    }
    for object in objects.iter().take(MAX_LISTED_OBJECTS) {
        let what = match object.outcome {
            DeleteOutcome::WouldDelete => "delete",
            DeleteOutcome::Deleted => "deleted",
            DeleteOutcome::Pending => "deleting (finalizers or grace period)",
            DeleteOutcome::AlreadyGone => "already gone",
        };
        text.push_str(&format!("  {what} {}\n", object_label(&object.object)));
    }
    if objects.len() > MAX_LISTED_OBJECTS {
        text.push_str(&format!(
            "  … and {} more\n",
            objects.len() - MAX_LISTED_OBJECTS
        ));
    }
    text
}

/// The object without what is noise in a diff: bookkeeping metadata, the
/// last applied copy and status.
fn without_noise(object: &Value) -> Value {
    let mut object = object.clone();
    if let Some(Value::Object(metadata)) = object.get_mut("metadata") {
        for field in [
            "managedFields",
            "resourceVersion",
            "generation",
            "uid",
            "creationTimestamp",
            "selfLink",
        ] {
            metadata.remove(field);
        }
        if let Some(Value::Object(annotations)) = metadata.get_mut("annotations") {
            annotations.remove("kubectl.kubernetes.io/last-applied-configuration");
            annotations.remove("deployment.kubernetes.io/revision");
            if annotations.is_empty() {
                metadata.remove("annotations");
            }
        }
    }
    if let Value::Object(map) = &mut object {
        map.remove("status");
    }
    object
}

fn lines_of(object: &Value) -> Vec<String> {
    let sorted = sort_keys(object);
    serde_json::to_string_pretty(&sorted)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

fn sort_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), sort_keys(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_keys).collect()),
        other => other.clone(),
    }
}

/// A line diff of `live` against `result` without noise: `-`/`+` lines with
/// one line of context, at most [`MAX_DIFF_LINES`]. A new object is shown
/// whole, as `+` lines.
pub fn diff(live: Option<&Value>, result: &Value) -> String {
    let after = lines_of(&without_noise(result));
    let before = live
        .map(|l| lines_of(&without_noise(l)))
        .unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    if live.is_none() {
        out.extend(after.iter().map(|line| format!("+ {line}")));
    } else {
        let ops = line_diff(&before, &after);
        let changed: Vec<bool> = ops.iter().map(|op| !matches!(op, Op::Same(_))).collect();
        let mut last_shown: Option<usize> = None;
        for (index, op) in ops.iter().enumerate() {
            let near =
                (index.saturating_sub(1)..=(index + 1).min(ops.len() - 1)).any(|i| changed[i]);
            if !near {
                continue;
            }
            if last_shown.is_some_and(|last| index > last + 1) {
                out.push("  …".into());
            }
            last_shown = Some(index);
            out.push(match op {
                Op::Same(line) => format!("  {line}"),
                Op::Removed(line) => format!("- {line}"),
                Op::Added(line) => format!("+ {line}"),
            });
        }
        if out.is_empty() {
            out.push("  (no change to the object's content)".into());
        }
    }
    if out.len() > MAX_DIFF_LINES {
        let hidden = out.len() - MAX_DIFF_LINES;
        out.truncate(MAX_DIFF_LINES);
        out.push(format!("  … {hidden} more diff lines"));
    }
    let mut text = out.join("\n");
    text.push('\n');
    text
}

enum Op<'a> {
    Same(&'a str),
    Removed(&'a str),
    Added(&'a str),
}

/// Longest-common-subsequence line diff. Objects are small; a pathological
/// size falls back to "everything removed, everything added".
fn line_diff<'a>(before: &'a [String], after: &'a [String]) -> Vec<Op<'a>> {
    const MAX_CELLS: usize = 4_000_000;
    let (n, m) = (before.len(), after.len());
    if n.saturating_mul(m) > MAX_CELLS {
        return before
            .iter()
            .map(|l| Op::Removed(l))
            .chain(after.iter().map(|l| Op::Added(l)))
            .collect();
    }
    let mut table = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if before[i] == after[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut ops = Vec::new();
    while i < n && j < m {
        if before[i] == after[j] {
            ops.push(Op::Same(&before[i]));
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            ops.push(Op::Removed(&before[i]));
            i += 1;
        } else {
            ops.push(Op::Added(&after[j]));
            j += 1;
        }
    }
    ops.extend(before[i..].iter().map(|l| Op::Removed(l)));
    ops.extend(after[j..].iter().map(|l| Op::Added(l)));
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn every_mode_asks_for_a_write() {
        let target = KubeCluster::default();
        for mode in [
            CommandConfirmMode::Always,
            CommandConfirmMode::Explain,
            CommandConfirmMode::Allowlist,
            CommandConfirmMode::Never,
        ] {
            let loosening = [TagPolicy {
                tag: "dev".into(),
                confirm_mode: CommandConfirmMode::Never,
            }];
            assert_eq!(
                kube_write_confirmation_decision(mode, &target, &loosening),
                ConfirmDecision::NeedsConfirmation,
                "{mode:?}"
            );
        }
    }

    #[test]
    fn the_diff_drops_noise_and_shows_changes_with_context() {
        let live = json!({"apiVersion": "apps/v1", "kind": "Deployment",
            "metadata": {"name": "web", "resourceVersion": "7", "managedFields": [{"manager": "x"}],
                         "annotations": {"kubectl.kubernetes.io/last-applied-configuration": "{}"}},
            "spec": {"replicas": 2, "template": {"spec": {"containers": [{"image": "web:3", "name": "main"}]}}},
            "status": {"readyReplicas": 2}});
        let mut result = live.clone();
        result["spec"]["template"]["spec"]["containers"][0]["image"] = json!("web:4");
        result["metadata"]["resourceVersion"] = json!("8");
        result["status"]["readyReplicas"] = json!(1);
        let text = diff(Some(&live), &result);
        assert!(
            text.lines()
                .any(|l| l.starts_with("- ") && l.contains("\"image\": \"web:3\"")),
            "{text}"
        );
        assert!(
            text.lines()
                .any(|l| l.starts_with("+ ") && l.contains("\"image\": \"web:4\"")),
            "{text}"
        );
        for noise in [
            "resourceVersion",
            "managedFields",
            "readyReplicas",
            "last-applied",
        ] {
            assert!(!text.contains(noise), "{noise}: {text}");
        }
        assert!(
            !text.contains("replicas\": 2"),
            "far from the change: {text}"
        );
        let same = diff(Some(&live), &live);
        assert!(same.contains("no change"), "{same}");
    }

    #[test]
    fn a_new_object_is_shown_whole_and_long_diffs_are_cut() {
        let object = json!({"kind": "ConfigMap", "metadata": {"name": "x"}, "data": {"a": "1"}});
        let text = diff(None, &object);
        assert!(text.lines().all(|line| line.starts_with("+ ")), "{text}");
        let big: Map<String, Value> = (0..200).map(|i| (format!("k{i:03}"), json!(i))).collect();
        let text = diff(None, &json!({"data": big}));
        assert!(text.contains("more diff lines"), "{text}");
        assert!(text.lines().count() <= MAX_DIFF_LINES + 1);
    }

    #[test]
    fn display_commands_never_show_a_manifest() {
        let apply = KubeWriteCall::Apply {
            manifest: "kind: Secret\nstringData: {password: hunter2}".into(),
            namespace: Some("shop".into()),
        };
        assert_eq!(
            apply.display_command(),
            "k8s_apply -f <manifest> -n shop",
            "not an object"
        );
        let apply = KubeWriteCall::Apply {
            manifest: "apiVersion: v1\nkind: Secret\nmetadata: {name: db}\nstringData: {password: hunter2}\n---\napiVersion: v1\nkind: Pod\nmetadata: {name: debug}\n".into(),
            namespace: Some("shop".into()),
        };
        assert_eq!(
            apply.display_command(),
            "k8s_apply Secret/db Pod/debug -n shop"
        );
        let scale = KubeWriteCall::Scale {
            kind: ScaleKind::Deployment,
            namespace: "shop".into(),
            name: "web".into(),
            replicas: 0,
        };
        assert_eq!(
            scale.display_command(),
            "k8s_scale deployment/web --replicas=0 -n shop"
        );
        let delete = KubeWriteCall::Delete {
            kind: ObjectKind {
                api_version: None,
                kind: "Pod".into(),
            },
            namespace: Some("shop".into()),
            target: DeleteTarget::Selector {
                labels: Some("app=web".into()),
                fields: None,
            },
            options: DeleteOptions {
                force: true,
                ..DeleteOptions::default()
            },
        };
        assert_eq!(
            delete.display_command(),
            "k8s_delete Pod -l app=web -n shop --force"
        );
    }

    #[test]
    fn traces_name_the_context() {
        let target = KubeCluster {
            context: "prod-eu-admin".into(),
            ..KubeCluster::default()
        };
        assert_eq!(
            trace_line("configured Deployment shop/web", &target, "12:00:00"),
            "Изменение в кластере: configured Deployment shop/web (контекст prod-eu-admin, 12:00:00)"
        );
    }
}
