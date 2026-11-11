//! Write access to the Kubernetes API (#638).
//!
//! [`ClusterWriteApi`] is a separate trait from the read-only
//! [`ClusterApi`](super::ClusterApi): fleets and read-only paths are handed
//! only `ClusterApi`, so they cannot write by type, not by a runtime check.
//! [`KubeClusterApi`](super::KubeClusterApi) implements both.
//!
//! filar keeps no list of forbidden kinds or operations: the limit is the
//! RBAC of the kubeconfig context, and the protection is the confirmation
//! gate of the caller (dry-run, diff, explicit approval). Every method takes
//! `dry_run` (`dryRun=All` on the API server).
//!
//! `Secret` objects are written and deleted like any other, but their values
//! never leave the write methods: in every object they return `data` and
//! `stringData` values are replaced by [`SECRET_HIDDEN`], a diff gets
//! [`SecretKeyChange`]s — keys and whether a value changed — instead, and a
//! submitted value quoted in a server error is masked too. The one exception
//! is [`parse_manifest`]: it parses the caller's own manifest and returns it
//! as given, values included, because `apply` sends exactly that.

use std::collections::BTreeMap;

use filar_core::{CoreError, KubeCluster, Result};
use k8s_openapi::api::apps::v1::{Deployment, ReplicaSet};
use kube::api::{
    ApiResource, DeleteParams, DynamicObject, GroupVersionKind, ListParams, Patch, PatchParams,
    PostParams, Preconditions, PropagationPolicy, ValidationDirective,
};
use kube::discovery::{ApiCapabilities, Discovery, Scope};
use kube::{Api, Client};
use serde_json::{json, Value};

use super::client::{api_error, is_rbac_message};
use super::KubeClusterApi;

/// The field manager of every filar write (server-side apply ownership).
pub const FIELD_MANAGER: &str = "filar";

/// What a `Secret` value is replaced with in everything the write methods
/// return (not in [`parse_manifest`], which returns the manifest as given).
pub const SECRET_HIDDEN: &str = "«скрыто»";

/// Most objects a selector delete may touch; a broader selector is refused
/// rather than cut, so the dry-run list is always the full list.
pub const MAX_DELETE_OBJECTS: usize = 2000;

/// The annotation `kubectl rollout restart` sets on the pod template.
pub const RESTARTED_AT_ANNOTATION: &str = "kubectl.kubernetes.io/restartedAt";

/// The ReplicaSet/Deployment annotation holding the rollout revision.
const REVISION_ANNOTATION: &str = "deployment.kubernetes.io/revision";

/// A kind named by the caller: `kind` alone (resolved through discovery,
/// core group first) or with `apiVersion` (`apps/v1`, `v1`, `example.com/v1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectKind {
    /// `apiVersion`, when known.
    pub api_version: Option<String>,
    /// Kind (`Deployment`), resource plural (`deployments`) or singular,
    /// case-insensitive.
    pub kind: String,
}

/// One object addressed by a write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectRef {
    /// `apiVersion` of the object.
    pub api_version: String,
    /// Kind of the object.
    pub kind: String,
    /// Namespace; `None` for cluster-scoped objects.
    pub namespace: Option<String>,
    /// Object name.
    pub name: String,
}

/// What server-side apply did (or, in a dry run, would do) to one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyAction {
    /// The object did not exist.
    Created,
    /// The object existed and its content changes.
    Configured,
    /// The object existed and nothing but bookkeeping changes.
    Unchanged,
}

impl ApplyAction {
    /// `created`, `configured` or `unchanged`, as `kubectl apply` says.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Configured => "configured",
            Self::Unchanged => "unchanged",
        }
    }
}

/// How one key of a `Secret` changes; the values themselves are never shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretKeyState {
    /// The key is new.
    Added,
    /// The key stays and its value changes.
    Changed,
    /// The key and its value stay.
    Unchanged,
    /// The key goes away.
    Removed,
}

/// One key of a `Secret` in a diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretKeyChange {
    /// The key.
    pub key: String,
    /// What happens to it.
    pub state: SecretKeyState,
}

/// One object of an apply, in manifest order.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedObject {
    /// The object.
    pub object: ObjectRef,
    /// What apply does to it.
    pub action: ApplyAction,
    /// The object before (`None` when created), with Secret values hidden.
    pub live: Option<Value>,
    /// The object after, as the API server returned it, Secret values hidden.
    pub result: Value,
    /// For a `Secret`: its keys and whether each value changes.
    pub secret_keys: Vec<SecretKeyChange>,
    /// The resource has no `status` subresource, so status is written with
    /// the object and a status change is a change of content (#639).
    pub status_is_content: bool,
}

/// Workload kinds with a `scale` subresource that filar scales.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScaleKind {
    /// `apps/v1` Deployment.
    Deployment,
    /// `apps/v1` StatefulSet.
    StatefulSet,
    /// `apps/v1` ReplicaSet.
    ReplicaSet,
}

impl ScaleKind {
    fn kind(self) -> &'static str {
        match self {
            Self::Deployment => "Deployment",
            Self::StatefulSet => "StatefulSet",
            Self::ReplicaSet => "ReplicaSet",
        }
    }
}

/// Result of a scale: replicas before and after.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleOutcome {
    /// The workload.
    pub object: ObjectRef,
    /// `spec.replicas` before.
    pub from: i32,
    /// `spec.replicas` after.
    pub to: i32,
}

/// Workload kinds `rollout restart` applies to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartKind {
    /// `apps/v1` Deployment.
    Deployment,
    /// `apps/v1` StatefulSet.
    StatefulSet,
    /// `apps/v1` DaemonSet.
    DaemonSet,
}

impl RestartKind {
    fn kind(self) -> &'static str {
        match self {
            Self::Deployment => "Deployment",
            Self::StatefulSet => "StatefulSet",
            Self::DaemonSet => "DaemonSet",
        }
    }
}

/// A patched object before and after the write.
#[derive(Debug, Clone, PartialEq)]
pub struct WriteOutcome {
    /// The object.
    pub object: ObjectRef,
    /// Before.
    pub live: Value,
    /// After, as the API server returned it.
    pub result: Value,
}

/// Result of a rollout undo.
#[derive(Debug, Clone, PartialEq)]
pub struct UndoOutcome {
    /// The Deployment before and after.
    pub write: WriteOutcome,
    /// Revision the Deployment was on.
    pub from_revision: i64,
    /// Revision whose template it gets back.
    pub to_revision: i64,
}

