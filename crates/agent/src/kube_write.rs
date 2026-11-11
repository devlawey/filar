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
    DeletedObject, ObjectKind, ObjectRef, PinnedName, RestartKind, ScaleKind, SecretKeyState,
    WorkloadKind,
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
                let kind_name = kind.kind.to_ascii_lowercase();
                let what = match target {
                    DeleteTarget::Name(name) => format!("{kind_name}/{name}"),
                    DeleteTarget::Names(names) => {
                        let names: Vec<_> = names.iter().map(|n| n.name.as_str()).collect();
                        format!("{kind_name} {}", names.join(" "))
                    }
                    DeleteTarget::Selector { labels, fields } => {
                        let mut out = kind_name.clone();
                        if let Some(labels) = labels {
                            out.push_str(&format!(" -l {labels}"));
                        }
                        if let Some(fields) = fields {
                            out.push_str(&format!(" --field-selector {fields}"));
                        }
                        out
                    }
                };
                let mut flags = String::new();
                // The apiVersion picks between same-named kinds of two
                // groups: the window and the arbiter must see which.
                if let Some(api_version) = &kind.api_version {
                    flags.push_str(&format!(" --api-version={api_version}"));
                }
                match options.propagation {
                    filar_transport::Propagation::Background => {}
                    filar_transport::Propagation::Foreground => {
                        flags.push_str(" --cascade=foreground")
                    }
                    filar_transport::Propagation::Orphan => flags.push_str(" --cascade=orphan"),
                }
                if options.force {
                    flags.push_str(" --force");
                } else if let Some(seconds) = options.grace_period_seconds {
                    flags.push_str(&format!(" --grace-period={seconds}"));
                }
                format!("k8s_delete {what}{}{flags}", ns(namespace.as_deref()))
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

/// Tool name of a delete (#642).
pub const TOOL_K8S_DELETE: &str = "k8s_delete";

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
        crate::kube_tools::definition(
            TOOL_K8S_DELETE,
            "Delete Kubernetes objects of any kind the user's RBAC allows, cluster-scoped ones included \
             (Namespace, Node, PersistentVolume, ClusterRole, CRD) and custom resources: by name or by a label/field selector. \
             The most dangerous write: dry-run first, the user sees what goes with it (a namespace's contents, a CRD's objects, \
             volume data under reclaimPolicy Delete, a node's pods) and confirms. A selector deletes exactly what the dry run listed. \
             A pod managed by a controller is recreated by it. Prefer the smallest delete that solves the problem.",
            Map::from_iter([
                ("kind".to_string(), serde_json::json!({"type": "string", "description": "Kind (Pod), plural (pods) or plural.group (certificates.cert-manager.io)."})),
                ("api_version".to_string(), serde_json::json!({"type": "string", "description": "apiVersion when the kind is ambiguous (cert-manager.io/v1)."})),
                ("name".to_string(), serde_json::json!({"type": "string", "description": "Object name; or give a selector instead."})),
                ("label_selector".to_string(), serde_json::json!({"type": "string"})),
                ("field_selector".to_string(), serde_json::json!({"type": "string"})),
                ("namespace".to_string(), namespace()),
                ("propagation".to_string(), serde_json::json!({"type": "string", "enum": ["Background", "Foreground", "Orphan"], "default": "Background"})),
                ("grace_period_seconds".to_string(), serde_json::json!({"type": "integer", "minimum": 0})),
                ("force".to_string(), serde_json::json!({"type": "boolean", "default": false, "description": "gracePeriodSeconds=0: no graceful shutdown."})),
            ]),
            vec!["kind"],
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
                "restart"
                    if args
                        .get("to_revision")
                        .is_some_and(|revision| !revision.is_null()) =>
                {
                    Err(CoreError::Other(
                        "to_revision belongs to action undo; a restart keeps the current template"
                            .into(),
                    ))
                }
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
        TOOL_K8S_DELETE => {
            for key in [
                "api_version",
                "name",
                "label_selector",
                "field_selector",
                "propagation",
            ] {
                if let Some(other) = args.get(key).filter(|v| !v.is_null() && !v.is_string()) {
                    return Err(CoreError::Other(format!(
                        "{key} must be a string, not {other}"
                    )));
                }
            }
            let kind = ObjectKind {
                api_version: args
                    .get("api_version")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned),
                kind: required_str(args, "kind")?,
            };
            let text = |key: &str| {
                args.get(key)
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_owned)
            };
            let target = match (text("name"), text("label_selector"), text("field_selector")) {
                (Some(name), None, None) => DeleteTarget::Name(name),
                (None, labels, fields) if labels.is_some() || fields.is_some() => {
                    DeleteTarget::Selector { labels, fields }
                }
                (Some(_), _, _) => {
                    return Err(CoreError::Other(
                        "give either name or a selector, not both".into(),
                    ))
                }
                _ => {
                    return Err(CoreError::Other(
                        "give the name or a label/field selector".into(),
                    ))
                }
            };
            let propagation = match text("propagation").as_deref() {
                None | Some("Background") => filar_transport::Propagation::Background,
                Some("Foreground") => filar_transport::Propagation::Foreground,
                Some("Orphan") => filar_transport::Propagation::Orphan,
                Some(other) => {
                    return Err(CoreError::Other(format!(
                        "propagation must be Background, Foreground or Orphan, not {other}"
                    )))
                }
            };
            let grace_period_seconds = match args.get("grace_period_seconds") {
                None | Some(Value::Null) => None,
                Some(value) => Some(
                    value
                        .as_u64()
                        .and_then(|v| u32::try_from(v).ok())
                        .ok_or_else(|| {
                            CoreError::Other("grace_period_seconds must be 0 or more".into())
                        })?,
                ),
            };
            let force = match args.get("force") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(force)) => *force,
                Some(_) => return Err(CoreError::Other("force must be true or false".into())),
            };
            // Optional for cluster-scoped kinds, but never a wrong type.
            let namespace = namespace_of(args, default_namespace).ok();
            if let Some(other) = args
                .get("namespace")
                .filter(|ns| !ns.is_null() && !ns.is_string())
            {
                return Err(CoreError::Other(format!(
                    "namespace must be a string, not {other}"
                )));
            }
            Ok(KubeWriteCall::Delete {
                kind,
                namespace,
                target,
                options: DeleteOptions {
                    propagation,
                    grace_period_seconds,
                    force,
                },
            })
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
    let given = match args.get("namespace") {
        None | Some(Value::Null) => None,
        Some(Value::String(ns)) => Some(ns.trim()).filter(|ns| !ns.is_empty()),
        Some(other) => {
            return Err(CoreError::Other(format!(
                "namespace must be a string, not {other}"
            )))
        }
    };
    given
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

