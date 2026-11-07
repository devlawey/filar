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
    AppliedObject, ClusterApi, ClusterWriteApi, DeleteOptions, DeleteOutcome, DeleteTarget,
    DeletedObject, ObjectKind, ObjectRef, RestartKind, ScaleKind, SecretKeyState, WorkloadKind,
};
use serde_json::{Map, Value};

use crate::{security::ConfirmDecision, ToolDef};

/// Longest diff shown in the confirmation window, in lines.
pub const MAX_DIFF_LINES: usize = 80;

/// Most objects one `k8s_apply` takes: the confirmation window does not
/// scroll, and every object must be visible before approval (#640).
pub const MAX_APPLY_OBJECTS: usize = 20;

/// Most object names the feed's command line lists; the rest are counted.
const MAX_DISPLAY_OBJECTS: usize = 5;

/// Diff lines of all objects of one apply together in the window.
const MAX_APPLY_DIFF_LINES: usize = 160;

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
    /// Destructive by the call alone: a delete, or scaling to 0. The dry
    /// run can tell more (scaling an already-empty workload to 0 is not).
    pub fn is_destructive(&self) -> bool {
        matches!(self, Self::Delete { .. } | Self::Scale { replicas: 0, .. })
    }

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
                        let mut names = objects
                            .iter()
                            .take(MAX_DISPLAY_OBJECTS)
                            .map(|object| {
                                format!(
                                    "{}/{}",
                                    object["kind"].as_str().unwrap_or_default(),
                                    object["metadata"]["name"].as_str().unwrap_or_default()
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        if objects.len() > MAX_DISPLAY_OBJECTS {
                            names.push_str(&format!(
                                " and {} more",
                                objects.len() - MAX_DISPLAY_OBJECTS
                            ));
                        }
                        names
                    })
                    .unwrap_or_else(|_| "<manifest>".into());
                match namespace {
                    Some(namespace) => format!("k8s_apply -n {namespace}: {objects}"),
                    None => format!("k8s_apply: {objects}"),
                }
            }
            Self::Scale {
                kind,
                namespace,
                name,
                replicas,
            } => format!(
                "k8s_scale {}/{name}{} →{replicas}",
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
                ns(Some(namespace)),
                to_revision
                    .map(|r| format!(" --to-revision {r}"))
                    .unwrap_or_default()
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

/// Tool name of a server-side apply (#640).
pub const TOOL_K8S_APPLY: &str = "k8s_apply";

/// Tool name of a scale (#641).
pub const TOOL_K8S_SCALE: &str = "k8s_scale";

/// Tool name of a rollout restart or undo (#641).
pub const TOOL_K8S_ROLLOUT: &str = "k8s_rollout";

/// Most replicas `k8s_scale` asks for; a bigger number is a typo more often
/// than a plan.
pub const MAX_SCALE_REPLICAS: i64 = 1000;

/// How long a scale, restart or undo waits for its rollout (#641).
pub const ROLLOUT_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// How often the rollout is read while waiting.
pub const ROLLOUT_POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// The write tools offered in a cluster tab with write access; a fleet
/// never gets them. More come with #641–#642.
pub fn kube_write_tool_definitions(mode: CommandConfirmMode) -> Vec<ToolDef> {
    let namespace = || serde_json::json!({"type": "string", "description": "Namespace; omit for the tab namespace."});
    vec![
        crate::kube_tools::definition(
        TOOL_K8S_APPLY,
        "Create or change Kubernetes objects from a YAML manifest (server-side apply, field manager filar), \
         one or more documents separated by ---. Any kind the user's RBAC allows: Pod, PersistentVolumeClaim, \
         Deployment, Service, ConfigMap, Job, RBAC, custom resources, cluster-scoped objects. Objects without \
         metadata.namespace go to the tab namespace. Every apply is first run as a server dry run and shown to \
         the user as a diff; nothing changes until the user approves. Secret values may be written but are never \
         shown back. A running pod cannot get new volumes or most spec changes: create a new pod or a Deployment. \
         After the apply, check the result by reading (k8s_get, k8s_describe, events).",
        Map::from_iter([(
            "manifest".to_string(),
            serde_json::json!({"type": "string", "description": "YAML manifest, one or more documents."}),
        )]),
        vec!["manifest"],
        mode,
    ),
        crate::kube_tools::definition(
            TOOL_K8S_SCALE,
            "Set the replicas of a Deployment, StatefulSet or ReplicaSet through its scale subresource. \
             Dry-run first and confirmed by the user, as every write; scaling to 0 stops the workload and is shown as destructive. \
             After the scale filar waits a bounded time for the rollout and reports ready/desired; check events if it is not ready.",
            Map::from_iter([
                ("kind".to_string(), serde_json::json!({"type": "string", "enum": ["Deployment", "StatefulSet", "ReplicaSet"]})),
                ("name".to_string(), serde_json::json!({"type": "string"})),
                ("namespace".to_string(), namespace()),
                ("replicas".to_string(), serde_json::json!({"type": "integer", "minimum": 0, "maximum": MAX_SCALE_REPLICAS})),
            ]),
            vec!["kind", "name", "replicas"],
            mode,
        ),
        crate::kube_tools::definition(
            TOOL_K8S_ROLLOUT,
            "Restart a rollout (Deployment, StatefulSet, DaemonSet: every pod is replaced, as kubectl rollout restart) \
             or undo a Deployment to an earlier revision (its pod template comes back, as kubectl rollout undo). \
             Read k8s_rollout_history before an undo. Dry-run first and confirmed by the user, as every write. \
             After it filar waits a bounded time for the rollout and reports ready/desired; check events if it is not ready.",
            Map::from_iter([
                ("action".to_string(), serde_json::json!({"type": "string", "enum": ["restart", "undo"]})),
                ("kind".to_string(), serde_json::json!({"type": "string", "enum": ["Deployment", "StatefulSet", "DaemonSet"], "description": "undo works only for Deployment"})),
                ("name".to_string(), serde_json::json!({"type": "string"})),
                ("namespace".to_string(), namespace()),
                ("to_revision".to_string(), serde_json::json!({"type": "integer", "minimum": 1, "description": "undo only; omit for the revision before the current one"})),
            ]),
            vec!["action", "kind", "name"],
            mode,
        ),
    ]
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

/// Parse a write tool call. A manifest that is not valid YAML or has no
/// objects is refused here, before any request (#640).
pub fn parse_kube_write_call(
    name: &str,
    args: &Value,
    default_namespace: Option<&str>,
) -> Result<KubeWriteCall> {
    match name {
        TOOL_K8S_APPLY => {
            let manifest = args
                .get("manifest")
                .and_then(Value::as_str)
                .filter(|manifest| !manifest.trim().is_empty())
                .ok_or_else(|| CoreError::Other("missing required argument manifest".into()))?;
            let objects = filar_transport::parse_manifest(manifest)?;
            if objects.len() > MAX_APPLY_OBJECTS {
                return Err(CoreError::Other(format!(
                    "the manifest has {} objects; one k8s_apply takes at most {MAX_APPLY_OBJECTS}, so the confirmation window can show every one — split it into several calls",
                    objects.len()
                )));
            }
            Ok(KubeWriteCall::Apply {
                manifest: manifest.to_string(),
                namespace: default_namespace.map(str::to_owned),
            })
        }
        TOOL_K8S_SCALE => {
            let kind = match required_str(args, "kind")?.to_ascii_lowercase().as_str() {
                "deployment" => ScaleKind::Deployment,
                "statefulset" => ScaleKind::StatefulSet,
                "replicaset" => ScaleKind::ReplicaSet,
                other => {
                    return Err(CoreError::Other(format!(
                        "k8s_scale works on Deployment, StatefulSet or ReplicaSet, not {other}"
                    )))
                }
            };
            let replicas = args
                .get("replicas")
                .and_then(Value::as_i64)
                .filter(|r| (0..=MAX_SCALE_REPLICAS).contains(r))
                .ok_or_else(|| {
                    CoreError::Other(format!(
                        "replicas must be an integer from 0 to {MAX_SCALE_REPLICAS}"
                    ))
                })?;
            Ok(KubeWriteCall::Scale {
                kind,
                namespace: namespace_of(args, default_namespace)?,
                name: required_str(args, "name")?,
                replicas: replicas as i32,
            })
        }
        TOOL_K8S_ROLLOUT => {
            let kind = required_str(args, "kind")?.to_ascii_lowercase();
            let namespace = namespace_of(args, default_namespace)?;
            let name = required_str(args, "name")?;
            match required_str(args, "action")?.as_str() {
                "restart" => Ok(KubeWriteCall::RolloutRestart {
                    kind: match kind.as_str() {
                        "deployment" => RestartKind::Deployment,
                        "statefulset" => RestartKind::StatefulSet,
                        "daemonset" => RestartKind::DaemonSet,
                        other => {
                            return Err(CoreError::Other(format!(
                                "rollout restart works on Deployment, StatefulSet or DaemonSet, not {other}"
                            )))
                        }
                    },
                    namespace,
                    name,
                }),
                "undo" => {
                    if kind != "deployment" {
                        return Err(CoreError::Other(
                            "rollout undo works only on a Deployment".into(),
                        ));
                    }
                    let to_revision = match args.get("to_revision") {
                        None | Some(Value::Null) => None,
                        Some(value) => Some(value.as_i64().filter(|r| *r >= 1).ok_or_else(
                            || CoreError::Other("to_revision must be a revision number from 1".into()),
                        )?),
                    };
                    Ok(KubeWriteCall::RolloutUndo {
                        namespace,
                        name,
                        to_revision,
                    })
                }
                other => Err(CoreError::Other(format!(
                    "action must be restart or undo, not {other}"
                ))),
            }
        }
        other => Err(CoreError::Other(format!(
            "unknown Kubernetes write tool {other}"
        ))),
    }
}

fn required_str(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| CoreError::Other(format!("missing required argument {key}")))
}

fn namespace_of(args: &Value, default_namespace: Option<&str>) -> Result<String> {
    args.get("namespace")
        .and_then(Value::as_str)
        .filter(|ns| !ns.is_empty())
        .or(default_namespace)
        .map(str::to_owned)
        .ok_or_else(|| CoreError::Other("namespace is required (the tab has none)".into()))
}

/// The workload a scale, restart or undo leaves rolling out (#641).
pub fn rollout_target(call: &KubeWriteCall) -> Option<(WorkloadKind, &str, &str)> {
    match call {
        KubeWriteCall::Scale {
            kind,
            namespace,
            name,
            ..
        } => Some((
            match kind {
                ScaleKind::Deployment => WorkloadKind::Deployment,
                ScaleKind::StatefulSet => WorkloadKind::StatefulSet,
                ScaleKind::ReplicaSet => WorkloadKind::ReplicaSet,
            },
            namespace,
            name,
        )),
        KubeWriteCall::RolloutRestart {
            kind,
            namespace,
            name,
        } => Some((
            match kind {
                RestartKind::Deployment => WorkloadKind::Deployment,
                RestartKind::StatefulSet => WorkloadKind::StatefulSet,
                RestartKind::DaemonSet => WorkloadKind::DaemonSet,
            },
            namespace,
            name,
        )),
        KubeWriteCall::RolloutUndo {
            namespace, name, ..
        } => Some((WorkloadKind::Deployment, namespace, name)),
        _ => None,
    }
}

/// Wait for a rollout by reading the workload every `poll`, at most `wait`:
/// done when ready and updated replicas reach the desired count, seen on
/// the second read or later (the first may still show the old state). The
/// caller makes it cancellable. Returns one line for the model.
pub async fn wait_rollout(
    api: &dyn ClusterApi,
    kind: WorkloadKind,
    namespace: &str,
    name: &str,
    wait: std::time::Duration,
    poll: std::time::Duration,
) -> String {
    let started = tokio::time::Instant::now();
    let mut reads = 0usize;
    let mut last: String;
    loop {
        if reads > 0 {
            tokio::time::sleep(poll).await;
        }
        reads += 1;
        match api.list_workloads(Some(namespace), kind).await {
            Ok(workloads) => match workloads.into_iter().find(|w| w.name == name) {
                Some(workload) => {
                    let desired = workload.desired.unwrap_or(0);
                    let ready = workload.ready.unwrap_or(0);
                    let updated = workload.updated.unwrap_or(desired);
                    last = format!("{ready}/{desired} ready, {updated} updated");
                    if reads >= 2 && ready == desired && updated == desired {
                        return format!(
                            "rollout of {name} complete: {last} (after {} s)",
                            started.elapsed().as_secs()
                        );
                    }
                }
                None => last = format!("{name} not found while waiting"),
            },
            Err(error) => last = format!("status unreadable: {error}"),
        }
        if started.elapsed() >= wait {
            return format!(
                "rollout of {name} not finished after {} s: {last}; check k8s_get, k8s_describe and events",
                wait.as_secs()
            );
        }
    }
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
    /// A better command line than the call alone gives, once the dry run
    /// knows more (`k8s_scale deployment/web -n shop 3→5`).
    pub display: Option<String>,
    /// For an undo: the revision the dry run rolled back to. The real undo
    /// gets exactly this one, even if a rollout finished meanwhile.
    pub undo_revision: Option<i64>,
}

impl WritePreview {
    /// `call` bound to what this dry run showed: an undo without a revision
    /// gets the previewed one.
    pub fn pin(&self, call: &KubeWriteCall) -> KubeWriteCall {
        match (call, self.undo_revision) {
            (
                KubeWriteCall::RolloutUndo {
                    namespace,
                    name,
                    to_revision: None,
                },
                Some(revision),
            ) => KubeWriteCall::RolloutUndo {
                namespace: namespace.clone(),
                name: name.clone(),
                to_revision: Some(revision),
            },
            _ => call.clone(),
        }
    }
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
    let mut undo_revision = None;
    let mut display = None;
    match call {
        KubeWriteCall::Apply {
            manifest,
            namespace,
        } => {
            let applied = api
                .apply(manifest, namespace.as_deref(), true)
                .await
                .map_err(explain_apply_error)?;
            text.push_str(&applied_list(&applied));
            let mut budget = MAX_APPLY_DIFF_LINES;
            for object in &applied {
                let section = applied_text(object);
                let lines = section.lines().count();
                if lines <= budget {
                    text.push('\n');
                    text.push_str(&section);
                    budget -= lines;
                } else {
                    text.push_str(&format!(
                        "\n{} {}: diff not shown (window limit); read it after the apply\n",
                        object.action.as_str(),
                        object_label(&object.object)
                    ));
                    budget = 0;
                }
            }
        }
        KubeWriteCall::Scale {
            kind,
            namespace,
            name,
            replicas,
        } => {
            let outcome = api.scale(*kind, namespace, name, *replicas, true).await?;
            // Every scale to 0 is destructive, already-empty or not: the
            // window and the proposal must agree.
            destructive = *replicas == 0;
            display = Some(format!(
                "k8s_scale {}/{name} -n {namespace} {}→{}",
                scale_kind(*kind),
                outcome.from,
                outcome.to
            ));
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
            undo_revision = Some(outcome.to_revision);
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
        undo_revision,
        display,
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

/// The objects of an apply, one line each, before their diffs: a long
/// manifest is first seen as a list.
/// One line per object, before any diff: even when diffs do not fit the
/// window, every object and every Secret key change is seen before approval.
fn applied_list(objects: &[AppliedObject]) -> String {
    let mut text = format!("apply {} object(s):\n", objects.len());
    for object in objects {
        let keys = object
            .secret_keys
            .iter()
            .filter(|key| key.state != SecretKeyState::Unchanged)
            .map(|key| format!("{} {}", key.key, key_state(key.state)))
            .collect::<Vec<_>>();
        let keys = if keys.is_empty() {
            String::new()
        } else {
            format!(" (keys: {})", keys.join(", "))
        };
        text.push_str(&format!(
            "  {} {}{keys}\n",
            object.action.as_str(),
            object_label(&object.object)
        ));
    }
    text
}

/// A refused apply the model can act on: a running pod's spec is mostly
/// immutable, so a new pod or a Deployment is the way, not a retry (#640).
fn explain_apply_error(error: CoreError) -> CoreError {
    let text = error.to_string();
    if text.contains("pod updates may not change fields") || text.contains("Forbidden: pod updates")
    {
        return CoreError::Other(format!(
            "{text} — a running pod cannot get new volumes or most spec changes: create a new pod (another name) or a Deployment instead of retrying"
        ));
    }
    error
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
    // A resource without a `status` subresource takes status through the
    // main endpoint: there status is content, not noise.
    text.push_str(&diff_with(
        object.live.as_ref(),
        &object.result,
        object.status_is_content,
    ));
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
fn without_noise(object: &Value, keep_status: bool) -> Value {
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
    if !keep_status {
        if let Value::Object(map) = &mut object {
            map.remove("status");
        }
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
    diff_with(live, result, false)
}

/// What [`diff`] says when nothing but noise changed.
const NO_CHANGE: &str = "(no change to the object's content)";

fn diff_with(live: Option<&Value>, result: &Value, keep_status: bool) -> String {
    let after = lines_of(&without_noise(result, keep_status));
    let before = live
        .map(|l| lines_of(&without_noise(l, keep_status)))
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
            out.push(format!("  {NO_CHANGE}"));
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
            "k8s_apply -n shop: <manifest>",
            "not an object"
        );
        let apply = KubeWriteCall::Apply {
            manifest: "apiVersion: v1\nkind: Secret\nmetadata: {name: db}\nstringData: {password: hunter2}\n---\napiVersion: v1\nkind: Pod\nmetadata: {name: debug}\n".into(),
            namespace: Some("shop".into()),
        };
        assert_eq!(
            apply.display_command(),
            "k8s_apply -n shop: Secret/db, Pod/debug"
        );
        let scale = KubeWriteCall::Scale {
            kind: ScaleKind::Deployment,
            namespace: "shop".into(),
            name: "web".into(),
            replicas: 0,
        };
        assert_eq!(
            scale.display_command(),
            "k8s_scale deployment/web -n shop →0"
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

    #[test]
    fn k8s_apply_parses_multi_document_yaml_into_the_tab_namespace() {
        let manifest = "apiVersion: v1\nkind: PersistentVolumeClaim\nmetadata: {name: data}\n---\napiVersion: rbac.authorization.k8s.io/v1\nkind: ClusterRole\nmetadata: {name: reader}\nrules: []\n";
        let call = parse_kube_write_call(
            TOOL_K8S_APPLY,
            &json!({"manifest": manifest, "explanation": "storage and a role"}),
            Some("shop"),
        )
        .unwrap();
        assert_eq!(
            call,
            KubeWriteCall::Apply {
                manifest: manifest.into(),
                namespace: Some("shop".into())
            }
        );
        assert_eq!(
            call.display_command(),
            "k8s_apply -n shop: PersistentVolumeClaim/data, ClusterRole/reader"
        );
        let no_tab =
            parse_kube_write_call(TOOL_K8S_APPLY, &json!({"manifest": manifest}), None).unwrap();
        assert_eq!(
            no_tab.display_command(),
            "k8s_apply: PersistentVolumeClaim/data, ClusterRole/reader"
        );
        for bad in [
            json!({}),
            json!({"manifest": "  "}),
            json!({"manifest": "---\n"}),
            json!({"manifest": "a: [b"}),
        ] {
            assert!(
                parse_kube_write_call(TOOL_K8S_APPLY, &bad, Some("shop")).is_err(),
                "{bad}"
            );
        }
        let definitions = kube_write_tool_definitions(CommandConfirmMode::Explain);
        assert_eq!(definitions[0].name, TOOL_K8S_APPLY);
        assert_eq!(
            definitions[0].parameters["required"],
            json!(["manifest", "explanation"])
        );
    }

    #[test]
    fn a_running_pod_refusal_says_what_to_do_instead() {
        let error = explain_apply_error(CoreError::AccessDenied(
            "Pod \"web\" is invalid: spec: Forbidden: pod updates may not change fields other than `spec.containers[*].image`".into(),
        ));
        assert!(error.to_string().contains("create a new pod"), "{error}");
        let other = explain_apply_error(CoreError::AccessDenied("forbidden".into()));
        assert_eq!(other.to_string(), "forbidden");
    }

    fn many_configmaps(count: usize) -> String {
        (0..count)
            .map(|i| format!("apiVersion: v1\nkind: ConfigMap\nmetadata: {{name: c{i}}}\n"))
            .collect::<Vec<_>>()
            .join("---\n")
    }

    #[test]
    fn a_long_apply_is_counted_in_the_feed_and_capped_in_size() {
        let call = KubeWriteCall::Apply {
            manifest: many_configmaps(8),
            namespace: Some("shop".into()),
        };
        assert_eq!(
            call.display_command(),
            "k8s_apply -n shop: ConfigMap/c0, ConfigMap/c1, ConfigMap/c2, ConfigMap/c3, ConfigMap/c4 and 3 more"
        );
        let args = |count| json!({"manifest": many_configmaps(count)});
        assert!(
            parse_kube_write_call(TOOL_K8S_APPLY, &args(MAX_APPLY_OBJECTS), Some("shop")).is_ok()
        );
        let error =
            parse_kube_write_call(TOOL_K8S_APPLY, &args(MAX_APPLY_OBJECTS + 1), Some("shop"))
                .unwrap_err()
                .to_string();
        assert!(error.contains("split it"), "{error}");
    }

    #[test]
    fn the_object_list_carries_secret_key_changes_before_any_diff() {
        let secret = AppliedObject {
            object: ObjectRef {
                api_version: "v1".into(),
                kind: "Secret".into(),
                namespace: Some("shop".into()),
                name: "db".into(),
            },
            action: filar_transport::ApplyAction::Configured,
            live: None,
            result: json!({}),
            secret_keys: vec![
                filar_transport::SecretKeyChange {
                    key: "password".into(),
                    state: SecretKeyState::Changed,
                },
                filar_transport::SecretKeyChange {
                    key: "user".into(),
                    state: SecretKeyState::Unchanged,
                },
            ],
            status_is_content: false,
        };
        let list = applied_list(&[secret]);
        assert!(
            list.contains("configured Secret shop/db (keys: password value changed)"),
            "{list}"
        );
        assert!(!list.contains("user"), "{list}");
    }

    #[test]
    fn k8s_scale_takes_scalable_kinds_and_bounded_replicas() {
        let parse = |args: Value| parse_kube_write_call(TOOL_K8S_SCALE, &args, Some("shop"));
        let call = parse(json!({"kind": "Deployment", "name": "web", "replicas": 5})).unwrap();
        assert_eq!(
            call.display_command(),
            "k8s_scale deployment/web -n shop →5"
        );
        assert_eq!(
            rollout_target(&call),
            Some((WorkloadKind::Deployment, "shop", "web"))
        );
        for kind in ["StatefulSet", "replicaset"] {
            assert!(
                parse(json!({"kind": kind, "name": "x", "replicas": 0})).is_ok(),
                "{kind}"
            );
        }
        for bad in [
            json!({"kind": "DaemonSet", "name": "x", "replicas": 1}),
            json!({"kind": "Deployment", "name": "x", "replicas": -1}),
            json!({"kind": "Deployment", "name": "x", "replicas": MAX_SCALE_REPLICAS + 1}),
            json!({"kind": "Deployment", "name": "x", "replicas": "3"}),
            json!({"kind": "Deployment", "replicas": 1}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
        assert!(
            parse_kube_write_call(
                TOOL_K8S_SCALE,
                &json!({"kind": "Deployment", "name": "x", "replicas": 1}),
                None
            )
            .is_err(),
            "no namespace"
        );
    }

    #[test]
    fn k8s_rollout_restarts_workloads_and_undoes_deployments() {
        let parse = |args: Value| parse_kube_write_call(TOOL_K8S_ROLLOUT, &args, Some("shop"));
        let restart =
            parse(json!({"action": "restart", "kind": "DaemonSet", "name": "agent"})).unwrap();
        assert_eq!(
            restart.display_command(),
            "k8s_rollout restart daemonset/agent -n shop"
        );
        let undo =
            parse(json!({"action": "undo", "kind": "Deployment", "name": "web", "to_revision": 7}))
                .unwrap();
        assert_eq!(
            undo.display_command(),
            "k8s_rollout undo deployment/web -n shop --to-revision 7"
        );
        let previous =
            parse(json!({"action": "undo", "kind": "Deployment", "name": "web"})).unwrap();
        assert_eq!(
            previous.display_command(),
            "k8s_rollout undo deployment/web -n shop"
        );
        for bad in [
            json!({"action": "undo", "kind": "StatefulSet", "name": "db"}),
            json!({"action": "undo", "kind": "Deployment", "name": "web", "to_revision": 0}),
            json!({"action": "restart", "kind": "ReplicaSet", "name": "web"}),
            json!({"action": "pause", "kind": "Deployment", "name": "web"}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
        let names: Vec<_> = kube_write_tool_definitions(CommandConfirmMode::Always)
            .into_iter()
            .map(|tool| tool.name)
            .collect();
        assert_eq!(names, [TOOL_K8S_APPLY, TOOL_K8S_SCALE, TOOL_K8S_ROLLOUT]);
    }

    fn workload(ready: i32, updated: i32) -> filar_transport::WorkloadInfo {
        filar_transport::WorkloadInfo {
            namespace: "shop".into(),
            name: "web".into(),
            kind: WorkloadKind::Deployment,
            desired: Some(5),
            ready: Some(ready),
            updated: Some(updated),
            available: Some(ready),
            age_seconds: None,
            images: Vec::new(),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn the_rollout_wait_reports_completion_or_the_last_status() {
        use crate::kube_tools::tests::MockCluster;
        let cluster = MockCluster::default();
        // The first read still shows the old state as "complete": not trusted.
        *cluster.workload_reads.lock().unwrap() = [
            vec![workload(5, 5)],
            vec![workload(3, 5)],
            vec![workload(5, 5)],
        ]
        .into_iter()
        .collect();
        let done = wait_rollout(
            &cluster,
            WorkloadKind::Deployment,
            "shop",
            "web",
            ROLLOUT_WAIT,
            ROLLOUT_POLL,
        )
        .await;
        assert!(
            done.starts_with("rollout of web complete: 5/5 ready, 5 updated"),
            "{done}"
        );

        let stuck = MockCluster::default();
        *stuck.workload_reads.lock().unwrap() = [vec![workload(2, 5)]].into_iter().collect();
        let late = wait_rollout(
            &stuck,
            WorkloadKind::Deployment,
            "shop",
            "web",
            ROLLOUT_WAIT,
            ROLLOUT_POLL,
        )
        .await;
        assert!(
            late.starts_with("rollout of web not finished after 120 s: 2/5 ready"),
            "{late}"
        );
        assert!(late.contains("events"), "{late}");
    }
}

#[cfg(test)]
mod destructive_tests {
    use super::*;

    #[test]
    fn deletes_and_scaling_to_zero_are_destructive_by_the_call() {
        let scale = |replicas| KubeWriteCall::Scale {
            kind: ScaleKind::Deployment,
            namespace: "shop".into(),
            name: "web".into(),
            replicas,
        };
        assert!(scale(0).is_destructive());
        assert!(!scale(3).is_destructive());
    }
    #[test]
    fn an_undo_without_a_revision_is_pinned_to_the_previewed_one() {
        let preview = WritePreview {
            text: String::new(),
            destructive: false,
            names: None,
            display: None,
            undo_revision: Some(2),
        };
        let undo = |to_revision| KubeWriteCall::RolloutUndo {
            namespace: "shop".into(),
            name: "web".into(),
            to_revision,
        };
        assert_eq!(preview.pin(&undo(None)), undo(Some(2)));
        assert_eq!(
            preview.pin(&undo(Some(5))),
            undo(Some(5)),
            "an explicit revision stays"
        );
    }

    #[test]
    fn a_status_change_shows_in_the_window_without_a_status_subresource() {
        let live = serde_json::json!({"kind": "Widget", "metadata": {"name": "w"},
            "spec": {"a": 1}, "status": {"phase": "old"}});
        let result = serde_json::json!({"kind": "Widget", "metadata": {"name": "w"},
            "spec": {"a": 2}, "status": {"phase": "new"}});
        let object = AppliedObject {
            object: ObjectRef {
                api_version: "example.com/v1".into(),
                kind: "Widget".into(),
                namespace: Some("shop".into()),
                name: "w".into(),
            },
            action: filar_transport::ApplyAction::Configured,
            live: Some(live),
            result,
            secret_keys: Vec::new(),
            status_is_content: true,
        };
        let text = applied_text(&object);
        assert!(text.contains("\"new\""), "status shown beside spec: {text}");
        assert!(text.contains("\"a\": 2"), "{text}");
    }
}
