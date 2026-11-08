//! `ClusterWriteApi` against an in-process fake API server (#638): the
//! requests each method sends (method, path, query, content type, body) and
//! what it makes of the answers.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;

use super::*;

/// One request the fake API server received.
#[derive(Debug, Clone)]
struct Request {
    method: String,
    /// Path without the query.
    path: String,
    query: BTreeMap<String, String>,
    content_type: Option<String>,
    body: Value,
}

type Router = dyn Fn(&Request, &mut State) -> (u16, Value) + Send + Sync;

/// Objects of the fake cluster, by `path` of the object.
#[derive(Default)]
struct State {
    objects: BTreeMap<String, Value>,
}

struct FakeApi {
    requests: Arc<Mutex<Vec<Request>>>,
    state: Arc<Mutex<State>>,
    url: String,
}

impl FakeApi {
    async fn start(router: Arc<Router>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<Request>>> = Arc::default();
        let state: Arc<Mutex<State>> = Arc::default();
        let (log, shared) = (requests.clone(), state.clone());
        tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let (log, shared, router) = (log.clone(), shared.clone(), router.clone());
                tokio::spawn(async move {
                    let (read, mut write) = stream.into_split();
                    let mut reader = BufReader::new(read);
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                            return;
                        }
                        let mut parts = line.split_whitespace();
                        let method = parts.next().unwrap_or_default().to_string();
                        let target = parts.next().unwrap_or_default().to_string();
                        let mut headers = BTreeMap::new();
                        loop {
                            let mut header = String::new();
                            reader.read_line(&mut header).await.unwrap();
                            let header = header.trim_end();
                            if header.is_empty() {
                                break;
                            }
                            if let Some((name, value)) = header.split_once(':') {
                                headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
                            }
                        }
                        let length: usize = headers
                            .get("content-length")
                            .and_then(|l| l.parse().ok())
                            .unwrap_or(0);
                        let mut body = vec![0; length];
                        reader.read_exact(&mut body).await.unwrap();
                        let (path, query) = target.split_once('?').unwrap_or((&target, ""));
                        let request = Request {
                            method,
                            path: path.to_string(),
                            query: query
                                .split('&')
                                .filter(|p| !p.is_empty())
                                .map(|p| {
                                    let (k, v) = p.split_once('=').unwrap_or((p, ""));
                                    (k.to_string(), v.replace("%3D", "=").replace("%2C", ","))
                                })
                                .collect(),
                            content_type: headers.get("content-type").cloned(),
                            body: serde_json::from_slice(&body).unwrap_or(Value::Null),
                        };
                        log.lock().unwrap().push(request.clone());
                        let (code, answer) = {
                            let mut state = shared.lock().unwrap();
                            router(&request, &mut state)
                        };
                        let answer = answer.to_string();
                        let response = format!(
                            "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{answer}",
                            answer.len()
                        );
                        if write.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                });
            }
        });
        Self {
            requests,
            state,
            url,
        }
    }

    fn api(&self) -> KubeClusterApi {
        let config = kube::Config::new(self.url.parse().unwrap());
        let client = kube::Client::try_from(config).unwrap();
        KubeClusterApi::with_client(client, target())
    }

    /// Requests other than discovery.
    fn writes(&self) -> Vec<Request> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| discovery(&r.path).is_none())
            .cloned()
            .collect()
    }

    fn put(&self, path: &str, object: Value) {
        self.state
            .lock()
            .unwrap()
            .objects
            .insert(path.into(), object);
    }

    fn get(&self, path: &str) -> Option<Value> {
        self.state.lock().unwrap().objects.get(path).cloned()
    }
}

fn target() -> KubeCluster {
    KubeCluster {
        name: "prod-eu".into(),
        context: "prod-eu-admin".into(),
        ..KubeCluster::default()
    }
}

fn status(code: u16, reason: &str, message: &str) -> (u16, Value) {
    (
        code,
        json!({"kind": "Status", "apiVersion": "v1", "metadata": {}, "status": "Failure",
               "message": message, "reason": reason, "code": code}),
    )
}

/// Discovery documents for core v1 (pods, secrets, configmaps, namespaces,
/// persistentvolumes) and apps/v1 (deployments, statefulsets, replicasets,
/// daemonsets), with `scale` subresources.
fn discovery(path: &str) -> Option<Value> {
    let resource = |name: &str, kind: &str, namespaced: bool| {
        json!({"name": name, "singularName": "", "namespaced": namespaced, "kind": kind,
               "verbs": ["create", "delete", "get", "list", "patch", "update"]})
    };
    let scale = |name: &str| {
        json!({"name": format!("{name}/scale"), "singularName": "", "namespaced": true,
               "group": "autoscaling", "version": "v1", "kind": "Scale", "verbs": ["get", "patch", "update"]})
    };
    match path {
        "/api" => Some(
            json!({"kind": "APIVersions", "versions": ["v1"], "serverAddressByClientCIDRs": []}),
        ),
        "/apis" => Some(
            json!({"kind": "APIGroupList", "apiVersion": "v1", "groups": [
            {"name": "apps", "versions": [{"groupVersion": "apps/v1", "version": "v1"}],
             "preferredVersion": {"groupVersion": "apps/v1", "version": "v1"}},
            {"name": "example.com", "versions": [{"groupVersion": "example.com/v1", "version": "v1"}],
             "preferredVersion": {"groupVersion": "example.com/v1", "version": "v1"}}]}),
        ),
        "/api/v1" => Some(
            json!({"kind": "APIResourceList", "groupVersion": "v1", "resources": [
            resource("pods", "Pod", true), resource("secrets", "Secret", true),
            resource("configmaps", "ConfigMap", true), resource("namespaces", "Namespace", false),
            resource("persistentvolumes", "PersistentVolume", false)]}),
        ),
        "/apis/apps/v1" => Some(
            json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "apps/v1", "resources": [
            resource("deployments", "Deployment", true), scale("deployments"),
            resource("statefulsets", "StatefulSet", true), scale("statefulsets"),
            resource("replicasets", "ReplicaSet", true), scale("replicasets"),
            resource("daemonsets", "DaemonSet", true)]}),
        ),
        // Custom resources: `widgetses` is listed before `widgets` (so a
        // guessed singular could pick it), only `gadgets` has `/status`.
        "/apis/example.com/v1" => Some(
            json!({"kind": "APIResourceList", "apiVersion": "v1", "groupVersion": "example.com/v1", "resources": [
            resource("widgetses", "Widgetse", true), resource("widgets", "Widget", true),
            resource("gadgets", "Gadget", true),
            {"name": "gadgets/status", "singularName": "", "namespaced": true, "kind": "Gadget",
             "verbs": ["get", "patch", "update"]}]}),
        ),
        _ => None,
    }
}