/// What a delete addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeleteTarget {
    /// One object by name.
    Name(String),
    /// Every object matching the selectors (`kubectl delete -l …`). The
    /// matching objects are listed first and deleted by name.
    Selector {
        /// Label selector.
        labels: Option<String>,
        /// Field selector.
        fields: Option<String>,
    },
    /// Exactly these objects — what a dry run listed. A name gone since is
    /// reported, not an error; nothing beyond these names is deleted. A name
    /// with a UID is deleted only while it is that same object (`uid`
    /// precondition, #659): one recreated under the same name since the dry
    /// run (a StatefulSet pod `web-0`) is left and reported as
    /// [`DeleteOutcome::Recreated`]. Without a UID the name is the identity,
    /// as in `kubectl delete`.
    Names(Vec<PinnedName>),
}

/// One object a dry run showed, by name and, when known, UID (#659).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinnedName {
    /// Object name.
    pub name: String,
    /// `metadata.uid` the dry run saw; `None` = not pinned, the name decides.
    pub uid: Option<String>,
}

impl PinnedName {
    /// A name without a UID: deleted by name alone.
    pub fn unpinned(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            uid: None,
        }
    }
}

/// Garbage-collection policy of a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Propagation {
    /// Dependents are deleted in the background (default).
    #[default]
    Background,
    /// Dependents are deleted first.
    Foreground,
    /// Dependents are left.
    Orphan,
}

/// Options of a delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DeleteOptions {
    /// `propagationPolicy`.
    pub propagation: Propagation,
    /// `gracePeriodSeconds`; `None` = the object's default.
    pub grace_period_seconds: Option<u32>,
    /// Force: `gracePeriodSeconds=0`.
    pub force: bool,
}

/// What a delete did (or would do) to one object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteOutcome {
    /// Dry run: would be deleted.
    WouldDelete,
    /// Deleted.
    Deleted,
    /// Deletion started; finalizers or a grace period keep it for now.
    Pending,
    /// Already gone when its turn came.
    AlreadyGone,
    /// Not deleted: an object of that name exists, but it is not the one the
    /// dry run showed (its UID changed, #659).
    Recreated,
}

/// One object of a delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeletedObject {
    /// The object.
    pub object: ObjectRef,
    /// What happened.
    pub outcome: DeleteOutcome,
    /// The object as the API server answered the delete, when it did
    /// (owner, reclaim policy, …), Secret values hidden (#642).
    pub live: Option<Value>,
}

impl DeletedObject {
    /// `metadata.uid` of the object as the API server answered, if it did.
    pub fn uid(&self) -> Option<&str> {
        self.live.as_ref()?.pointer("/metadata/uid")?.as_str()
    }
}

/// What else a delete takes with it or puts at risk (#642), as far as
/// reading shows. Empty fields mean nothing known, not nothing at risk.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeletionImpact {
    /// The controller that recreates the object (`ReplicaSet web-7f9c`).
    pub controller: Option<String>,
    /// For a Namespace: objects in it by kind, `(kind, count, more)`;
    /// `more` = the count stopped at a page limit.
    pub contents: Vec<(String, usize, bool)>,
    /// For a Namespace: some kinds could not be counted (discovery or a
    /// list failed), so `contents` may miss objects.
    pub contents_incomplete: bool,
    /// For a CustomResourceDefinition: its objects in all namespaces.
    pub custom_objects: Option<(usize, bool)>,
    /// For a PVC or PV: the volume's reclaim policy (`Delete`, `Retain`).
    pub reclaim_policy: Option<String>,
    /// For a Node: pods scheduled on it.
    pub node_pods: Option<(usize, bool)>,
}

/// Write access to Kubernetes objects (#638). Separate from
/// [`ClusterApi`](super::ClusterApi): fleets and read-only paths never get it.
///
/// Every method takes `dry_run`: the API server validates and admits the
/// request and answers as if it ran, but persists nothing (`dryRun=All`).
#[async_trait::async_trait]
pub trait ClusterWriteApi: Send + Sync {
    /// Server-side apply of a YAML or JSON manifest, one or more documents,
    /// in order (`fieldManager=filar`, no force, `fieldValidation=Strict`).
    /// A namespaced object without `metadata.namespace` goes to
    /// `default_namespace`.
    async fn apply(
        &self,
        manifest: &str,
        default_namespace: Option<&str>,
        dry_run: bool,
    ) -> Result<Vec<AppliedObject>>;

    /// Set `spec.replicas` through the `scale` subresource.
    async fn scale(
        &self,
        kind: ScaleKind,
        namespace: &str,
        name: &str,
        replicas: i32,
        dry_run: bool,
    ) -> Result<ScaleOutcome>;

    /// Restart a rollout as `kubectl rollout restart`: the pod template gets
    /// a fresh [`RESTARTED_AT_ANNOTATION`].
    async fn rollout_restart(
        &self,
        kind: RestartKind,
        namespace: &str,
        name: &str,
        dry_run: bool,
    ) -> Result<WriteOutcome>;

    /// Roll a Deployment back as `kubectl rollout undo`: its pod template
    /// becomes the template of the ReplicaSet of `to_revision`, or of the
    /// revision before the current one.
    async fn rollout_undo(
        &self,
        namespace: &str,
        name: &str,
        to_revision: Option<i64>,
        dry_run: bool,
    ) -> Result<UndoOutcome>;

    /// Delete objects of any kind, cluster-scoped ones included. A selector
    /// is resolved to names first; in a dry run the result is the full list.
    async fn delete(
        &self,
        kind: &ObjectKind,
        namespace: Option<&str>,
        target: &DeleteTarget,
        options: &DeleteOptions,
        dry_run: bool,
    ) -> Result<Vec<DeletedObject>>;