/// Wait for a rollout by reading the workload by name every `poll`, at most
/// `wait` — complete in the sense of `kubectl rollout status` (#657):
/// - the controller has seen the new spec (`observedGeneration ≥
///   generation`); a workload without these fields is trusted from the
///   second read on, as the first may still show the old state;
/// - updated and ready replicas reach the desired count;
/// - no pod of an old revision is left (`status.replicas ≤ updated`).
///
/// Reads with `get`, so a `list` right is not needed. The caller makes it
/// cancellable. Returns one line for the model.
pub async fn wait_rollout(
    api: &dyn ClusterApi,
    kind: WorkloadKind,
    namespace: &str,
    name: &str,
    wait: std::time::Duration,
    poll: std::time::Duration,
) -> String {
    let started = tokio::time::Instant::now();
    let deadline = started + wait;
    let mut reads = 0usize;
    let mut last = String::from("status not read yet");
    loop {
        if reads > 0 {
            tokio::time::sleep_until(deadline.min(tokio::time::Instant::now() + poll)).await;
        }
        reads += 1;
        let read = tokio::time::timeout_at(deadline, api.get_workload(namespace, kind, name));
        let Ok(read) = read.await else {
            return not_finished(name, wait, &last);
        };
        match read {
            Ok(Some(workload)) => {
                let (done, status) = rollout_status(&workload, reads);
                last = status;
                if done {
                    return format!(
                        "rollout of {name} complete: {last} (after {} s)",
                        started.elapsed().as_secs()
                    );
                }
            }
            Ok(None) => last = format!("{name} not found while waiting"),
            // No permission to read workloads: retrying cannot help.
            Err(error @ CoreError::AccessDenied(_)) => {
                return format!(
                    "rollout of {name} not followed: status unreadable ({error}); the change is applied"
                );
            }
            Err(error) => last = format!("status unreadable: {error}"),
        }
        if tokio::time::Instant::now() >= deadline {
            return not_finished(name, wait, &last);
        }
    }
}

/// Whether one read of the workload shows a finished rollout, and the
/// status line saying what is still awaited. `read` counts from 1.
fn rollout_status(workload: &filar_transport::WorkloadInfo, read: usize) -> (bool, String) {
    let desired = workload.desired.unwrap_or(0);
    let ready = workload.ready.unwrap_or(0);
    // The API may omit a zero count: missing means none updated. Only a
    // ReplicaSet has no updated count at all — its pods are all one revision.
    let updated = match workload.kind {
        WorkloadKind::ReplicaSet => workload.updated.unwrap_or(desired),
        _ => workload.updated.unwrap_or(0),
    };
    let counts = format!("{ready}/{desired} ready, {updated} updated");
    if let (Some(generation), Some(observed)) = (workload.generation, workload.observed_generation)
    {
        if observed < generation {
            return (
                false,
                format!("{counts}; waiting for the controller to observe generation {generation} (seen {observed})"),
            );
        }
    } else if read < 2 {
        return (false, counts);
    }
    if let Some(total) = workload.replicas {
        if total > updated {
            return (
                false,
                format!(
                    "{counts}; {} old replica(s) pending termination",
                    total - updated
                ),
            );
        }
    }
    (ready == desired && updated == desired, counts)
}

fn not_finished(name: &str, wait: std::time::Duration, last: &str) -> String {
    format!(
        "rollout of {name} not finished after {} s: {last}; check k8s_get, k8s_describe and events",
        wait.as_secs()
    )
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
    /// For a delete: the objects the dry run listed, by name and UID
    /// (#659). The real delete gets exactly these and leaves any of them
    /// recreated since.
    pub names: Option<Vec<PinnedName>>,
    /// A better command line than the call alone gives, once the dry run
    /// knows more (`k8s_scale deployment/web -n shop 3→5`).
    pub display: Option<String>,
    /// For an undo: the revision the dry run rolled back to. The real undo
    /// gets exactly this one, even if a rollout finished meanwhile.
    pub undo_revision: Option<i64>,
    /// Every object the dry run touched is cluster-scoped: the window and
    /// the risk scorer get no namespace, not the tab's (#642).
    pub cluster_scoped: bool,
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
    let mut cluster_scoped = false;
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
            if outcome
                .live
                .pointer("/spec/updateStrategy/type")
                .and_then(Value::as_str)
                == Some("OnDelete")
            {
                return Err(CoreError::Other(format!(
                    "{} uses updateStrategy OnDelete: a restart changes the template but replaces no pod; delete its pods one by one (k8s_delete) instead",
                    object_label(&outcome.object)
                )));
            }
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
            // Warnings first: a long list must not push them out of a window
            // that does not scroll.
            text.push_str(deletion_warnings(api, &listed, options).await.trim_start());
            text.push('\n');
            text.push_str(&deleted_text(&listed, true));
            // A cluster-scoped kind has no namespace: the dry run knows.
            if listed.iter().all(|d| d.object.namespace.is_none()) {
                cluster_scoped = true;
                let mut cluster_scoped = call.clone();
                if let KubeWriteCall::Delete { namespace, .. } = &mut cluster_scoped {
                    *namespace = None;
                }
                display = Some(cluster_scoped.display_command());
            }
            names = Some(
                listed
                    .iter()
                    .map(|d| PinnedName {
                        name: d.object.name.clone(),
                        uid: d.uid().map(str::to_owned),
                    })
                    .collect(),
            );
        }
    }
    Ok(WritePreview {
        text,
        destructive,
        names,
        undo_revision,
        display,
        cluster_scoped,
    })
}