/// A small API server: discovery, GET/PATCH(apply, merge)/PUT/DELETE/LIST of
/// stored objects. Dry runs answer without storing.
fn cluster(request: &Request, state: &mut State) -> (u16, Value) {
    if let Some(document) = discovery(&request.path) {
        return (200, document);
    }
    let dry_run = is_dry_run(request);
    let path = request.path.clone();
    match request.method.as_str() {
        "GET" if path.ends_with("/scale") => {
            let owner = path.trim_end_matches("/scale");
            match state.objects.get(owner) {
                Some(object) => (
                    200,
                    json!({"apiVersion": "autoscaling/v1", "kind": "Scale",
                    "metadata": {"name": object["metadata"]["name"]},
                    "spec": {"replicas": object["spec"]["replicas"]}}),
                ),
                None => status(404, "NotFound", "not found"),
            }
        }
        "PATCH" if path.ends_with("/scale") => {
            let owner = path.trim_end_matches("/scale").to_string();
            let Some(mut object) = state.objects.get(&owner).cloned() else {
                return status(404, "NotFound", "not found");
            };
            object["spec"]["replicas"] = request.body["spec"]["replicas"].clone();
            if !dry_run {
                state.objects.insert(owner, object.clone());
            }
            (
                200,
                json!({"apiVersion": "autoscaling/v1", "kind": "Scale",
                "metadata": {"name": object["metadata"]["name"]},
                "spec": {"replicas": object["spec"]["replicas"]}}),
            )
        }
        "GET" => match state.objects.get(&path) {
            Some(object) => (200, object.clone()),
            None if is_collection(&path) => {
                // A collection: list by prefix, filtered by the label selector.
                let selector = request
                    .query
                    .get("labelSelector")
                    .cloned()
                    .unwrap_or_default();
                let items: Vec<Value> = state
                    .objects
                    .iter()
                    .filter(|(p, _)| p.rsplit_once('/').map(|(c, _)| c) == Some(path.as_str()))
                    .filter(|(_, o)| {
                        selector.is_empty()
                            || selector.split(',').all(|kv| {
                                let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
                                o["metadata"]["labels"][k].as_str() == Some(v)
                            })
                    })
                    .map(|(_, o)| o.clone())
                    .collect();
                (
                    200,
                    json!({"kind": "List", "apiVersion": "v1", "metadata": {"resourceVersion": "1"}, "items": items}),
                )
            }
            None => status(404, "NotFound", "not found"),
        },
        "PATCH" => {
            let mut object = state.objects.get(&path).cloned();
            let content_type = request.content_type.clone().unwrap_or_default();
            let updated = if content_type.contains("apply-patch") {
                let mut applied = request.body.clone();
                applied["metadata"]["resourceVersion"] =
                    json!(if object.is_some() { "2" } else { "1" });
                if let Some(live) = &object {
                    applied["metadata"]["uid"] = live["metadata"]["uid"].clone();
                }
                if applied["kind"] == "Secret" {
                    // The server turns stringData into data.
                    if let Some(Value::Object(string_data)) = applied.get("stringData").cloned() {
                        for (key, value) in string_data {
                            applied["data"][key] =
                                json!(format!("b64:{}", value.as_str().unwrap_or_default()));
                        }
                        applied.as_object_mut().unwrap().remove("stringData");
                    }
                }
                applied
            } else {
                let Some(live) = object.as_mut() else {
                    return status(404, "NotFound", "not found");
                };
                merge(live, &request.body);
                live.clone()
            };
            if !dry_run {
                state.objects.insert(path, updated.clone());
            }
            (200, updated)
        }
        "PUT" => {
            if !state.objects.contains_key(&path) {
                return status(404, "NotFound", "not found");
            }
            if !dry_run {
                state.objects.insert(path, request.body.clone());
            }
            (200, request.body.clone())
        }
        "DELETE" => match state.objects.get(&path).cloned() {
            Some(object) => {
                if !dry_run {
                    state.objects.remove(&path);
                }
                (200, object)
            }
            None => status(404, "NotFound", "not found"),
        },
        _ => status(405, "MethodNotAllowed", "method not allowed"),
    }
}

/// A collection path ends with a resource plural, not an object name.
fn is_collection(path: &str) -> bool {
    let last = path.rsplit('/').next().unwrap_or_default();
    [
        "pods",
        "secrets",
        "configmaps",
        "namespaces",
        "persistentvolumes",
        "deployments",
        "statefulsets",
        "replicasets",
        "daemonsets",
        "widgets",
        "widgetses",
        "gadgets",
    ]
    .contains(&last)
}

/// `dryRun=All` in the query (patch, put) or in the body (delete options).
fn is_dry_run(request: &Request) -> bool {
    request.query.get("dryRun").map(String::as_str) == Some("All")
        || request.body["dryRun"]
            .as_array()
            .is_some_and(|values| values.iter().any(|v| v == "All"))
}