    /// What deleting `object` (from a dry run) takes with it, by reading:
    /// the controller that recreates it, the contents of a Namespace, the
    /// objects of a CRD, a volume's reclaim policy, a node's pods (#642).
    /// Unreadable parts stay empty. The default knows nothing.
    async fn deletion_impact(&self, object: &DeletedObject) -> DeletionImpact {
        let _ = object;
        DeletionImpact::default()
    }
}

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// Parse a manifest: YAML (one or more `---` documents) or JSON. Empty
/// documents are skipped; each object needs `apiVersion`, `kind` and
/// `metadata.name`. A `List` is not unpacked: send its items as documents.
///
/// The documents come back as given: `Secret` values are **not** hidden
/// (call [`hide_secret_values`] before showing a document to anyone).
pub fn parse_manifest(manifest: &str) -> Result<Vec<Value>> {
    // The parser's own message may quote the manifest (a Secret value next
    // to the error), so only the position goes out.
    let documents: Vec<Value> = serde_saphyr::from_multiple(manifest).map_err(|error| {
        CoreError::Other(match error.location() {
            Some(at) => format!(
                "the manifest is not valid YAML (line {}, column {})",
                at.line(),
                at.column()
            ),
            None => "the manifest is not valid YAML".into(),
        })
    })?;
    let objects: Vec<Value> = documents.into_iter().filter(|doc| !doc.is_null()).collect();
    if objects.is_empty() {
        return Err(CoreError::Other("the manifest has no objects".into()));
    }
    for (index, object) in objects.iter().enumerate() {
        let field = |pointer: &str| object.pointer(pointer).and_then(Value::as_str);
        let missing = ["/apiVersion", "/kind", "/metadata/name"]
            .into_iter()
            .find(|pointer| field(pointer).is_none_or(str::is_empty));
        if let Some(pointer) = missing {
            return Err(CoreError::Other(format!(
                "manifest document {} has no {}",
                index + 1,
                pointer.trim_start_matches('/').replace('/', ".")
            )));
        }
    }
    Ok(objects)
}

/// Replace every `data` and `stringData` value of a `Secret` with
/// [`SECRET_HIDDEN`]. Other kinds are left alone.
pub fn hide_secret_values(object: &mut Value) {
    if object.get("kind").and_then(Value::as_str) != Some("Secret") {
        return;
    }
    for field in ["data", "stringData"] {
        if let Some(Value::Object(map)) = object.get_mut(field) {
            for value in map.values_mut() {
                *value = Value::String(SECRET_HIDDEN.into());
            }
        }
    }
    // `kubectl apply` keeps the whole last applied object here, values too.
    if let Some(Value::Object(annotations)) = object.pointer_mut("/metadata/annotations") {
        annotations.remove("kubectl.kubernetes.io/last-applied-configuration");
    }
}

/// Shortest submitted `Secret` value [`mask_secret_values`] masks in place;
/// a message containing a shorter one is withheld whole, since masking
/// `ab` would garble the text and leaving it would leak it.
const MIN_MASKED_VALUE: usize = 4;

/// The values a manifest document submits for a `Secret`: `stringData`
/// values, `data` values as sent (base64) and decoded, longest first.
fn submitted_secret_values(document: &Value) -> Vec<String> {
    if document.get("kind").and_then(Value::as_str) != Some("Secret") {
        return Vec::new();
    }
    let mut values = Vec::new();
    for field in ["data", "stringData"] {
        let Some(Value::Object(map)) = document.get(field) else {
            continue;
        };
        for value in map.values().filter_map(Value::as_str) {
            values.push(value.to_string());
            if field == "stringData" {
                // As `data` would carry it, should a webhook quote that.
                if let Ok(Value::String(encoded)) =
                    serde_json::to_value(k8s_openapi::ByteString(value.as_bytes().to_vec()))
                {
                    values.push(encoded);
                }
            }
            if field == "data" {
                let decoded = serde_json::from_value::<k8s_openapi::ByteString>(Value::String(
                    value.to_string(),
                ))
                .ok()
                .and_then(|bytes| String::from_utf8(bytes.0).ok());
                values.extend(decoded);
            }
        }
    }
    values.retain(|value| !value.is_empty());
    values.sort_by_key(|value| std::cmp::Reverse(value.chars().count()));
    values.dedup();
    values
}

/// Mask submitted `Secret` values in an error message: an admission webhook
/// may quote the object it rejects.
fn mask_secret_values(error: CoreError, values: &[String]) -> CoreError {
    if values.is_empty() {
        return error;
    }
    let mask = |mut message: String| {
        for value in values {
            if value.chars().count() < MIN_MASKED_VALUE {
                if message.contains(value.as_str()) {
                    return format!(
                        "the Kubernetes API refused the Secret; its message is withheld because it may quote a submitted value ({SECRET_HIDDEN})"
                    );
                }
            } else {
                message = message.replace(value.as_str(), SECRET_HIDDEN);
            }
        }
        message
    };
    match error {
        CoreError::Config(m) => CoreError::Config(mask(m)),
        CoreError::Secret(m) => CoreError::Secret(mask(m)),
        CoreError::ConnectionLost(m) => CoreError::ConnectionLost(mask(m)),
        CoreError::ContextOverflow(m) => CoreError::ContextOverflow(mask(m)),
        CoreError::AccessDenied(m) => CoreError::AccessDenied(mask(m)),
        CoreError::NotFound(m) => CoreError::NotFound(mask(m)),
        CoreError::Unavailable(m) => CoreError::Unavailable(mask(m)),
        CoreError::Other(m) => CoreError::Other(mask(m)),
        other => CoreError::Other(mask(other.to_string())),
    }
}

/// Keys of a `Secret` before and after, and whether each value changes.
/// `stringData` values count as their `data` keys. Called on unredacted
/// objects only, inside this module.
fn secret_key_changes(live: Option<&Value>, result: &Value) -> Vec<SecretKeyChange> {
    let values = |object: &Value| -> BTreeMap<String, Value> {
        let mut out = BTreeMap::new();
        for field in ["data", "stringData"] {
            if let Some(Value::Object(map)) = object.get(field) {
                for (key, value) in map {
                    out.insert(key.clone(), value.clone());
                }
            }
        }
        out
    };
    let before = live.map(values).unwrap_or_default();
    let after = values(result);
    let mut changes: Vec<SecretKeyChange> = after
        .iter()
        .map(|(key, value)| SecretKeyChange {
            key: key.clone(),
            state: match before.get(key) {
                None => SecretKeyState::Added,
                Some(old) if old == value => SecretKeyState::Unchanged,
                Some(_) => SecretKeyState::Changed,
            },
        })
        .collect();
    changes.extend(
        before
            .keys()
            .filter(|key| !after.contains_key(*key))
            .map(|key| SecretKeyChange {
                key: key.clone(),
                state: SecretKeyState::Removed,
            }),
    );
    changes.sort_by(|a, b| a.key.cmp(&b.key));
    changes
}

/// The object without what changes on every write: bookkeeping metadata and,
/// when a `status` subresource owns it, status. Two objects equal here
/// differ in nothing a user changed.
fn content(object: &Value, status_is_content: bool) -> Value {
    let mut object = object.clone();
    if let Some(Value::Object(metadata)) = object.get_mut("metadata") {
        for field in [
            "managedFields",
            "resourceVersion",
            "generation",
            "creationTimestamp",
            "uid",
            "selfLink",
        ] {
            metadata.remove(field);
        }
    }
    if !status_is_content {
        if let Value::Object(map) = &mut object {
            map.remove("status");
        }
    }
    object
}