/// Run `call` for real. A delete gets the objects of its dry run, pinned
/// by UID (#659), so nothing beyond what the window showed is deleted —
/// not even a same-named object recreated since.
pub async fn execute(
    api: &dyn ClusterWriteApi,
    call: &KubeWriteCall,
    previewed_names: Option<Vec<PinnedName>>,
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
            // Named here too: the list above stops at MAX_LISTED_OBJECTS.
            let recreated: Vec<_> = deleted
                .iter()
                .filter(|d| d.outcome == DeleteOutcome::Recreated)
                .map(|d| d.object.name.as_str())
                .collect();
            if !recreated.is_empty() {
                let mut names = recreated
                    .iter()
                    .take(MAX_LISTED_OBJECTS)
                    .copied()
                    .collect::<Vec<_>>()
                    .join(", ");
                if recreated.len() > MAX_LISTED_OBJECTS {
                    names.push_str(&format!(
                        " and {} more",
                        recreated.len() - MAX_LISTED_OBJECTS
                    ));
                }
                summary.push_str(&format!(
                    "Recreated since the preview, not deleted: {names}. If the new ones must go too, call k8s_delete again (a new preview).\n"
                ));
            }
            changes.extend(
                deleted
                    .iter()
                    .filter(|d| {
                        !matches!(
                            d.outcome,
                            DeleteOutcome::AlreadyGone | DeleteOutcome::Recreated
                        )
                    })
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

/// Most objects whose impact is read for the window: a long selector list
/// is warned about by kind, not object by object.
const MAX_IMPACT_READS: usize = 10;

/// What a delete takes with it, for the window (#642): controllers that
/// recreate, cascades, the contents of a Namespace, the objects of a CRD,
/// volume data, a node's pods, RBAC access, and the options.
async fn deletion_warnings(
    api: &dyn ClusterWriteApi,
    objects: &[DeletedObject],
    options: &DeleteOptions,
) -> String {
    let mut lines: Vec<String> = Vec::new();
    for object in objects.iter().take(MAX_IMPACT_READS) {
        let label = object_label(&object.object);
        let impact = api.deletion_impact(object).await;
        if let Some(controller) = &impact.controller {
            lines.push(format!(
                "{label} is managed by {controller}: the controller will recreate it"
            ));
        }
        lines.extend(kind_warnings(object, &impact, options, &label));
    }
    if objects.len() > MAX_IMPACT_READS {
        let rest = &objects[MAX_IMPACT_READS..];
        lines.push(format!(
            "{} more object(s) not inspected; the same kind warnings apply",
            rest.len()
        ));
        if rest.iter().any(|d| {
            matches!(
                d.object.kind.as_str(),
                "PersistentVolumeClaim" | "PersistentVolume" | "StatefulSet"
            )
        }) {
            lines.push(
                "⚠ volumes of the uninspected objects may have reclaimPolicy Delete: their DATA may be lost".into(),
            );
        }
    }
    let propagation = match options.propagation {
        filar_transport::Propagation::Background => "Background",
        filar_transport::Propagation::Foreground => "Foreground",
        filar_transport::Propagation::Orphan => "Orphan (dependents stay)",
    };
    lines.push(format!("⚠ propagationPolicy: {propagation}"));
    if options.force {
        lines.push("⚠ force: gracePeriodSeconds=0 — no graceful shutdown".into());
    } else if let Some(seconds) = options.grace_period_seconds {
        lines.push(format!("⚠ gracePeriodSeconds={seconds}"));
    }
    // Unreadable in the dry run: the real delete goes by name alone (#659).
    let unpinned: Vec<_> = objects
        .iter()
        .filter(|d| d.uid().is_none())
        .map(|d| d.object.name.as_str())
        .collect();
    if !unpinned.is_empty() {
        let more = unpinned.len().saturating_sub(MAX_IMPACT_READS);
        let mut shown = unpinned
            .iter()
            .take(MAX_IMPACT_READS)
            .copied()
            .collect::<Vec<_>>()
            .join(", ");
        if more > 0 {
            shown.push_str(&format!(" and {more} more"));
        }
        lines.push(format!(
            "⚠ identity not pinned (no UID in the dry run): {shown} — deleted by name, even if recreated meanwhile"
        ));
    }
    let mut text = String::from("\n");
    for line in lines {
        text.push_str(&line);
        text.push('\n');
    }
    text
}

/// Warnings by kind for one object of a delete.
fn kind_warnings(
    deleted: &DeletedObject,
    impact: &filar_transport::DeletionImpact,
    options: &DeleteOptions,
    label: &str,
) -> Vec<String> {
    let orphan = options.propagation == filar_transport::Propagation::Orphan;
    let count = |(count, more): (usize, bool)| {
        if more {
            format!("{count}+")
        } else {
            count.to_string()
        }
    };
    let object = &deleted.object;
    let mut lines = Vec::new();
    match object.kind.as_str() {
        "StatefulSet" if !orphan => {
            lines.push(format!("{label}: its pods go with it"));
            let retention = deleted.live.as_ref().map(|live| {
                live.pointer("/spec/persistentVolumeClaimRetentionPolicy/whenDeleted")
                    .and_then(Value::as_str)
                    .unwrap_or("Retain")
                    .to_string()
            });
            match retention.as_deref() {
                Some("Delete") => lines.push(format!(
                    "{label}: PVC retention whenDeleted=Delete — its PVCs and their DATA are deleted too"
                )),
                Some(_) => {}
                None => lines.push(format!(
                    "{label}: if its PVC retention is whenDeleted=Delete, its PVCs and their data go too"
                )),
            }
        }
        "Deployment" | "DaemonSet" | "ReplicaSet" if !orphan => {
            lines.push(format!("{label}: its pods go with it"));
        }
        "Job" | "CronJob" if !orphan => {
            lines.push(format!("{label}: its jobs and pods go with it"));
        }
        "Namespace" => {
            if impact.contents.is_empty() {
                lines.push(format!(
                    "{label}: EVERYTHING in this namespace is deleted (its contents could not be read or it is empty)"
                ));
            } else {
                let contents: Vec<String> = impact
                    .contents
                    .iter()
                    .map(|(kind, n, more)| format!("{kind} {}", count((*n, *more))))
                    .collect();
                let incomplete = if impact.contents_incomplete {
                    " (some kinds could not be read: the list may be incomplete)"
                } else {
                    ""
                };
                lines.push(format!(
                    "{label}: EVERYTHING in this namespace is deleted: {}{incomplete}",
                    contents.join(", ")
                ));
            }
        }
        "CustomResourceDefinition" => match impact.custom_objects {
            Some(objects) => lines.push(format!(
                "{label}: all {} object(s) of this type in all namespaces are deleted",
                count(objects)
            )),
            None => lines.push(format!(
                "{label}: all objects of this type in all namespaces are deleted"
            )),
        },
        "PersistentVolumeClaim" | "PersistentVolume" => match impact.reclaim_policy.as_deref() {
            Some("Delete") => lines.push(format!(
                "{label}: reclaimPolicy Delete — the volume's DATA WILL BE LOST"
            )),
            Some("Recycle") => lines.push(format!(
                "{label}: reclaimPolicy Recycle — the volume is scrubbed: its DATA WILL BE LOST"
            )),
            Some(policy) => lines.push(format!(
                "{label}: reclaimPolicy {policy} — the volume is kept"
            )),
            None => lines.push(format!(
                "{label}: if the volume's reclaimPolicy is Delete, its data will be lost"
            )),
        },
        "Node" => lines.push(format!(
            "{label}: {} pod(s) on it are garbage-collected later (not at once); controller-managed ones may be recreated elsewhere if capacity allows, standalone ones are lost — drain the node first (kubectl drain)",
            impact.node_pods.map(count).unwrap_or_else(|| "its".into())
        )),
        "Role" | "ClusterRole" | "RoleBinding" | "ClusterRoleBinding" => lines.push(format!(
            "{label}: if this grants your own access, you may lose access to the cluster"
        )),
        _ => {}
    }
    lines
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
            DeleteOutcome::Recreated => {
                text.push_str(&format!(
                    "  {}: recreated since preview (uid changed), not deleted\n",
                    object_label(&object.object)
                ));
                continue;
            }
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
            "k8s_delete pod -l app=web -n shop --force"
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
        assert_eq!(
            names,
            [
                TOOL_K8S_APPLY,
                TOOL_K8S_SCALE,
                TOOL_K8S_ROLLOUT,
                TOOL_K8S_DELETE
            ]
        );
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
            generation: None,
            observed_generation: None,
            replicas: None,
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
        assert_eq!(
            cluster.workload_reads.lock().unwrap().len(),
            1,
            "the intermediate status was read"
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

        let mut denied = MockCluster::default();
        denied.workloads_denied = true;
        let started = tokio::time::Instant::now();
        let refused = wait_rollout(
            &denied,
            WorkloadKind::Deployment,
            "shop",
            "web",
            ROLLOUT_WAIT,
            ROLLOUT_POLL,
        )
        .await;
        assert!(refused.contains("not followed"), "{refused}");
        assert!(started.elapsed() < ROLLOUT_POLL, "no retries on RBAC");
    }

    /// A workload whose status carries generations and the total of pods.
    fn observed(
        ready: i32,
        updated: i32,
        replicas: i32,
        generation: i64,
        observed: i64,
    ) -> filar_transport::WorkloadInfo {
        filar_transport::WorkloadInfo {
            generation: Some(generation),
            observed_generation: Some(observed),
            replicas: Some(replicas),
            ..workload(ready, updated)
        }
    }

    async fn wait_on(cluster: &crate::kube_tools::tests::MockCluster) -> String {
        wait_rollout(
            cluster,
            WorkloadKind::Deployment,
            "shop",
            "web",
            ROLLOUT_WAIT,
            ROLLOUT_POLL,
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn the_rollout_waits_for_the_controller_to_see_the_new_spec() {
        use crate::kube_tools::tests::MockCluster;
        // (а) All ready by the old spec: the controller has not seen gen 4.
        let stale = MockCluster::default();
        *stale.workload_reads.lock().unwrap() =
            [vec![observed(5, 5, 5, 4, 3)]].into_iter().collect();
        let late = wait_on(&stale).await;
        assert!(
            late.starts_with("rollout of web not finished after 120 s")
                && late.contains("waiting for the controller to observe generation 4 (seen 3)"),
            "{late}"
        );
        // Seen and done: complete on the first read, no second read needed.
        let seen = MockCluster::default();
        *seen.workload_reads.lock().unwrap() =
            [vec![observed(5, 5, 5, 4, 4)]].into_iter().collect();
        let started = tokio::time::Instant::now();
        let done = wait_on(&seen).await;
        assert!(
            done.starts_with("rollout of web complete: 5/5 ready, 5 updated"),
            "{done}"
        );
        assert!(
            started.elapsed() < ROLLOUT_POLL,
            "the generations make one read enough"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_rollout_waits_for_old_replicas_to_go() {
        use crate::kube_tools::tests::MockCluster;
        // (б) 5 updated and ready, 2 pods of the old ReplicaSet still there.
        let cluster = MockCluster::default();
        *cluster.workload_reads.lock().unwrap() =
            [vec![observed(5, 5, 7, 2, 2)], vec![observed(5, 5, 5, 2, 2)]]
                .into_iter()
                .collect();
        let done = wait_on(&cluster).await;
        assert!(done.starts_with("rollout of web complete"), "{done}");
        assert!(
            cluster.workload_reads.lock().unwrap().len() == 1,
            "the old pods were waited for"
        );

        let stuck = MockCluster::default();
        *stuck.workload_reads.lock().unwrap() =
            [vec![observed(5, 5, 7, 2, 2)]].into_iter().collect();
        let late = wait_on(&stuck).await;
        assert!(
            late.contains("2 old replica(s) pending termination"),
            "{late}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_missing_updated_count_is_none_updated() {
        use crate::kube_tools::tests::MockCluster;
        // Right after a restart: new generation seen, the five old pods are
        // ready, `updatedReplicas` (zero) omitted by the API.
        let restarted = MockCluster::default();
        *restarted.workload_reads.lock().unwrap() = [vec![filar_transport::WorkloadInfo {
            updated: None,
            ..observed(5, 0, 5, 3, 3)
        }]]
        .into_iter()
        .collect();
        let late = wait_on(&restarted).await;
        assert!(
            late.starts_with("rollout of web not finished")
                && late.contains("5/5 ready, 0 updated"),
            "{late}"
        );
        // A ReplicaSet has no updated count: its ready pods are the rollout.
        let replicaset = MockCluster::default();
        *replicaset.workload_reads.lock().unwrap() = [vec![filar_transport::WorkloadInfo {
            kind: WorkloadKind::ReplicaSet,
            updated: None,
            ..observed(5, 0, 5, 1, 1)
        }]]
        .into_iter()
        .collect();
        let done = wait_on(&replicaset).await;
        assert!(done.starts_with("rollout of web complete"), "{done}");
    }

    #[tokio::test(start_paused = true)]
    async fn the_rollout_is_read_by_name_without_a_list_right() {
        use crate::kube_tools::tests::MockCluster;
        // (в) `get` allowed, `list` refused.
        let mut cluster = MockCluster::default();
        cluster.workload_list_denied = true;
        *cluster.workload_reads.lock().unwrap() =
            [vec![observed(5, 5, 5, 1, 1)]].into_iter().collect();
        let done = wait_on(&cluster).await;
        assert!(done.starts_with("rollout of web complete"), "{done}");

        // (г) Deleted while waiting.
        let gone = MockCluster::default();
        *gone.workload_reads.lock().unwrap() = [vec![observed(3, 5, 5, 2, 2)], vec![]]
            .into_iter()
            .collect();
        let late = wait_on(&gone).await;
        assert!(late.contains("web not found while waiting"), "{late}");
    }

    #[test]
    fn rollout_arguments_that_do_not_belong_are_refused() {
        let parse = |args: Value| parse_kube_write_call(TOOL_K8S_ROLLOUT, &args, Some("shop"));
        let error = parse(
            json!({"action": "restart", "kind": "Deployment", "name": "web", "to_revision": 3}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("belongs to action undo"), "{error}");
        let error = parse(
            json!({"action": "restart", "kind": "Deployment", "name": "web", "namespace": 7}),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("namespace must be a string"), "{error}");
        assert!(parse(
            json!({"action": "restart", "kind": "Deployment", "name": "web", "namespace": null})
        )
        .is_ok());
    }

    struct OnDeleteWriter;

    #[async_trait::async_trait]
    impl ClusterWriteApi for OnDeleteWriter {
        async fn apply(&self, _: &str, _: Option<&str>, _: bool) -> Result<Vec<AppliedObject>> {
            unreachable!()
        }
        async fn scale(
            &self,
            _: ScaleKind,
            _: &str,
            _: &str,
            _: i32,
            _: bool,
        ) -> Result<filar_transport::ScaleOutcome> {
            unreachable!()
        }
        async fn rollout_restart(
            &self,
            _: RestartKind,
            namespace: &str,
            name: &str,
            _: bool,
        ) -> Result<filar_transport::WriteOutcome> {
            let live = json!({"spec": {"updateStrategy": {"type": "OnDelete"}}});
            Ok(filar_transport::WriteOutcome {
                object: ObjectRef {
                    api_version: "apps/v1".into(),
                    kind: "StatefulSet".into(),
                    namespace: Some(namespace.into()),
                    name: name.into(),
                },
                live: live.clone(),
                result: live,
            })
        }
        async fn rollout_undo(
            &self,
            _: &str,
            _: &str,
            _: Option<i64>,
            _: bool,
        ) -> Result<filar_transport::UndoOutcome> {
            unreachable!()
        }
        async fn delete(
            &self,
            _: &filar_transport::ObjectKind,
            _: Option<&str>,
            _: &filar_transport::DeleteTarget,
            _: &filar_transport::DeleteOptions,
            _: bool,
        ) -> Result<Vec<filar_transport::DeletedObject>> {
            unreachable!()
        }
    }

    #[tokio::test]
    async fn an_on_delete_restart_is_refused_before_the_window() {
        let call = KubeWriteCall::RolloutRestart {
            kind: RestartKind::StatefulSet,
            namespace: "shop".into(),
            name: "db".into(),
        };
        let error = preview(&OnDeleteWriter, &call)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("OnDelete") && error.contains("replaces no pod"),
            "{error}"
        );
    }

    #[test]
    fn k8s_delete_takes_any_kind_and_a_name_or_a_selector() {
        let parse = |args: Value| parse_kube_write_call(TOOL_K8S_DELETE, &args, Some("shop"));
        let pod = parse(json!({"kind": "Pod", "name": "web-7f9c"})).unwrap();
        assert_eq!(pod.display_command(), "k8s_delete pod/web-7f9c -n shop");
        let custom = parse(json!({"kind": "certificates.cert-manager.io", "api_version": "cert-manager.io/v1", "name": "tls"})).unwrap();
        assert!(
            matches!(&custom, KubeWriteCall::Delete { kind, .. } if kind.api_version.as_deref() == Some("cert-manager.io/v1"))
        );
        let selector = parse(json!({"kind": "Pod", "label_selector": "app=web", "propagation": "Foreground", "grace_period_seconds": 30})).unwrap();
        assert_eq!(
            selector.display_command(),
            "k8s_delete pod -l app=web -n shop --cascade=foreground --grace-period=30"
        );
        let forced = parse(json!({"kind": "Pod", "name": "stuck", "force": true})).unwrap();
        assert!(forced.display_command().ends_with("--force"));
        let node = parse_kube_write_call(
            TOOL_K8S_DELETE,
            &json!({"kind": "Node", "name": "worker-3"}),
            None,
        )
        .unwrap();
        assert_eq!(node.display_command(), "k8s_delete node/worker-3");
        for bad in [
            json!({"kind": "Pod"}),
            json!({"kind": "Pod", "name": "a", "label_selector": "app=web"}),
            json!({"name": "a"}),
            json!({"kind": "Pod", "name": "a", "propagation": "Later"}),
            json!({"kind": "Pod", "name": "a", "grace_period_seconds": -1}),
            json!({"kind": "Pod", "name": "a", "force": "yes"}),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad}");
        }
    }

    /// A writer whose dry-run delete lists one object of the asked kind and
    /// whose impact reading answers by kind.
    struct ImpactWriter;

    #[async_trait::async_trait]
    impl ClusterWriteApi for ImpactWriter {
        async fn apply(&self, _: &str, _: Option<&str>, _: bool) -> Result<Vec<AppliedObject>> {
            unreachable!()
        }
        async fn scale(
            &self,
            _: ScaleKind,
            _: &str,
            _: &str,
            _: i32,
            _: bool,
        ) -> Result<filar_transport::ScaleOutcome> {
            unreachable!()
        }
        async fn rollout_restart(
            &self,
            _: RestartKind,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<filar_transport::WriteOutcome> {
            unreachable!()
        }
        async fn rollout_undo(
            &self,
            _: &str,
            _: &str,
            _: Option<i64>,
            _: bool,
        ) -> Result<filar_transport::UndoOutcome> {
            unreachable!()
        }
        async fn delete(
            &self,
            kind: &ObjectKind,
            namespace: Option<&str>,
            target: &DeleteTarget,
            _: &DeleteOptions,
            _: bool,
        ) -> Result<Vec<DeletedObject>> {
            let DeleteTarget::Name(name) = target else {
                unreachable!()
            };
            Ok(vec![DeletedObject {
                object: ObjectRef {
                    api_version: "v1".into(),
                    kind: kind.kind.clone(),
                    namespace: namespace.map(str::to_owned),
                    name: name.clone(),
                },
                outcome: DeleteOutcome::WouldDelete,
                // A StatefulSet named `x` is read and deletes its PVCs; one
                // named `unread` could not be read.
                live: (kind.kind == "StatefulSet" && name == "x").then(|| {
                    json!({"spec": {"persistentVolumeClaimRetentionPolicy": {"whenDeleted": "Delete"}}})
                }),
            }])
        }
        async fn deletion_impact(&self, object: &DeletedObject) -> filar_transport::DeletionImpact {
            let mut impact = filar_transport::DeletionImpact::default();
            if object.object.name == "partial" {
                impact.contents = vec![("Pod".into(), 3, false)];
                impact.contents_incomplete = true;
                return impact;
            }
            if object.object.name == "recycled" {
                impact.reclaim_policy = Some("Recycle".into());
                return impact;
            }
            match object.object.kind.as_str() {
                "Pod" => impact.controller = Some("ReplicaSet web-7f9c".into()),
                "Namespace" => {
                    impact.contents = vec![("Pod".into(), 12, false), ("Secret".into(), 500, true)]
                }
                "CustomResourceDefinition" => impact.custom_objects = Some((42, false)),
                "PersistentVolumeClaim" => impact.reclaim_policy = Some("Delete".into()),
                "PersistentVolume" => impact.reclaim_policy = Some("Retain".into()),
                "Node" => impact.node_pods = Some((17, false)),
                _ => {}
            }
            impact
        }
    }

    async fn delete_window(kind: &str, force: bool) -> WritePreview {
        delete_named(kind, "x", force).await
    }

    async fn delete_named(kind: &str, name: &str, force: bool) -> WritePreview {
        let call = KubeWriteCall::Delete {
            kind: ObjectKind {
                api_version: None,
                kind: kind.into(),
            },
            namespace: Some("shop".into()),
            target: DeleteTarget::Name(name.into()),
            options: DeleteOptions {
                force,
                ..DeleteOptions::default()
            },
        };
        preview(&ImpactWriter, &call).await.unwrap()
    }

    #[tokio::test]
    async fn the_delete_window_says_what_goes_with_it() {
        let pod = delete_window("Pod", false).await;
        assert!(pod.destructive);
        assert!(
            pod.text
                .contains("managed by ReplicaSet web-7f9c: the controller will recreate it"),
            "{}",
            pod.text
        );
        assert!(
            pod.text.contains("⚠ propagationPolicy: Background"),
            "{}",
            pod.text
        );
        let namespace = delete_window("Namespace", false).await;
        assert_eq!(
            namespace.display, None,
            "the mock lists it with the tab namespace"
        );
        assert!(
            namespace
                .text
                .contains("EVERYTHING in this namespace is deleted: Pod 12, Secret 500+"),
            "{}",
            namespace.text
        );
        let crd = delete_window("CustomResourceDefinition", false).await;
        assert!(
            crd.text
                .contains("all 42 object(s) of this type in all namespaces"),
            "{}",
            crd.text
        );
        let pvc = delete_window("PersistentVolumeClaim", false).await;
        assert!(
            pvc.text
                .contains("reclaimPolicy Delete — the volume's DATA WILL BE LOST"),
            "{}",
            pvc.text
        );
        let pv = delete_window("PersistentVolume", false).await;
        assert!(
            pv.text
                .contains("reclaimPolicy Retain — the volume is kept"),
            "{}",
            pv.text
        );
        let node = delete_window("Node", false).await;
        assert!(
            node.text
                .contains("17 pod(s) on it are garbage-collected later (not at once); controller-managed ones may be recreated elsewhere if capacity allows, standalone ones are lost"),
            "{}",
            node.text
        );
        let statefulset = delete_window("StatefulSet", false).await;
        assert!(
            statefulset.text.contains(
                "PVC retention whenDeleted=Delete — its PVCs and their DATA are deleted too"
            ),
            "{}",
            statefulset.text
        );
        let unread = delete_named("StatefulSet", "unread", false).await;
        assert!(
            unread
                .text
                .contains("if its PVC retention is whenDeleted=Delete"),
            "{}",
            unread.text
        );
        let recycled = delete_named("PersistentVolumeClaim", "recycled", false).await;
        assert!(
            recycled
                .text
                .contains("reclaimPolicy Recycle — the volume is scrubbed"),
            "{}",
            recycled.text
        );
        let partial = delete_named("Namespace", "partial", false).await;
        assert!(
            partial.text.contains("Pod 3 (some kinds could not be read"),
            "{}",
            partial.text
        );
        let first = |text: &str, needle: &str| text.find(needle).unwrap_or(usize::MAX);
        assert!(
            first(&pod.text, "managed by") < first(&pod.text, "delete 1 object(s)"),
            "warnings come before the list: {}",
            pod.text
        );
        let binding = delete_window("ClusterRoleBinding", true).await;
        assert!(
            binding.text.contains("you may lose access"),
            "{}",
            binding.text
        );
        assert!(
            binding.text.contains("⚠ force: gracePeriodSeconds=0"),
            "{}",
            binding.text
        );
        let deployment = delete_window("Deployment", false).await;
        assert!(
            deployment.text.contains("its pods go with it"),
            "{}",
            deployment.text
        );
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
            cluster_scoped: false,
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

#[cfg(test)]
mod delete_review_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delete_arguments_of_the_wrong_type_are_refused() {
        let parse = |args: Value| parse_kube_write_call(TOOL_K8S_DELETE, &args, Some("shop"));
        for (key, args) in [
            (
                "name",
                json!({"kind": "Pod", "name": 7, "label_selector": "app=web"}),
            ),
            (
                "propagation",
                json!({"kind": "Pod", "name": "web", "propagation": true}),
            ),
            (
                "api_version",
                json!({"kind": "Widget", "name": "x", "api_version": 1}),
            ),
            (
                "namespace",
                json!({"kind": "Pod", "name": "web", "namespace": ["shop"]}),
            ),
        ] {
            let error = parse(args).unwrap_err().to_string();
            assert!(
                error.contains(&format!("{key} must be a string")),
                "{error}"
            );
        }
    }

    #[test]
    fn the_api_version_shows_in_the_delete_command() {
        let call = parse_kube_write_call(
            TOOL_K8S_DELETE,
            &json!({"kind": "Widget", "name": "x", "api_version": "example.com/v1"}),
            Some("shop"),
        )
        .unwrap();
        assert_eq!(
            call.display_command(),
            "k8s_delete widget/x -n shop --api-version=example.com/v1"
        );
    }
}

#[cfg(test)]
mod pinned_delete_tests {
    //! The real delete removes only what the window showed (#659).
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// Pods by name → UID; a delete with a UID precondition refuses a
    /// same-named successor, as the API server does.
    struct UidWriter {
        pods: Mutex<BTreeMap<String, Option<String>>>,
    }

    impl UidWriter {
        fn new(pods: &[(&str, Option<&str>)]) -> Self {
            Self {
                pods: Mutex::new(
                    pods.iter()
                        .map(|(name, uid)| (name.to_string(), uid.map(str::to_owned)))
                        .collect(),
                ),
            }
        }

        fn recreate(&self, name: &str, uid: &str) {
            self.pods
                .lock()
                .unwrap()
                .insert(name.into(), Some(uid.into()));
        }
    }

    #[async_trait::async_trait]
    impl ClusterWriteApi for UidWriter {
        async fn apply(&self, _: &str, _: Option<&str>, _: bool) -> Result<Vec<AppliedObject>> {
            unreachable!()
        }
        async fn scale(
            &self,
            _: ScaleKind,
            _: &str,
            _: &str,
            _: i32,
            _: bool,
        ) -> Result<filar_transport::ScaleOutcome> {
            unreachable!()
        }
        async fn rollout_restart(
            &self,
            _: RestartKind,
            _: &str,
            _: &str,
            _: bool,
        ) -> Result<filar_transport::WriteOutcome> {
            unreachable!()
        }
        async fn rollout_undo(
            &self,
            _: &str,
            _: &str,
            _: Option<i64>,
            _: bool,
        ) -> Result<filar_transport::UndoOutcome> {
            unreachable!()
        }
        async fn delete(
            &self,
            kind: &ObjectKind,
            namespace: Option<&str>,
            target: &DeleteTarget,
            _: &DeleteOptions,
            dry_run: bool,
        ) -> Result<Vec<DeletedObject>> {
            let mut pods = self.pods.lock().unwrap();
            let wanted: Vec<PinnedName> = match target {
                DeleteTarget::Name(name) => vec![PinnedName::unpinned(name.clone())],
                DeleteTarget::Names(names) => names.clone(),
                DeleteTarget::Selector { .. } => {
                    pods.keys().cloned().map(PinnedName::unpinned).collect()
                }
            };
            let mut out = Vec::new();
            for PinnedName { name, uid } in wanted {
                let live = pods.get(&name).cloned();
                let outcome = match (&live, &uid) {
                    (None, _) => DeleteOutcome::AlreadyGone,
                    (Some(have), Some(want)) if have.as_deref() != Some(want.as_str()) => {
                        DeleteOutcome::Recreated
                    }
                    _ if dry_run => DeleteOutcome::WouldDelete,
                    _ => {
                        pods.remove(&name);
                        DeleteOutcome::Deleted
                    }
                };
                out.push(DeletedObject {
                    object: ObjectRef {
                        api_version: "v1".into(),
                        kind: kind.kind.clone(),
                        namespace: namespace.map(str::to_owned),
                        name: name.clone(),
                    },
                    outcome,
                    live: live.map(|uid| match uid {
                        Some(uid) => json!({"metadata": {"name": name, "uid": uid}}),
                        None => json!({"metadata": {"name": name}}),
                    }),
                });
            }
            Ok(out)
        }
    }

    fn delete_call(args: Value) -> KubeWriteCall {
        parse_kube_write_call(TOOL_K8S_DELETE, &args, Some("shop")).unwrap()
    }

    #[tokio::test]
    async fn a_pod_recreated_while_the_window_is_open_is_not_deleted() {
        let api = UidWriter::new(&[("web-0", Some("uid-a"))]);
        let call = delete_call(json!({"kind": "Pod", "name": "web-0"}));
        let preview = preview(&api, &call).await.unwrap();
        assert_eq!(
            preview.names,
            Some(vec![PinnedName {
                name: "web-0".into(),
                uid: Some("uid-a".into())
            }]),
            "a delete by name is pinned too"
        );
        assert!(
            !preview.text.contains("identity not pinned"),
            "{}",
            preview.text
        );

        api.recreate("web-0", "uid-b");
        let report = execute(&api, &preview.pin(&call), preview.names.clone())
            .await
            .unwrap();
        assert!(report.changes.is_empty(), "{:?}", report.changes);
        assert!(
            report
                .summary
                .contains("Pod shop/web-0: recreated since preview (uid changed), not deleted"),
            "{}",
            report.summary
        );
        assert!(
            report
                .summary
                .contains("Recreated since the preview, not deleted: web-0. If the new ones must go too, call k8s_delete again"),
            "{}",
            report.summary
        );
        assert!(
            api.pods.lock().unwrap().contains_key("web-0"),
            "the successor is kept"
        );
    }

    #[tokio::test]
    async fn a_selector_delete_skips_only_the_recreated_object() {
        let api = UidWriter::new(&[
            ("web-0", Some("uid-a")),
            ("web-1", Some("uid-b")),
            ("web-2", Some("uid-c")),
        ]);
        let call = delete_call(json!({"kind": "Pod", "label_selector": "app=web"}));
        let preview = preview(&api, &call).await.unwrap();
        api.recreate("web-1", "uid-new");
        let report = execute(&api, &preview.pin(&call), preview.names.clone())
            .await
            .unwrap();
        assert_eq!(
            report.changes,
            ["deleted Pod shop/web-0", "deleted Pod shop/web-2"]
        );
        assert!(
            report
                .summary
                .contains("Pod shop/web-1: recreated since preview (uid changed), not deleted"),
            "{}",
            report.summary
        );
        let left: Vec<_> = api.pods.lock().unwrap().keys().cloned().collect();
        assert_eq!(left, ["web-1"]);
    }

    #[tokio::test]
    async fn a_recreated_object_past_the_listed_ones_is_still_named() {
        let pods: Vec<(String, String)> = (0..30)
            .map(|i| (format!("web-{i:02}"), format!("uid-{i}")))
            .collect();
        let refs: Vec<(&str, Option<&str>)> = pods
            .iter()
            .map(|(name, uid)| (name.as_str(), Some(uid.as_str())))
            .collect();
        let api = UidWriter::new(&refs);
        let call = delete_call(json!({"kind": "Pod", "label_selector": "app=web"}));
        let preview = preview(&api, &call).await.unwrap();
        api.recreate("web-28", "uid-new");
        let report = execute(&api, &preview.pin(&call), preview.names.clone())
            .await
            .unwrap();
        assert!(
            !report.summary.contains("Pod shop/web-28:"),
            "past the list limit: {}",
            report.summary
        );
        assert!(
            report
                .summary
                .contains("Recreated since the preview, not deleted: web-28."),
            "{}",
            report.summary
        );
        assert_eq!(report.changes.len(), 29);
    }

    #[tokio::test]
    async fn an_object_without_a_uid_is_flagged_and_deleted_by_name() {
        let api = UidWriter::new(&[("odd", None)]);
        let call = delete_call(json!({"kind": "Pod", "name": "odd"}));
        let preview = preview(&api, &call).await.unwrap();
        assert!(
            preview
                .text
                .contains("⚠ identity not pinned (no UID in the dry run): odd"),
            "{}",
            preview.text
        );
        let report = execute(&api, &preview.pin(&call), preview.names.clone())
            .await
            .unwrap();
        assert_eq!(report.changes, ["deleted Pod shop/odd"]);
    }
}