fn merge(target: &mut Value, patch: &Value) {
    match (target, patch) {
        (Value::Object(target), Value::Object(patch)) => {
            for (key, value) in patch {
                merge(target.entry(key.clone()).or_insert(Value::Null), value);
            }
        }
        (target, patch) => *target = patch.clone(),
    }
}

async fn fake() -> FakeApi {
    FakeApi::start(Arc::new(cluster)).await
}

fn deployment(name: &str, replicas: i64) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "Deployment",
           "metadata": {"name": name, "namespace": "shop", "uid": format!("uid-{name}"),
                        "resourceVersion": "1", "labels": {"app": name},
                        "annotations": {"deployment.kubernetes.io/revision": "3"}},
           "spec": {"replicas": replicas, "selector": {"matchLabels": {"app": name}},
                    "template": {"metadata": {"labels": {"app": name}},
                                 "spec": {"containers": [{"name": "main", "image": "web:3"}]}}}})
}

const DEPLOY: &str = "/apis/apps/v1/namespaces/shop/deployments";

// ── apply ────────────────────────────────────────────────────────────────

const MANIFEST: &str = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: web-config
data:
  mode: fast
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
  namespace: shop
spec:
  replicas: 2
  selector: {matchLabels: {app: web}}
  template:
    metadata: {labels: {app: web}}
    spec: {containers: [{name: main, image: "web:4"}]}
"#;

#[tokio::test]
async fn apply_is_server_side_apply_in_manifest_order() {
    let api = fake().await;
    let applied = api
        .api()
        .apply(MANIFEST, Some("shop"), false)
        .await
        .unwrap();
    assert_eq!(applied.len(), 2);
    assert_eq!(applied[0].object.kind, "ConfigMap");
    assert_eq!(
        applied[0].object.namespace.as_deref(),
        Some("shop"),
        "default namespace"
    );
    assert_eq!(applied[1].object.name, "web");
    assert!(applied.iter().all(|a| a.action == ApplyAction::Created));
    let writes: Vec<_> = api
        .writes()
        .into_iter()
        .filter(|r| r.method == "PATCH")
        .collect();
    assert_eq!(writes.len(), 2);
    assert_eq!(
        writes[0].path,
        "/api/v1/namespaces/shop/configmaps/web-config"
    );
    assert_eq!(
        writes[1].path,
        "/apis/apps/v1/namespaces/shop/deployments/web"
    );
    for write in &writes {
        assert_eq!(
            write.content_type.as_deref(),
            Some("application/apply-patch+yaml")
        );
        assert_eq!(
            write.query.get("fieldManager").map(String::as_str),
            Some("filar")
        );
        assert_eq!(
            write.query.get("fieldValidation").map(String::as_str),
            Some("Strict")
        );
        assert!(!write.query.contains_key("force"), "never forced");
        assert!(!write.query.contains_key("dryRun"));
    }
    assert!(api.get(&format!("{DEPLOY}/web")).is_some(), "stored");
}

#[tokio::test]
async fn apply_dry_run_changes_nothing_and_tells_configured_from_unchanged() {
    let api = fake().await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 2));
    let dry = api.api().apply(MANIFEST, Some("shop"), true).await.unwrap();
    assert!(api
        .writes()
        .iter()
        .filter(|r| r.method == "PATCH")
        .all(|r| r.query.get("dryRun").map(String::as_str) == Some("All")));
    assert_eq!(dry[0].action, ApplyAction::Created);
    assert_eq!(
        dry[1].action,
        ApplyAction::Configured,
        "image web:3 → web:4"
    );
    assert_eq!(
        dry[1].live.as_ref().unwrap()["spec"]["template"]["spec"]["containers"][0]["image"],
        "web:3"
    );
    assert_eq!(
        dry[1].result["spec"]["template"]["spec"]["containers"][0]["image"],
        "web:4"
    );
    assert!(api
        .get("/api/v1/namespaces/shop/configmaps/web-config")
        .is_none());
    assert_eq!(
        api.get(&format!("{DEPLOY}/web")).unwrap()["spec"]["template"]["spec"]["containers"][0]
            ["image"],
        "web:3"
    );

    api.api()
        .apply(MANIFEST, Some("shop"), false)
        .await
        .unwrap();
    let again = api.api().apply(MANIFEST, Some("shop"), true).await.unwrap();
    assert_eq!(again[0].action, ApplyAction::Unchanged);
    assert_eq!(again[1].action, ApplyAction::Unchanged);
}

#[tokio::test]
async fn a_secret_is_applied_but_its_values_never_come_back() {
    let api = fake().await;
    api.put(
        "/api/v1/namespaces/shop/secrets/db",
        json!({"apiVersion": "v1", "kind": "Secret", "metadata": {"name": "db", "namespace": "shop"},
               "data": {"user": "b64:admin", "password": "b64:old-secret-value"}}),
    );
    let manifest = r#"
apiVersion: v1
kind: Secret
metadata: {name: db}
stringData:
  user: admin
  password: new-secret-value
  token: fresh-token-value
"#;
    let applied = api
        .api()
        .apply(manifest, Some("shop"), false)
        .await
        .unwrap();
    let secret = &applied[0];
    let printed = format!("{secret:?}");
    for value in [
        "old-secret-value",
        "new-secret-value",
        "fresh-token-value",
        "admin",
    ] {
        assert!(!printed.contains(value), "{value} leaked: {printed}");
    }
    assert_eq!(secret.result["data"]["password"], SECRET_HIDDEN);
    assert_eq!(secret.live.as_ref().unwrap()["data"]["user"], SECRET_HIDDEN);
    let states: Vec<_> = secret
        .secret_keys
        .iter()
        .map(|c| (c.key.as_str(), c.state))
        .collect();
    assert_eq!(
        states,
        [
            ("password", SecretKeyState::Changed),
            ("token", SecretKeyState::Added),
            ("user", SecretKeyState::Unchanged),
        ]
    );
    // The value did reach the API server: only the answer is hidden.
    assert_eq!(
        api.get("/api/v1/namespaces/shop/secrets/db").unwrap()["data"]["token"],
        "b64:fresh-token-value"
    );
}