/// Without a `status` subresource (a CRD that does not declare one) status
/// is written through the main endpoint, so an apply can change it.
fn status_is_content(caps: &ApiCapabilities) -> bool {
    !caps
        .subresources
        .iter()
        .any(|(resource, _)| resource.plural == "status")
}

fn apply_action(live: Option<&Value>, result: &Value, status_is_content: bool) -> ApplyAction {
    let content = |object| content(object, status_is_content);
    match live {
        None => ApplyAction::Created,
        Some(live) if content(live) == content(result) => ApplyAction::Unchanged,
        Some(_) => ApplyAction::Configured,
    }
}

fn to_value<T: serde::Serialize>(object: &T) -> Result<Value> {
    serde_json::to_value(object).map_err(|error| CoreError::Other(error.to_string()))
}

fn gvk_of(api_version: &str, kind: &str) -> GroupVersionKind {
    match api_version.split_once('/') {
        Some((group, version)) => GroupVersionKind::gvk(group, version, kind),
        None => GroupVersionKind::gvk("", api_version, kind),
    }
}

fn now_rfc3339() -> String {
    k8s_openapi::jiff::Timestamp::now()
        .round(k8s_openapi::jiff::Unit::Second)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| k8s_openapi::jiff::Timestamp::now().to_string())
}

/// A write error in the style of the read errors: RBAC as «нет права …»,
/// an admission denial verbatim, a server-side apply conflict with the
/// owning manager, a validation failure with the server's text.
pub(super) fn write_error(
    target: &KubeCluster,
    error: kube::Error,
    verb: &str,
    resource: &str,
    namespace: Option<&str>,
) -> CoreError {
    match error {
        kube::Error::Api(status) if status.code == 409 && verb == "apply" => {
            CoreError::Other(format!(
                "server-side apply conflict on {resource}: {} — another field manager owns these fields; filar does not force (context {})",
                status.message, target.context
            ))
        }
        kube::Error::Api(status) if status.code == 409 => CoreError::Other(format!(
            "Kubernetes API conflict on {verb} {resource}: {} (context {})",
            status.message, target.context
        )),
        kube::Error::Api(status)
            if matches!(status.code, 400 | 422)
                || (status.code == 403 && !is_rbac_message(&status.message)) =>
        {
            // Validation (fieldValidation=Strict) and admission denials:
            // the server's own words are the explanation.
            CoreError::AccessDenied(format!(
                "Kubernetes API refused {verb} {resource}: {} (context {})",
                status.message, target.context
            ))
        }
        other => api_error(target, other, verb, resource, namespace),
    }
}

fn paused_error(kind: &str, name: &str, verb: &str) -> CoreError {
    CoreError::Other(format!(
        "{} {name} is paused: a {verb} would change nothing now and take effect on resume; resume it first (kubectl rollout resume)",
        kind.to_ascii_lowercase()
    ))
}

/// Resources `kind` names, one per kind and version: the kind, plural or
/// `plural.group` exactly (case-insensitive), or — only when nothing
/// matches exactly — a singular guessed as the plural minus `s`/`es`.
fn matching_resources(
    resources: impl IntoIterator<Item = (ApiResource, ApiCapabilities)>,
    kind: &str,
) -> Vec<(ApiResource, ApiCapabilities)> {
    let wanted = kind.to_ascii_lowercase();
    let mut exact: Vec<(ApiResource, ApiCapabilities)> = Vec::new();
    let mut guessed: Vec<(ApiResource, ApiCapabilities)> = Vec::new();
    for (resource, caps) in resources {
        let list = if resource.kind.to_ascii_lowercase() == wanted
            || resource.plural == wanted
            || resource_label(&resource) == wanted
        {
            &mut exact
        } else if [format!("{wanted}s"), format!("{wanted}es")].contains(&resource.plural) {
            &mut guessed
        } else {
            continue;
        };
        if !list
            .iter()
            .any(|(r, _)| r.api_version == resource.api_version && r.kind == resource.kind)
        {
            list.push((resource, caps));
        }
    }
    if exact.is_empty() {
        guessed
    } else {
        exact
    }
}

/// The one resource of `found`, or why not: none, or several (a delete
/// never picks one of several by order).
fn single_resource(
    mut found: Vec<(ApiResource, ApiCapabilities)>,
    kind: &str,
    context: &str,
) -> Result<(ApiResource, ApiCapabilities)> {
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(CoreError::NotFound(format!(
            "the cluster has no kind {kind} (context {context})"
        ))),
        _ => Err(CoreError::Other(format!(
            "{kind} names several resources ({}); name the kind exactly",
            found
                .iter()
                .map(|(resource, _)| format!("{} {}", resource.api_version, resource.kind))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

/// Resource name for messages: `deployments`, `deployments.apps`.
fn resource_label(resource: &ApiResource) -> String {
    if resource.group.is_empty() {
        resource.plural.clone()
    } else {
        format!("{}.{}", resource.plural, resource.group)
    }
}

// ---------------------------------------------------------------------------
// KubeClusterApi
// ---------------------------------------------------------------------------

impl KubeClusterApi {
    fn write_error(
        &self,
        error: kube::Error,
        verb: &str,
        resource: &str,
        namespace: Option<&str>,
    ) -> CoreError {
        write_error(self.target(), error, verb, resource, namespace)
    }

    /// Resolve `kind` (and optional `apiVersion`) to a resource and scope.
    /// With an `apiVersion` the kind may also be a plural or singular
    /// (`deployments`); without one the core group is asked first, so an
    /// unrelated broken API group cannot block `Pod` or `Secret`.
    async fn resolve_kind(&self, kind: &ObjectKind) -> Result<(ApiResource, ApiCapabilities)> {
        if let Some(api_version) = kind.api_version.as_deref() {
            let gvk = gvk_of(api_version, &kind.kind);
            return match resolve_gvk(self.client(), self.target(), &gvk).await {
                Err(CoreError::NotFound(message)) => {
                    let discovery = Discovery::new(self.client().clone())
                        .filter(&[gvk.group.as_str()])
                        .run()
                        .await
                        .map_err(|error| {
                            api_error(self.target(), error, "discover", api_version, None)
                        })?;
                    let resources = discovery
                        .get(&gvk.group)
                        .map(|group| group.versioned_resources(&gvk.version))
                        .unwrap_or_default();
                    match matching_resources(resources, &kind.kind) {
                        found if found.is_empty() => Err(CoreError::NotFound(message)),
                        found => single_resource(found, &kind.kind, &self.target().context),
                    }
                }
                other => other,
            };
        }
        let core = self
            .discover_matching(Discovery::new(self.client().clone()).filter(&[""]), kind)
            .await?;
        if !core.is_empty() {
            return single_resource(core, &kind.kind, &self.target().context);
        }
        let mut found = self
            .discover_matching(Discovery::new(self.client().clone()).exclude(&[""]), kind)
            .await?;
        match found.len() {
            1 => Ok(found.remove(0)),
            0 => Err(CoreError::NotFound(format!(
                "the cluster has no kind {} (context {})",
                kind.kind,
                self.target().context
            ))),
            _ => Err(CoreError::Other(format!(
                "kind {} exists in several API groups ({}); name the apiVersion",
                kind.kind,
                found
                    .iter()
                    .map(|(resource, _)| resource.api_version.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Resources of the groups `discovery` covers that `kind` names.
    async fn discover_matching(
        &self,
        discovery: Discovery,
        kind: &ObjectKind,
    ) -> Result<Vec<(ApiResource, ApiCapabilities)>> {
        let discovery = discovery
            .run()
            .await
            .map_err(|error| api_error(self.target(), error, "discover", "API resources", None))?;
        Ok(matching_resources(
            discovery
                .groups()
                .flat_map(|group| group.recommended_resources()),
            &kind.kind,
        ))
    }

    fn dynamic_api(
        &self,
        resource: &ApiResource,
        caps: &ApiCapabilities,
        namespace: Option<&str>,
    ) -> Result<Api<DynamicObject>> {
        let client = self.client().clone();
        match (&caps.scope, namespace) {
            (Scope::Namespaced, Some(namespace)) => {
                Ok(Api::namespaced_with(client, namespace, resource))
            }
            (Scope::Namespaced, None) => Err(CoreError::Other(format!(
                "{} is namespaced: name the namespace",
                resource.kind
            ))),
            (Scope::Cluster, _) => Ok(Api::all_with(client, resource)),
        }
    }

    async fn apply_one(
        &self,
        document: &Value,
        default_namespace: Option<&str>,
        dry_run: bool,
    ) -> Result<AppliedObject> {
        let api_version = document["apiVersion"].as_str().unwrap_or_default();
        let kind = document["kind"].as_str().unwrap_or_default();
        let name = document["metadata"]["name"].as_str().unwrap_or_default();
        let (resource, caps) =
            resolve_gvk(self.client(), self.target(), &gvk_of(api_version, kind)).await?;
        let label = resource_label(&resource);
        if let Some(value) = document
            .pointer("/metadata/namespace")
            .filter(|value| !value.is_null() && !value.is_string())
        {
            return Err(CoreError::Other(format!(
                "{kind} {name}: metadata.namespace must be a string, not {value}"
            )));
        }
        let namespace = match caps.scope {
            Scope::Namespaced => Some(
                document
                    .pointer("/metadata/namespace")
                    .and_then(Value::as_str)
                    .filter(|ns| !ns.is_empty())
                    .or(default_namespace)
                    .ok_or_else(|| {
                        CoreError::Other(format!(
                            "{kind} {name} is namespaced: set metadata.namespace or a default namespace"
                        ))
                    })?
                    .to_string(),
            ),
            Scope::Cluster => None,
        };
        let mut document = document.clone();
        if let (Some(namespace), Some(Value::Object(metadata))) =
            (namespace.as_deref(), document.get_mut("metadata"))
        {
            metadata.insert("namespace".into(), Value::String(namespace.into()));
        }
        let api = self.dynamic_api(&resource, &caps, namespace.as_deref())?;
        let live = api
            .get_opt(name)
            .await
            .map_err(|error| self.write_error(error, "get", &label, namespace.as_deref()))?
            .map(|object| to_value(&object))
            .transpose()?;
        let params = PatchParams {
            dry_run,
            force: false,
            field_manager: Some(FIELD_MANAGER.into()),
            field_validation: Some(ValidationDirective::Strict),
        };
        let result = api
            .patch(name, &params, &Patch::Apply(&document))
            .await
            .map_err(|error| self.write_error(error, "apply", &label, namespace.as_deref()))?;
        let mut result = to_value(&result)?;
        let mut live = live;
        let status_is_content = status_is_content(&caps);
        let action = apply_action(live.as_ref(), &result, status_is_content);
        let secret_keys = if kind == "Secret" && resource.group.is_empty() {
            secret_key_changes(live.as_ref(), &result)
        } else {
            Vec::new()
        };
        hide_secret_values(&mut result);
        if let Some(live) = live.as_mut() {
            hide_secret_values(live);
        }
        Ok(AppliedObject {
            object: ObjectRef {
                api_version: api_version.into(),
                kind: kind.into(),
                namespace,
                name: name.into(),
            },
            action,
            live,
            result,
            secret_keys,
            status_is_content,
        })
    }

    async fn list_names(
        &self,
        api: &Api<DynamicObject>,
        labels: Option<&str>,
        fields: Option<&str>,
        label: &str,
        namespace: Option<&str>,
    ) -> Result<Vec<String>> {
        let mut names = Vec::new();
        let mut params = ListParams::default().limit(500);
        if let Some(labels) = labels.filter(|s| !s.is_empty()) {
            params = params.labels(labels);
        }
        if let Some(fields) = fields.filter(|s| !s.is_empty()) {
            params = params.fields(fields);
        }
        loop {
            let page = api
                .list(&params)
                .await
                .map_err(|error| self.write_error(error, "list", label, namespace))?;
            names.extend(
                page.items
                    .into_iter()
                    .filter_map(|object| object.metadata.name),
            );
            if names.len() > MAX_DELETE_OBJECTS {
                return Err(CoreError::Other(format!(
                    "the selector matches more than {MAX_DELETE_OBJECTS} {label}; narrow it"
                )));
            }
            match page.metadata.continue_.filter(|token| !token.is_empty()) {
                Some(token) => params = params.continue_token(&token),
                None => return Ok(names),
            }
        }
    }
}

async fn resolve_gvk(
    client: &Client,
    target: &KubeCluster,
    gvk: &GroupVersionKind,
) -> Result<(ApiResource, ApiCapabilities)> {
    kube::discovery::pinned_kind(client, gvk)
        .await
        .map_err(|error| match error {
            kube::Error::Discovery(_) => CoreError::NotFound(format!(
                "the cluster has no kind {} in {} (context {})",
                gvk.kind,
                gvk.api_version(),
                target.context
            )),
            kube::Error::Api(status) if status.code == 404 => CoreError::NotFound(format!(
                "the cluster has no API {} for kind {} (context {})",
                gvk.api_version(),
                gvk.kind,
                target.context
            )),
            other => api_error(target, other, "discover", &gvk.api_version(), None),
        })
}

#[async_trait::async_trait]
impl ClusterWriteApi for KubeClusterApi {
    async fn apply(
        &self,
        manifest: &str,
        default_namespace: Option<&str>,
        dry_run: bool,
    ) -> Result<Vec<AppliedObject>> {
        let documents = parse_manifest(manifest)?;
        let total = documents.len();
        let mut applied = Vec::with_capacity(total);
        for document in &documents {
            match self.apply_one(document, default_namespace, dry_run).await {
                Ok(object) => applied.push(object),
                Err(error) => {
                    let error = mask_secret_values(error, &submitted_secret_values(document));
                    let name = format!(
                        "{} {}",
                        document["kind"].as_str().unwrap_or_default(),
                        document["metadata"]["name"].as_str().unwrap_or_default()
                    );
                    let done = applied.len();
                    let note = if done == 0 || dry_run {
                        String::new()
                    } else {
                        format!(" ({done} of {total} objects before it were applied)")
                    };
                    return Err(match error {
                        CoreError::AccessDenied(m) => {
                            CoreError::AccessDenied(format!("{name}: {m}{note}"))
                        }
                        CoreError::NotFound(m) => CoreError::NotFound(format!("{name}: {m}{note}")),
                        CoreError::Unavailable(m) => {
                            CoreError::Unavailable(format!("{name}: {m}{note}"))
                        }
                        other => CoreError::Other(format!("{name}: {other}{note}")),
                    });
                }
            }
        }
        Ok(applied)
    }

    async fn scale(
        &self,
        kind: ScaleKind,
        namespace: &str,
        name: &str,
        replicas: i32,
        dry_run: bool,
    ) -> Result<ScaleOutcome> {
        if replicas < 0 {
            return Err(CoreError::Other("replicas must be 0 or more".into()));
        }
        let (resource, caps) = resolve_gvk(
            self.client(),
            self.target(),
            &GroupVersionKind::gvk("apps", "v1", kind.kind()),
        )
        .await?;
        let label = format!("{}/scale", resource_label(&resource));
        let api = self.dynamic_api(&resource, &caps, Some(namespace))?;
        let before = api
            .get_scale(name)
            .await
            .map_err(|error| self.write_error(error, "get", &label, Some(namespace)))?;
        let params = PatchParams {
            dry_run,
            field_manager: Some(FIELD_MANAGER.into()),
            ..PatchParams::default()
        };
        let after = api
            .patch_scale(
                name,
                &params,
                &Patch::Merge(json!({"spec": {"replicas": replicas}})),
            )
            .await
            .map_err(|error| self.write_error(error, "patch", &label, Some(namespace)))?;
        let replicas_of = |scale: &k8s_openapi::api::autoscaling::v1::Scale| {
            scale
                .spec
                .as_ref()
                .and_then(|spec| spec.replicas)
                .unwrap_or(0)
        };
        Ok(ScaleOutcome {
            object: ObjectRef {
                api_version: "apps/v1".into(),
                kind: kind.kind().into(),
                namespace: Some(namespace.into()),
                name: name.into(),
            },
            from: replicas_of(&before),
            to: replicas_of(&after),
        })
    }

    async fn rollout_restart(
        &self,
        kind: RestartKind,
        namespace: &str,
        name: &str,
        dry_run: bool,
    ) -> Result<WriteOutcome> {
        let (resource, caps) = resolve_gvk(
            self.client(),
            self.target(),
            &GroupVersionKind::gvk("apps", "v1", kind.kind()),
        )
        .await?;
        let label = resource_label(&resource);
        let api = self.dynamic_api(&resource, &caps, Some(namespace))?;
        let live = api
            .get(name)
            .await
            .map_err(|error| self.write_error(error, "get", &label, Some(namespace)))?;
        if live.data.pointer("/spec/paused").and_then(Value::as_bool) == Some(true) {
            return Err(paused_error(kind.kind(), name, "restart"));
        }
        let params = PatchParams {
            dry_run,
            field_manager: Some(FIELD_MANAGER.into()),
            ..PatchParams::default()
        };
        let patch = json!({"spec": {"template": {"metadata": {"annotations": {
            RESTARTED_AT_ANNOTATION: now_rfc3339()
        }}}}});
        let result = api
            .patch(name, &params, &Patch::Merge(&patch))
            .await
            .map_err(|error| self.write_error(error, "patch", &label, Some(namespace)))?;
        Ok(WriteOutcome {
            object: ObjectRef {
                api_version: "apps/v1".into(),
                kind: kind.kind().into(),
                namespace: Some(namespace.into()),
                name: name.into(),
            },
            live: to_value(&live)?,
            result: to_value(&result)?,
        })
    }

    async fn rollout_undo(
        &self,
        namespace: &str,
        name: &str,
        to_revision: Option<i64>,
        dry_run: bool,
    ) -> Result<UndoOutcome> {
        let client = self.client().clone();
        let deployments = Api::<Deployment>::namespaced(client.clone(), namespace);
        let deployment = deployments
            .get(name)
            .await
            .map_err(|error| self.write_error(error, "get", "deployments", Some(namespace)))?;
        let revision_of = |annotations: Option<&BTreeMap<String, String>>| {
            annotations
                .and_then(|a| a.get(REVISION_ANNOTATION))
                .and_then(|r| r.parse::<i64>().ok())
        };
        if deployment.spec.as_ref().and_then(|spec| spec.paused) == Some(true) {
            return Err(paused_error("Deployment", name, "roll back"));
        }
        let current = revision_of(deployment.metadata.annotations.as_ref()).unwrap_or(0);
        let uid = deployment.metadata.uid.clone();
        let replicasets_api = Api::<ReplicaSet>::namespaced(client, namespace);
        let mut replicasets = Vec::new();
        let mut params = ListParams::default().limit(super::DEFAULT_CLUSTER_LIST_LIMIT);
        loop {
            let page = replicasets_api
                .list(&params)
                .await
                .map_err(|error| self.write_error(error, "list", "replicasets", Some(namespace)))?;
            replicasets.extend(page.items);
            match page.metadata.continue_.filter(|token| !token.is_empty()) {
                Some(token) => params = params.continue_token(&token),
                None => break,
            }
        }
        let mut owned: Vec<(i64, ReplicaSet)> = replicasets
            .into_iter()
            .filter(|rs| {
                uid.is_some()
                    && rs.metadata.owner_references.as_ref().is_some_and(|owners| {
                        owners
                            .iter()
                            .any(|o| o.kind == "Deployment" && Some(&o.uid) == uid.as_ref())
                    })
            })
            .filter_map(|rs| revision_of(rs.metadata.annotations.as_ref()).map(|r| (r, rs)))
            .collect();
        owned.sort_by_key(|(revision, _)| *revision);
        let target = match to_revision {
            Some(wanted) => owned.into_iter().find(|(revision, _)| *revision == wanted),
            None => owned
                .into_iter()
                .rev()
                .find(|(revision, _)| *revision < current),
        };
        let Some((target_revision, replicaset)) = target else {
            return Err(CoreError::NotFound(match to_revision {
                Some(r) => format!("deployment {name} has no revision {r}"),
                None => {
                    format!("deployment {name} has no revision before {current} to roll back to")
                }
            }));
        };
        if target_revision == current {
            return Err(CoreError::Other(format!(
                "deployment {name} is already at revision {current}"
            )));
        }
        let mut template = replicaset
            .spec
            .and_then(|spec| spec.template)
            .ok_or_else(|| {
                CoreError::Other(format!("revision {target_revision} has no pod template"))
            })?;
        if let Some(labels) = template
            .metadata
            .as_mut()
            .and_then(|metadata| metadata.labels.as_mut())
        {
            labels.remove("pod-template-hash");
        }
        let mut updated = deployment.clone();
        if let Some(spec) = updated.spec.as_mut() {
            spec.template = template;
        }
        let params = PostParams {
            dry_run,
            field_manager: Some(FIELD_MANAGER.into()),
        };
        let result = deployments
            .replace(name, &params, &updated)
            .await
            .map_err(|error| self.write_error(error, "update", "deployments", Some(namespace)))?;
        Ok(UndoOutcome {
            write: WriteOutcome {
                object: ObjectRef {
                    api_version: "apps/v1".into(),
                    kind: "Deployment".into(),
                    namespace: Some(namespace.into()),
                    name: name.into(),
                },
                live: to_value(&deployment)?,
                result: to_value(&result)?,
            },
            from_revision: current,
            to_revision: target_revision,
        })
    }

    async fn delete(
        &self,
        kind: &ObjectKind,
        namespace: Option<&str>,
        target: &DeleteTarget,
        options: &DeleteOptions,
        dry_run: bool,
    ) -> Result<Vec<DeletedObject>> {
        let (resource, caps) = self.resolve_kind(kind).await?;
        let namespace = match caps.scope {
            Scope::Namespaced => namespace,
            Scope::Cluster => None,
        };
        let label = resource_label(&resource);
        let api = self.dynamic_api(&resource, &caps, namespace)?;
        let (names, missing_is_error) = match target {
            DeleteTarget::Name(name) => (vec![PinnedName::unpinned(name.clone())], true),
            DeleteTarget::Names(names) => (names.clone(), false),
            DeleteTarget::Selector { labels, fields } => (
                self.list_names(
                    &api,
                    labels.as_deref(),
                    fields.as_deref(),
                    &label,
                    namespace,
                )
                .await?
                .into_iter()
                .map(PinnedName::unpinned)
                .collect(),
                false,
            ),
        };
        let base = DeleteParams {
            dry_run,
            grace_period_seconds: if options.force {
                Some(0)
            } else {
                options.grace_period_seconds
            },
            propagation_policy: Some(match options.propagation {
                Propagation::Background => PropagationPolicy::Background,
                Propagation::Foreground => PropagationPolicy::Foreground,
                Propagation::Orphan => PropagationPolicy::Orphan,
            }),
            preconditions: None,
        };
        let mut deleted = Vec::with_capacity(names.len());
        for PinnedName { name, uid } in names {
            // Only the object the window showed (#659): a UID precondition
            // makes the API server refuse a same-named successor.
            let params = DeleteParams {
                preconditions: uid.clone().map(|uid| Preconditions {
                    uid: Some(uid),
                    resource_version: None,
                }),
                ..base.clone()
            };
            let object = ObjectRef {
                api_version: resource.api_version.clone(),
                kind: resource.kind.clone(),
                namespace: namespace.map(str::to_owned),
                name: name.clone(),
            };
            let mut live = None;
            let outcome = match api.delete(&name, &params).await {
                Ok(answer) => {
                    let object = answer.left();
                    let pending = object
                        .as_ref()
                        .map(|o| o.metadata.deletion_timestamp.is_some());
                    let object = match object {
                        // A dry run answering a bare `Status`: read the
                        // object, so the window can still tell what it is.
                        // Unreadable (no `get` right): the window then gives
                        // the conditional warnings of an unknown object.
                        None if dry_run => match api.get_opt(&name).await {
                            Ok(object) => object,
                            Err(error) => {
                                tracing::warn!(%error, "dry-run delete of {name}: object unreadable, impact warnings are conditional");
                                None
                            }
                        },
                        object => object,
                    };
                    if let Some(object) = &object {
                        let mut value = to_value(object)?;
                        hide_secret_values(&mut value);
                        live = Some(value);
                    }
                    match pending {
                        _ if dry_run => DeleteOutcome::WouldDelete,
                        Some(true) => DeleteOutcome::Pending,
                        Some(false) => DeleteOutcome::Deleted,
                        // A bare `Status` (aggregated APIs, async deletes)
                        // says the request was accepted, not that the
                        // object is gone.
                        None => match api.get_opt(&name).await {
                            Ok(Some(_)) => DeleteOutcome::Pending,
                            Ok(None) => DeleteOutcome::Deleted,
                            Err(error) => {
                                tracing::debug!(%error, "post-delete check of {name} failed");
                                DeleteOutcome::Pending
                            }
                        },
                    }
                }
                Err(kube::Error::Api(status)) if status.code == 404 && !missing_is_error => {
                    DeleteOutcome::AlreadyGone
                }
                Err(kube::Error::Api(status)) if uid.is_some() && is_uid_conflict(&status) => {
                    DeleteOutcome::Recreated
                }
                Err(error) => {
                    let error = self.write_error(error, "delete", &label, namespace);
                    let done = deleted.len();
                    return Err(if done == 0 || dry_run {
                        error
                    } else {
                        CoreError::Other(format!(
                            "{error} ({done} objects before {name} were already deleted)"
                        ))
                    });
                }
            };
            deleted.push(DeletedObject {
                object,
                outcome,
                live,
            });
        }
        Ok(deleted)
    }

    async fn deletion_impact(&self, object: &DeletedObject) -> DeletionImpact {
        impact_of(self, object).await
    }
}

impl KubeClusterApi {
    /// Count the objects of `resource` in `namespace` (or everywhere), one
    /// page at most: `(count, more)`.
    async fn count_objects(
        &self,
        resource: &ApiResource,
        caps: &ApiCapabilities,
        namespace: Option<&str>,
        params: &ListParams,
    ) -> Option<(usize, bool)> {
        let client = self.client().clone();
        let api: Api<DynamicObject> = match (&caps.scope, namespace) {
            (Scope::Namespaced, Some(namespace)) => {
                Api::namespaced_with(client, namespace, resource)
            }
            _ => Api::all_with(client, resource),
        };
        let page = api.list(&params.clone().limit(500)).await.ok()?;
        let more = page
            .metadata
            .continue_
            .as_deref()
            .is_some_and(|token| !token.is_empty());
        Some((page.items.len(), more))
    }

    /// Objects of a namespace by kind, and whether some kinds could not be
    /// counted.
    async fn namespace_contents(&self, namespace: &str) -> (Vec<(String, usize, bool)>, bool) {
        let Ok(discovery) = Discovery::new(self.client().clone()).run().await else {
            return (Vec::new(), true);
        };
        let mut incomplete = false;
        let mut contents = Vec::new();
        for group in discovery.groups() {
            for (resource, caps) in group.recommended_resources() {
                let listable = caps.operations.iter().any(|verb| verb == "list");
                if caps.scope != Scope::Namespaced || !listable || resource.kind == "Event" {
                    continue;
                }
                match self
                    .count_objects(&resource, &caps, Some(namespace), &ListParams::default())
                    .await
                {
                    Some((count, more)) if count > 0 => {
                        contents.push((resource.kind.clone(), count, more));
                    }
                    Some(_) => {}
                    None => incomplete = true,
                }
            }
        }
        contents.sort();
        (contents, incomplete)
    }

    async fn reclaim_policy(&self, kind: &str, live: &Value) -> Option<String> {
        let client = self.client().clone();
        let field = |object: &Value, pointer: &str| {
            object
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::to_owned)
        };
        if kind == "PersistentVolume" {
            return field(live, "/spec/persistentVolumeReclaimPolicy");
        }
        if let Some(volume) = field(live, "/spec/volumeName") {
            let pv = ApiResource::from_gvk_with_plural(
                &GroupVersionKind::gvk("", "v1", "PersistentVolume"),
                "persistentvolumes",
            );
            let object = Api::<DynamicObject>::all_with(client.clone(), &pv)
                .get(&volume)
                .await
                .ok()?;
            return field(
                &to_value(&object).ok()?,
                "/spec/persistentVolumeReclaimPolicy",
            );
        }
        let class = field(live, "/spec/storageClassName")?;
        let sc = ApiResource::from_gvk_with_plural(
            &GroupVersionKind::gvk("storage.k8s.io", "v1", "StorageClass"),
            "storageclasses",
        );
        let object = Api::<DynamicObject>::all_with(client, &sc)
            .get(&class)
            .await
            .ok()?;
        // A StorageClass without reclaimPolicy deletes.
        Some(field(&to_value(&object).ok()?, "/reclaimPolicy").unwrap_or_else(|| "Delete".into()))
    }
}

/// Fill a [`DeletionImpact`] for one object of a dry-run delete.
async fn impact_of(api: &KubeClusterApi, object: &DeletedObject) -> DeletionImpact {
    let mut impact = DeletionImpact::default();
    let Some(live) = object.live.as_ref() else {
        return impact;
    };
    impact.controller = live
        .pointer("/metadata/ownerReferences")
        .and_then(Value::as_array)
        .and_then(|owners| {
            owners
                .iter()
                .find(|owner| owner.get("controller").and_then(Value::as_bool) == Some(true))
        })
        .map(|owner| {
            format!(
                "{} {}",
                owner["kind"].as_str().unwrap_or_default(),
                owner["name"].as_str().unwrap_or_default()
            )
        });
    match object.object.kind.as_str() {
        "Namespace" => {
            (impact.contents, impact.contents_incomplete) =
                api.namespace_contents(&object.object.name).await;
        }
        "CustomResourceDefinition" => {
            let group = live.pointer("/spec/group").and_then(Value::as_str);
            let plural = live.pointer("/spec/names/plural").and_then(Value::as_str);
            let kind = live.pointer("/spec/names/kind").and_then(Value::as_str);
            let version = live
                .pointer("/spec/versions")
                .and_then(Value::as_array)
                .and_then(|versions| {
                    versions
                        .iter()
                        .find(|v| v.get("storage").and_then(Value::as_bool) == Some(true))
                })
                .and_then(|v| v.get("name"))
                .and_then(Value::as_str);
            if let (Some(group), Some(plural), Some(kind), Some(version)) =
                (group, plural, kind, version)
            {
                let resource = ApiResource::from_gvk_with_plural(
                    &GroupVersionKind::gvk(group, version, kind),
                    plural,
                );
                let caps = ApiCapabilities {
                    scope: Scope::Cluster,
                    subresources: Vec::new(),
                    operations: Vec::new(),
                };
                impact.custom_objects = api
                    .count_objects(&resource, &caps, None, &ListParams::default())
                    .await;
            }
        }
        "PersistentVolumeClaim" | "PersistentVolume" => {
            impact.reclaim_policy = api.reclaim_policy(&object.object.kind, live).await;
        }
        "Node" => {
            let pods =
                ApiResource::from_gvk_with_plural(&GroupVersionKind::gvk("", "v1", "Pod"), "pods");
            let caps = ApiCapabilities {
                scope: Scope::Cluster,
                subresources: Vec::new(),
                operations: Vec::new(),
            };
            let params =
                ListParams::default().fields(&format!("spec.nodeName={}", object.object.name));
            impact.node_pods = api.count_objects(&pods, &caps, None, &params).await;
        }
        _ => {}
    }
    impact
}

#[cfg(test)]
#[path = "write_tests.rs"]
mod tests;

/// A 409 refusing a delete because its UID precondition no longer holds:
/// the name now belongs to another object (#659).
fn is_uid_conflict(status: &kube::core::Status) -> bool {
    status.code == 409 && status.message.to_ascii_lowercase().contains("precondition")
}