#[tokio::test]
async fn apply_refuses_empty_or_incomplete_manifests_before_the_api() {
    let api = fake().await;
    let writes = api.api();
    assert!(writes
        .apply("---\n", None, true)
        .await
        .unwrap_err()
        .to_string()
        .contains("no objects"));
    let error = writes
        .apply(
            "apiVersion: v1\nkind: ConfigMap\nmetadata: {}\n",
            None,
            true,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("metadata.name"), "{error}");
    let error = writes
        .apply(
            "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: x}\n",
            None,
            true,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("namespaced"), "{error}");
    assert!(api.writes().iter().all(|r| r.method != "PATCH"));
}

#[tokio::test]
async fn an_unknown_kind_is_named() {
    let api = fake().await;
    let error = api
        .api()
        .apply(
            "apiVersion: v1\nkind: Gizmo\nmetadata: {name: x}\n",
            Some("shop"),
            true,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("no kind Gizmo"), "{error}");
}

// ── errors ───────────────────────────────────────────────────────────────

async fn refusing(code: u16, reason: &'static str, message: &'static str) -> FakeApi {
    FakeApi::start(Arc::new(move |request: &Request, state: &mut State| {
        if request.method == "PATCH" {
            return status(code, reason, message);
        }
        cluster(request, state)
    }))
    .await
}

#[tokio::test]
async fn rbac_admission_conflict_and_validation_errors_read_clearly() {
    let rbac = refusing(403, "Forbidden", "deployments.apps \"web\" is forbidden: User \"dev\" cannot patch resource \"deployments\" in API group \"apps\" in the namespace \"shop\"").await;
    let error = rbac
        .api()
        .apply(MANIFEST, Some("shop"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("нет права apply на configmaps в namespace shop (контекст prod-eu-admin)"),
        "{error}"
    );

    let admission = refusing(403, "Forbidden", "admission webhook \"policy.example.com\" denied the request: images must come from registry.example.com").await;
    let error = admission
        .api()
        .apply(MANIFEST, Some("shop"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("denied the request: images must come from registry.example.com"),
        "{error}"
    );

    let conflict = refusing(409, "Conflict", "Apply failed with 1 conflict: conflict with \"kubectl-client-side-apply\" using apps/v1: .spec.replicas").await;
    let error = conflict
        .api()
        .apply(MANIFEST, Some("shop"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("server-side apply conflict"), "{error}");
    assert!(error.contains("kubectl-client-side-apply"), "{error}");
    assert!(error.contains("does not force"), "{error}");

    let invalid = refusing(
        400,
        "BadRequest",
        ".spec.replicaz: field not declared in schema",
    )
    .await;
    let error = invalid
        .api()
        .apply(MANIFEST, Some("shop"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("field not declared in schema"), "{error}");
}

#[tokio::test]
async fn a_failure_mid_manifest_says_what_was_already_applied() {
    let api = FakeApi::start(Arc::new(|request: &Request, state: &mut State| {
        if request.method == "PATCH" && request.path.contains("deployments") {
            return status(422, "Invalid", "spec.replicas: Invalid value: -1");
        }
        cluster(request, state)
    }))
    .await;
    let error = api
        .api()
        .apply(MANIFEST, Some("shop"), false)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.starts_with("Deployment web:"), "{error}");
    assert!(
        error.contains("1 of 2 objects before it were applied"),
        "{error}"
    );
}

// ── scale, restart, undo ─────────────────────────────────────────────────

#[tokio::test]
async fn scale_patches_the_scale_subresource() {
    let api = fake().await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 3));
    let outcome = api
        .api()
        .scale(ScaleKind::Deployment, "shop", "web", 0, true)
        .await
        .unwrap();
    assert_eq!((outcome.from, outcome.to), (3, 0));
    assert_eq!(
        api.get(&format!("{DEPLOY}/web")).unwrap()["spec"]["replicas"],
        3,
        "dry run"
    );
    let patch = api
        .writes()
        .into_iter()
        .find(|r| r.method == "PATCH")
        .unwrap();
    assert_eq!(patch.path, format!("{DEPLOY}/web/scale"));
    assert_eq!(
        patch.content_type.as_deref(),
        Some("application/merge-patch+json")
    );
    assert_eq!(patch.query.get("dryRun").map(String::as_str), Some("All"));
    assert_eq!(patch.body, json!({"spec": {"replicas": 0}}));
    api.api()
        .scale(ScaleKind::Deployment, "shop", "web", 5, false)
        .await
        .unwrap();
    assert_eq!(
        api.get(&format!("{DEPLOY}/web")).unwrap()["spec"]["replicas"],
        5
    );
    assert!(api
        .api()
        .scale(ScaleKind::Deployment, "shop", "web", -1, true)
        .await
        .is_err());
}

#[tokio::test]
async fn rollout_restart_sets_the_restarted_at_annotation() {
    let api = fake().await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 2));
    let outcome = api
        .api()
        .rollout_restart(RestartKind::Deployment, "shop", "web", false)
        .await
        .unwrap();
    let patch = api
        .writes()
        .into_iter()
        .find(|r| r.method == "PATCH")
        .unwrap();
    assert_eq!(patch.path, format!("{DEPLOY}/web"));
    assert_eq!(
        patch.content_type.as_deref(),
        Some("application/merge-patch+json")
    );
    let stamp = patch.body["spec"]["template"]["metadata"]["annotations"][RESTARTED_AT_ANNOTATION]
        .as_str()
        .unwrap()
        .to_string();
    assert!(stamp.ends_with('Z') && stamp.contains('T'), "{stamp}");
    assert_eq!(
        outcome.result["spec"]["template"]["metadata"]["annotations"][RESTARTED_AT_ANNOTATION],
        stamp.as_str()
    );
    assert!(outcome.live["spec"]["template"]["metadata"]["annotations"].is_null());
}

fn replicaset(revision: i64, image: &str) -> Value {
    json!({"apiVersion": "apps/v1", "kind": "ReplicaSet",
           "metadata": {"name": format!("web-{revision}"), "namespace": "shop",
                        "annotations": {"deployment.kubernetes.io/revision": revision.to_string()},
                        "ownerReferences": [{"apiVersion": "apps/v1", "kind": "Deployment", "name": "web", "uid": "uid-web"}]},
           "spec": {"selector": {"matchLabels": {"app": "web"}},
                    "template": {"metadata": {"labels": {"app": "web", "pod-template-hash": format!("h{revision}")}},
                                 "spec": {"containers": [{"name": "main", "image": image}]}}}})
}

#[tokio::test]
async fn rollout_undo_takes_the_template_of_the_previous_revision() {
    let api = fake().await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 2));
    for (revision, image) in [(1, "web:1"), (2, "web:2"), (3, "web:3")] {
        api.put(
            &format!("/apis/apps/v1/namespaces/shop/replicasets/web-{revision}"),
            replicaset(revision, image),
        );
    }
    let undo = api
        .api()
        .rollout_undo("shop", "web", None, true)
        .await
        .unwrap();
    assert_eq!((undo.from_revision, undo.to_revision), (3, 2));
    let put = api
        .writes()
        .into_iter()
        .find(|r| r.method == "PUT")
        .unwrap();
    assert_eq!(put.path, format!("{DEPLOY}/web"));
    assert_eq!(put.query.get("dryRun").map(String::as_str), Some("All"));
    let template = &put.body["spec"]["template"];
    assert_eq!(template["spec"]["containers"][0]["image"], "web:2");
    assert!(
        template["metadata"]["labels"]["pod-template-hash"].is_null(),
        "hash label dropped"
    );
    assert_eq!(
        api.get(&format!("{DEPLOY}/web")).unwrap()["spec"]["template"]["spec"]["containers"][0]
            ["image"],
        "web:3"
    );

    let to_one = api
        .api()
        .rollout_undo("shop", "web", Some(1), false)
        .await
        .unwrap();
    assert_eq!(to_one.to_revision, 1);
    assert_eq!(
        api.get(&format!("{DEPLOY}/web")).unwrap()["spec"]["template"]["spec"]["containers"][0]
            ["image"],
        "web:1"
    );
    assert!(api
        .api()
        .rollout_undo("shop", "web", Some(9), true)
        .await
        .unwrap_err()
        .to_string()
        .contains("no revision 9"));
}

// ── delete ───────────────────────────────────────────────────────────────

fn pod(name: &str, app: &str) -> Value {
    json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name, "namespace": "shop", "labels": {"app": app}}})
}

const PODS: &str = "/api/v1/namespaces/shop/pods";

#[tokio::test]
async fn delete_by_selector_dry_run_lists_every_match_and_deletes_nothing() {
    let api = fake().await;
    for (name, app) in [("web-1", "web"), ("web-2", "web"), ("db-1", "db")] {
        api.put(&format!("{PODS}/{name}"), pod(name, app));
    }
    let kind = ObjectKind {
        api_version: None,
        kind: "pod".into(),
    };
    let selector = DeleteTarget::Selector {
        labels: Some("app=web".into()),
        fields: None,
    };
    let listed = api
        .api()
        .delete(
            &kind,
            Some("shop"),
            &selector,
            &DeleteOptions::default(),
            true,
        )
        .await
        .unwrap();
    let names: Vec<_> = listed
        .iter()
        .map(|d| (d.object.name.as_str(), d.outcome))
        .collect();
    assert_eq!(
        names,
        [
            ("web-1", DeleteOutcome::WouldDelete),
            ("web-2", DeleteOutcome::WouldDelete)
        ]
    );
    assert!(api.get(&format!("{PODS}/web-1")).is_some());
    let deletes: Vec<_> = api
        .writes()
        .into_iter()
        .filter(|r| r.method == "DELETE")
        .collect();
    assert_eq!(deletes.len(), 2, "one dry-run delete per object, by name");
    for delete in &deletes {
        assert!(is_dry_run(delete), "{delete:?}");
        assert_eq!(delete.body["propagationPolicy"], "Background");
    }

    // The real run deletes exactly those names; one vanished in between.
    api.state
        .lock()
        .unwrap()
        .objects
        .remove(&format!("{PODS}/web-2"));
    api.put(&format!("{PODS}/web-3"), pod("web-3", "web"));
    let names = DeleteTarget::Names(listed.iter().map(|d| d.object.name.clone()).collect());
    let deleted = api
        .api()
        .delete(
            &kind,
            Some("shop"),
            &names,
            &DeleteOptions::default(),
            false,
        )
        .await
        .unwrap();
    let outcomes: Vec<_> = deleted
        .iter()
        .map(|d| (d.object.name.as_str(), d.outcome))
        .collect();
    assert_eq!(
        outcomes,
        [
            ("web-1", DeleteOutcome::Deleted),
            ("web-2", DeleteOutcome::AlreadyGone)
        ]
    );
    assert!(
        api.get(&format!("{PODS}/web-3")).is_some(),
        "appeared after the dry run: kept"
    );
}

#[tokio::test]
async fn delete_options_and_cluster_scoped_kinds() {
    let api = fake().await;
    api.put(
        "/api/v1/namespaces/old",
        json!({"apiVersion": "v1", "kind": "Namespace", "metadata": {"name": "old"}}),
    );
    let kind = ObjectKind {
        api_version: Some("v1".into()),
        kind: "Namespace".into(),
    };
    let options = DeleteOptions {
        propagation: Propagation::Foreground,
        grace_period_seconds: Some(30),
        force: true,
    };
    let deleted = api
        .api()
        .delete(
            &kind,
            Some("ignored"),
            &DeleteTarget::Name("old".into()),
            &options,
            false,
        )
        .await
        .unwrap();
    assert_eq!(deleted[0].object.namespace, None, "cluster-scoped");
    let delete = api
        .writes()
        .into_iter()
        .find(|r| r.method == "DELETE")
        .unwrap();
    assert_eq!(delete.path, "/api/v1/namespaces/old");
    assert_eq!(delete.body["propagationPolicy"], "Foreground");
    assert_eq!(
        delete.body["gracePeriodSeconds"], 0,
        "force wins over the grace period"
    );
    let missing = api
        .api()
        .delete(
            &kind,
            None,
            &DeleteTarget::Name("nope".into()),
            &DeleteOptions::default(),
            true,
        )
        .await
        .unwrap_err();
    assert!(matches!(missing, CoreError::NotFound(_)), "{missing}");
}

#[tokio::test]
async fn a_kind_is_found_by_kind_plural_or_singular() {
    let api = fake().await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 1));
    for name in [
        "Deployment",
        "deployments",
        "deployment",
        "deployments.apps",
    ] {
        let kind = ObjectKind {
            api_version: None,
            kind: name.into(),
        };
        let listed = api
            .api()
            .delete(
                &kind,
                Some("shop"),
                &DeleteTarget::Name("web".into()),
                &DeleteOptions::default(),
                true,
            )
            .await
            .unwrap();
        assert_eq!(listed[0].object.api_version, "apps/v1", "{name}");
    }
}

// ── pure helpers ─────────────────────────────────────────────────────────

#[test]
fn hiding_touches_only_secret_values_and_the_last_applied_copy() {
    let mut secret = json!({"kind": "Secret", "metadata": {"annotations": {
        "kubectl.kubernetes.io/last-applied-configuration": "{\"data\":{\"k\":\"c2VjcmV0\"}}", "team": "web"}},
        "data": {"k": "c2VjcmV0"}, "stringData": {"s": "plain"}});
    hide_secret_values(&mut secret);
    assert_eq!(secret["data"]["k"], SECRET_HIDDEN);
    assert_eq!(secret["stringData"]["s"], SECRET_HIDDEN);
    assert!(
        secret["metadata"]["annotations"]["kubectl.kubernetes.io/last-applied-configuration"]
            .is_null()
    );
    assert_eq!(secret["metadata"]["annotations"]["team"], "web");
    let mut config = json!({"kind": "ConfigMap", "data": {"k": "v"}});
    hide_secret_values(&mut config);
    assert_eq!(config["data"]["k"], "v");
}

#[test]
fn manifests_parse_as_yaml_documents_or_json() {
    assert_eq!(parse_manifest(MANIFEST).unwrap().len(), 2);
    let json = r#"{"apiVersion": "v1", "kind": "ConfigMap", "metadata": {"name": "x"}}"#;
    assert_eq!(parse_manifest(json).unwrap()[0]["kind"], "ConfigMap");
    assert!(parse_manifest("kind: [unclosed").is_err());
}

// ── review round 1 (#651) ────────────────────────────────────────────────

/// `cluster` with lists cut in two pages: the first item, then the rest
/// behind a `continue` token.
fn paged(request: &Request, state: &mut State) -> (u16, Value) {
    let (code, mut answer) = cluster(request, state);
    let Some(items) = answer["items"].as_array().cloned() else {
        return (code, answer);
    };
    if items.len() < 2 {
        return (code, answer);
    }
    if request.query.contains_key("continue") {
        answer["items"] = Value::Array(items[1..].to_vec());
    } else {
        answer["items"] = Value::Array(items[..1].to_vec());
        answer["metadata"]["continue"] = json!("page-2");
    }
    (code, answer)
}

#[tokio::test]
async fn rollout_undo_reads_every_replicaset_page() {
    let api = FakeApi::start(Arc::new(paged)).await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 2));
    for (revision, image) in [(1, "web:1"), (2, "web:2"), (3, "web:3")] {
        api.put(
            &format!("/apis/apps/v1/namespaces/shop/replicasets/web-{revision}"),
            replicaset(revision, image),
        );
    }
    let undo = api
        .api()
        .rollout_undo("shop", "web", None, true)
        .await
        .unwrap();
    assert_eq!(
        (undo.from_revision, undo.to_revision),
        (3, 2),
        "revision 2 is on the second page"
    );
}

#[tokio::test]
async fn a_selector_delete_follows_continue_and_keeps_the_selector() {
    let api = FakeApi::start(Arc::new(paged)).await;
    for name in ["web-1", "web-2", "web-3"] {
        api.put(&format!("{PODS}/{name}"), pod(name, "web"));
    }
    api.put(&format!("{PODS}/db-1"), pod("db-1", "db"));
    let kind = ObjectKind {
        api_version: Some("v1".into()),
        kind: "Pod".into(),
    };
    let listed = api
        .api()
        .delete(
            &kind,
            Some("shop"),
            &DeleteTarget::Selector {
                labels: Some("app=web".into()),
                fields: None,
            },
            &DeleteOptions::default(),
            true,
        )
        .await
        .unwrap();
    let names: Vec<_> = listed.iter().map(|d| d.object.name.as_str()).collect();
    assert_eq!(names, ["web-1", "web-2", "web-3"]);
    let continued = api
        .writes()
        .into_iter()
        .find(|r| r.method == "GET" && r.query.contains_key("continue"))
        .expect("a second page was asked for");
    assert_eq!(
        continued.query.get("labelSelector").map(String::as_str),
        Some("app=web")
    );
}

#[tokio::test]
async fn paused_deployments_are_neither_restarted_nor_rolled_back() {
    let api = fake().await;
    let mut paused = deployment("web", 2);
    paused["spec"]["paused"] = json!(true);
    api.put(&format!("{DEPLOY}/web"), paused);
    api.put(
        "/apis/apps/v1/namespaces/shop/replicasets/web-2",
        replicaset(2, "web:2"),
    );
    let restart = api
        .api()
        .rollout_restart(RestartKind::Deployment, "shop", "web", true)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        restart.contains("is paused") && restart.contains("resume"),
        "{restart}"
    );
    let undo = api
        .api()
        .rollout_undo("shop", "web", None, true)
        .await
        .unwrap_err()
        .to_string();
    assert!(undo.contains("is paused"), "{undo}");
    assert!(
        api.writes().iter().all(|r| r.method == "GET"),
        "nothing written"
    );
}

#[tokio::test]
async fn a_dry_run_restart_leaves_the_template_alone() {
    let api = fake().await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 2));
    api.api()
        .rollout_restart(RestartKind::Deployment, "shop", "web", true)
        .await
        .unwrap();
    let patch = api
        .writes()
        .into_iter()
        .find(|r| r.method == "PATCH")
        .unwrap();
    assert_eq!(patch.query.get("dryRun").map(String::as_str), Some("All"));
    assert!(
        api.get(&format!("{DEPLOY}/web")).unwrap()["spec"]["template"]["metadata"]["annotations"]
            .is_null()
    );
}

#[tokio::test]
async fn a_kind_with_an_api_version_is_found_by_plural_or_singular_too() {
    let api = fake().await;
    api.put(&format!("{DEPLOY}/web"), deployment("web", 1));
    for name in ["Deployment", "deployments", "deployment"] {
        let kind = ObjectKind {
            api_version: Some("apps/v1".into()),
            kind: name.into(),
        };
        let listed = api
            .api()
            .delete(
                &kind,
                Some("shop"),
                &DeleteTarget::Name("web".into()),
                &DeleteOptions::default(),
                true,
            )
            .await
            .unwrap();
        assert_eq!(listed[0].object.kind, "Deployment", "{name}");
    }
}

#[tokio::test]
async fn core_kinds_resolve_while_another_api_group_is_broken() {
    let api = FakeApi::start(Arc::new(|request: &Request, state: &mut State| {
        if request.path == "/apis/apps/v1" {
            return status(503, "ServiceUnavailable", "apps discovery is down");
        }
        cluster(request, state)
    }))
    .await;
    api.put(&format!("{PODS}/web-1"), pod("web-1", "web"));
    let kind = ObjectKind {
        api_version: None,
        kind: "pod".into(),
    };
    let listed = api
        .api()
        .delete(
            &kind,
            Some("shop"),
            &DeleteTarget::Name("web-1".into()),
            &DeleteOptions::default(),
            true,
        )
        .await
        .unwrap();
    assert_eq!(listed[0].object.kind, "Pod");
}

#[tokio::test]
async fn a_non_string_namespace_is_refused_not_replaced() {
    let api = fake().await;
    let manifest = "apiVersion: v1\nkind: ConfigMap\nmetadata: {name: c, namespace: 123}\n";
    let error = api
        .api()
        .apply(manifest, Some("prod"), false)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("metadata.namespace must be a string"),
        "{error}"
    );
    assert!(api.writes().iter().all(|r| r.method != "PATCH"));
}

#[tokio::test]
async fn an_accepted_delete_of_an_object_still_there_is_pending() {
    // An aggregated API answers a bare Status and keeps the object for now.
    let api = FakeApi::start(Arc::new(|request: &Request, state: &mut State| {
        if request.method == "DELETE" && !is_dry_run(request) {
            return (
                200,
                json!({"kind": "Status", "apiVersion": "v1", "metadata": {},
                       "status": "Success", "code": 200}),
            );
        }
        cluster(request, state)
    }))
    .await;
    api.put(&format!("{PODS}/web-1"), pod("web-1", "web"));
    let kind = ObjectKind {
        api_version: Some("v1".into()),
        kind: "Pod".into(),
    };
    let delete = |name: &'static str| {
        let api = api.api();
        let kind = kind.clone();
        async move {
            api.delete(
                &kind,
                Some("shop"),
                &DeleteTarget::Name(name.into()),
                &DeleteOptions::default(),
                false,
            )
            .await
            .unwrap()
        }
    };
    assert_eq!(delete("web-1").await[0].outcome, DeleteOutcome::Pending);
    // Gone by the time it is checked: deleted.
    let gone = FakeApi::start(Arc::new(|request: &Request, state: &mut State| {
        if request.method == "DELETE" {
            state.objects.remove(&request.path);
            return (
                200,
                json!({"kind": "Status", "apiVersion": "v1", "metadata": {},
                       "status": "Success", "code": 200}),
            );
        }
        cluster(request, state)
    }))
    .await;
    gone.put(&format!("{PODS}/web-3"), pod("web-3", "web"));
    let deleted = gone
        .api()
        .delete(
            &kind,
            Some("shop"),
            &DeleteTarget::Name("web-3".into()),
            &DeleteOptions::default(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(deleted[0].outcome, DeleteOutcome::Deleted);
}

#[tokio::test]
async fn submitted_secret_values_are_masked_in_server_errors() {
    // "aHVudGVyMjI=" is base64 of "hunter22".
    let api = FakeApi::start(Arc::new(|request: &Request, state: &mut State| {
        if request.method == "PATCH" {
            return status(
                403,
                "Forbidden",
                "admission webhook \"leaks.example.com\" denied the request: password s3cr3t-value, token aHVudGVyMjI= (hunter22) are weak",
            );
        }
        cluster(request, state)
    }))
    .await;
    let manifest = "apiVersion: v1\nkind: Secret\nmetadata: {name: db}\nstringData: {password: s3cr3t-value}\ndata: {token: aHVudGVyMjI=}\n";
    let error = api
        .api()
        .apply(manifest, Some("shop"), true)
        .await
        .unwrap_err()
        .to_string();
    for leaked in ["s3cr3t-value", "aHVudGVyMjI=", "hunter22"] {
        assert!(!error.contains(leaked), "{leaked} in {error}");
    }
    assert!(
        error.contains("denied the request") && error.contains(SECRET_HIDDEN),
        "{error}"
    );
}

#[test]
fn status_counts_as_content_only_without_a_status_subresource() {
    let live = json!({"kind": "Widget", "metadata": {"name": "w"}, "spec": {"a": 1}, "status": {"phase": "old"}});
    let result = json!({"kind": "Widget", "metadata": {"name": "w"}, "spec": {"a": 1}, "status": {"phase": "new"}});
    assert_eq!(
        apply_action(Some(&live), &result, false),
        ApplyAction::Unchanged
    );
    assert_eq!(
        apply_action(Some(&live), &result, true),
        ApplyAction::Configured
    );
}

#[tokio::test]
async fn a_message_quoting_a_short_secret_value_is_withheld_whole() {
    let api = refusing(
        403,
        "Forbidden",
        "admission webhook denied the request: pin 7x9 is too short",
    )
    .await;
    let manifest = "apiVersion: v1\nkind: Secret\nmetadata: {name: pin}\nstringData: {pin: 7x9}\n";
    let error = api
        .api()
        .apply(manifest, Some("shop"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("7x9"), "{error}");
    assert!(error.contains("message is withheld"), "{error}");
}

#[test]
fn a_singular_matches_its_plural_with_s_or_es() {
    let resource = |plural: &str, kind: &str| {
        (
            ApiResource {
                group: "networking.k8s.io".into(),
                version: "v1".into(),
                api_version: "networking.k8s.io/v1".into(),
                kind: kind.into(),
                plural: plural.into(),
            },
            ApiCapabilities {
                scope: Scope::Cluster,
                subresources: Vec::new(),
                operations: Vec::new(),
            },
        )
    };
    let resources = vec![
        resource("ingressclasses", "IngressClass"),
        resource("ingresses", "Ingress"),
    ];
    for (wanted, kind) in [
        ("ingressclass", "IngressClass"),
        ("ingressclasses", "IngressClass"),
        ("ingress", "Ingress"),
    ] {
        let found = matching_resources(resources.clone(), wanted);
        assert_eq!(found.len(), 1, "{wanted}");
        assert_eq!(found[0].0.kind, kind, "{wanted}");
    }
}

#[tokio::test]
async fn status_counts_for_a_custom_resource_without_a_status_subresource() {
    let api = fake().await;
    for (plural, kind) in [("widgets", "Widget"), ("gadgets", "Gadget")] {
        api.put(
            &format!("/apis/example.com/v1/namespaces/shop/{plural}/one"),
            json!({"apiVersion": "example.com/v1", "kind": kind,
                   "metadata": {"name": "one", "namespace": "shop"},
                   "spec": {"size": 1}, "status": {"phase": "old"}}),
        );
    }
    let action = |kind: &str| {
        let manifest = format!(
            "apiVersion: example.com/v1\nkind: {kind}\nmetadata: {{name: one}}\nspec: {{size: 1}}\nstatus: {{phase: new}}\n"
        );
        let api = api.api();
        async move { api.apply(&manifest, Some("shop"), true).await.unwrap()[0].action }
    };
    assert_eq!(
        action("Widget").await,
        ApplyAction::Configured,
        "status is the main endpoint's"
    );
    assert_eq!(
        action("Gadget").await,
        ApplyAction::Unchanged,
        "status belongs to /status"
    );
}

#[tokio::test]
async fn an_exact_plural_wins_over_a_guessed_singular() {
    let api = fake().await;
    api.put(
        "/apis/example.com/v1/namespaces/shop/widgets/one",
        json!({"apiVersion": "example.com/v1", "kind": "Widget", "metadata": {"name": "one", "namespace": "shop"}}),
    );
    for api_version in [Some("example.com/v1".to_string()), None] {
        let kind = ObjectKind {
            api_version: api_version.clone(),
            kind: "widgets".into(),
        };
        let listed = api
            .api()
            .delete(
                &kind,
                Some("shop"),
                &DeleteTarget::Name("one".into()),
                &DeleteOptions::default(),
                true,
            )
            .await
            .unwrap();
        assert_eq!(listed[0].object.kind, "Widget", "{api_version:?}");
    }
}

#[tokio::test]
async fn the_encoded_form_of_string_data_is_masked_too() {
    // "c2VjcmV0LXRva2Vu" is base64 of "secret-token".
    let api = refusing(
        403,
        "Forbidden",
        "admission webhook denied the request: data.token=c2VjcmV0LXRva2Vu is reused",
    )
    .await;
    let manifest =
        "apiVersion: v1\nkind: Secret\nmetadata: {name: t}\nstringData: {token: secret-token}\n";
    let error = api
        .api()
        .apply(manifest, Some("shop"), true)
        .await
        .unwrap_err()
        .to_string();
    assert!(!error.contains("c2VjcmV0LXRva2Vu"), "{error}");
}

#[test]
fn a_yaml_error_names_the_position_not_the_text() {
    let manifest = "apiVersion: v1\nkind: Secret\nmetadata: {name: x}\nstringData:\n  password: \"hunter22secret\n";
    let error = parse_manifest(manifest).unwrap_err().to_string();
    assert!(!error.contains("hunter22secret"), "{error}");
    assert!(error.contains("not valid YAML"), "{error}");
}
