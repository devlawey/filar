//! Kubernetes ↔ log sources (#583): `pod_logs_history`.
//!
//! A cluster or pod tab knows the namespace, the pod (or a label selector)
//! and the container; a log source linked to the cluster (`clusters = […]`)
//! knows its field names (`k8s_fields`). From both filar writes the query
//! itself, in the source's language, so the model never guesses label names.
//! The query then runs through the same frame and gate as `query_logs`.
//!
//! A fleet asks once for all its pods: one query per linked source with all
//! the pods in it, folded by pod into a difference table.

use std::collections::{BTreeMap, BTreeSet};

use filar_core::{CoreError, K8sFields, LogSourceConfig, LogSourceKind, Result};
use serde_json::{json, Map, Value};

use super::fold::normalize_message;
use super::{LogQuery, LogResult};

/// Tool name.
pub const TOOL_POD_LOGS_HISTORY: &str = "pod_logs_history";

/// Prompt paragraph telling `k8s_logs` and `pod_logs_history` apart.
pub const POD_LOGS_HISTORY_PROMPT: &str = "pod_logs_history reads a pod's history from the cluster's centralised log store for a window, \
including pods that no longer exist; filar writes the store query itself from the namespace, pod or label selector and container. \
k8s_logs is only the short tail of the current and previous container run, read from the node, without history. \
Prefer k8s_logs for a crash that just happened, pod_logs_history for \"what happened over the last hour\" or for deleted pods.";

/// One label term of a selector: `key=value` or `key!=value`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LabelTerm {
    /// Label name.
    pub key: String,
    /// Label value.
    pub value: String,
    /// `true` for `=`/`==`, `false` for `!=`.
    pub equal: bool,
}

/// Which pods of a namespace: by name, by labels, or all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PodPick {
    /// These pod names.
    Names(Vec<String>),
    /// Pods whose labels match every term.
    Labels(Vec<LabelTerm>),
    /// Every pod of the namespace.
    All,
}

/// Pods of one namespace, optionally one container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PodScope {
    /// Namespace.
    pub namespace: String,
    /// Which pods.
    pub pick: PodPick,
    /// Container, when one is meant.
    pub container: Option<String>,
}

/// Parse an equality-based label selector (`app=web,tier!=batch`). Set-based
/// forms (`in`, `notin`, bare keys) are refused: they cannot be written as a
/// field filter in every store language.
pub fn parse_selector(selector: &str) -> Result<Vec<LabelTerm>> {
    let mut terms = Vec::new();
    for part in selector.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let (key, value, equal) = if let Some((k, v)) = part.split_once("!=") {
            (k, v, false)
        } else if let Some((k, v)) = part.split_once("==") {
            (k, v, true)
        } else if let Some((k, v)) = part.split_once('=') {
            (k, v, true)
        } else {
            return Err(CoreError::Other(format!(
                "selector term \"{part}\" is not key=value or key!=value; set-based selectors are not supported here — name the pods instead"
            )));
        };
        let (key, value) = (key.trim(), value.trim());
        if value.is_empty() {
            // Valid in Kubernetes, but log stores cannot tell a label with an
            // empty value from a missing one: the filter would match the
            // wrong pods in one store and none in another.
            return Err(CoreError::Other(format!(
                "selector term \"{part}\" has an empty value; log stores cannot match empty labels reliably — name the pods instead"
            )));
        }
        if key.is_empty() || key.contains(char::is_whitespace) || value.contains(char::is_whitespace) {
            return Err(CoreError::Other(format!("selector term \"{part}\" is malformed")));
        }
        terms.push(LabelTerm {
            key: key.to_string(),
            value: value.to_string(),
            equal,
        });
    }
    if terms.is_empty() {
        return Err(CoreError::Other("the selector is empty".into()));
    }
    Ok(terms)
}

/// `"text"` with `\` and `"` escaped — LogsQL, LogQL and `query_string`
/// share this quoting.
pub fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// A field name for Lucene `query_string`: reserved characters escaped.
fn lucene_field(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if "+-=&|><!(){}[]^\"~*?:\\/ ".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A Prometheus/Loki label name as collectors write it: anything outside
/// `[A-Za-z0-9_]` becomes `_`, a leading digit gets a `_` prefix.
fn loki_label(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    if out.starts_with(|c: char| c.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

/// An RE2 pattern matching exactly `text`.
fn regex_literal(text: &str) -> String {
    let mut out = String::new();
    for c in text.chars() {
        if "\\.+*?()|[]{}^$".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

fn logsql_scope(f: &K8sFields, scope: &PodScope) -> String {
    let field = |name: &str| quote(name);
    let mut parts = vec![format!("{}:={}", field(f.namespace), quote(&scope.namespace))];
    match &scope.pick {
        PodPick::Names(names) if names.len() == 1 => {
            parts.push(format!("{}:={}", field(f.pod), quote(&names[0])));
        }
        PodPick::Names(names) => {
            let list: Vec<String> = names.iter().map(|n| quote(n)).collect();
            parts.push(format!("{}:in({})", field(f.pod), list.join(",")));
        }
        PodPick::Labels(terms) => {
            for t in terms {
                let term = format!("{}:={}", field(&format!("{}{}", f.labels_prefix, t.key)), quote(&t.value));
                parts.push(if t.equal { term } else { format!("-{term}") });
            }
        }
        PodPick::All => {}
    }
    if let Some(c) = &scope.container {
        parts.push(format!("{}:={}", field(f.container), quote(c)));
    }
    parts.join(" ")
}

fn lucene_scope(f: &K8sFields, scope: &PodScope) -> String {
    let mut parts = vec![format!("{}:{}", lucene_field(f.namespace), quote(&scope.namespace))];
    match &scope.pick {
        PodPick::Names(names) => {
            let list: Vec<String> = names.iter().map(|n| quote(n)).collect();
            parts.push(format!("{}:({})", lucene_field(f.pod), list.join(" OR ")));
        }
        PodPick::Labels(terms) => {
            for t in terms {
                let term = format!("{}:{}", lucene_field(&format!("{}{}", f.labels_prefix, t.key)), quote(&t.value));
                parts.push(if t.equal { term } else { format!("NOT {term}") });
            }
        }
        PodPick::All => {}
    }
    if let Some(c) = &scope.container {
        parts.push(format!("{}:{}", lucene_field(f.container), quote(c)));
    }
    parts.join(" AND ")
}

/// The store query for `scopes` (OR-ed) on a source with `cfg`, with an
/// optional plain-text `filter`.
///
/// Loki has one stream selector per query, so several scopes are merged:
/// namespaces and pod names become regex alternations (a pod name that
/// exists in two of the namespaces would match in both — fleets name
/// concrete pods, so this is rare), and label selectors are only allowed
/// for a single scope.
pub fn build_scope_query(cfg: &LogSourceConfig, scopes: &[PodScope], filter: Option<&str>) -> Result<String> {
    let kind = cfg
        .source_kind()
        .ok_or_else(|| CoreError::Other(format!("log source \"{}\" has an unknown kind", cfg.name)))?;
    let preset = cfg
        .k8s_preset()
        .ok_or_else(|| CoreError::Other(format!("log source \"{}\" has an unknown k8s_fields", cfg.name)))?;
    let f = preset.fields();
    if scopes.is_empty() {
        return Err(CoreError::Other("no pods to query".into()));
    }
    let filter = filter.map(str::trim).filter(|t| !t.is_empty());
    match kind {
        LogSourceKind::VictoriaLogs => {
            let scope = if scopes.len() == 1 {
                logsql_scope(&f, &scopes[0])
            } else {
                let parts: Vec<String> = scopes.iter().map(|s| format!("({})", logsql_scope(&f, s))).collect();
                format!("({})", parts.join(" OR "))
            };
            Ok(match filter {
                Some(text) => format!("{scope} {}", quote(text)),
                None => scope,
            })
        }
        LogSourceKind::OpenSearch | LogSourceKind::Elasticsearch => {
            let scope = if scopes.len() == 1 {
                lucene_scope(&f, &scopes[0])
            } else {
                let parts: Vec<String> = scopes.iter().map(|s| format!("({})", lucene_scope(&f, s))).collect();
                format!("({})", parts.join(" OR "))
            };
            Ok(match filter {
                Some(text) => format!("{scope} AND {}", quote(text)),
                None => scope,
            })
        }
        LogSourceKind::Loki => {
            if scopes.len() > 1 && scopes.iter().any(|s| matches!(s.pick, PodPick::Labels(_))) {
                return Err(CoreError::Other("label selectors over several namespaces cannot be one LogQL query".into()));
            }
            let namespaces: BTreeSet<&str> = scopes.iter().map(|s| s.namespace.as_str()).collect();
            let mut matchers = Vec::new();
            let ns_label = loki_label(f.namespace);
            if namespaces.len() == 1 {
                matchers.push(format!("{ns_label}={}", quote(namespaces.iter().next().copied().unwrap_or(""))));
            } else {
                let alt: Vec<String> = namespaces.iter().map(|n| regex_literal(n)).collect();
                matchers.push(format!("{ns_label}=~{}", quote(&alt.join("|"))));
            }
            let pods: BTreeSet<&str> = scopes
                .iter()
                .filter_map(|s| match &s.pick {
                    PodPick::Names(names) => Some(names.iter().map(String::as_str)),
                    _ => None,
                })
                .flatten()
                .collect();
            let all = scopes.iter().any(|s| matches!(s.pick, PodPick::All));
            // One selector cannot say "every pod of A, only web-1 of B":
            // dropping the pod matcher would widen B to all its pods.
            let whole: BTreeSet<&str> = scopes
                .iter()
                .filter(|s| matches!(s.pick, PodPick::All))
                .map(|s| s.namespace.as_str())
                .collect();
            // Pods in a namespace that is wanted whole are already covered.
            let uncovered = scopes
                .iter()
                .any(|s| matches!(s.pick, PodPick::Names(_)) && !whole.contains(s.namespace.as_str()));
            if all && namespaces.len() > 1 && uncovered {
                return Err(CoreError::Other(
                    "whole namespaces and single pods in other namespaces cannot be one LogQL query".into(),
                ));
            }
            if !pods.is_empty() && !all {
                let pod_label = loki_label(f.pod);
                if pods.len() == 1 {
                    matchers.push(format!("{pod_label}={}", quote(pods.iter().next().copied().unwrap_or(""))));
                } else {
                    let alt: Vec<String> = pods.iter().map(|p| regex_literal(p)).collect();
                    matchers.push(format!("{pod_label}=~{}", quote(&alt.join("|"))));
                }
            }
            if let [PodScope { pick: PodPick::Labels(terms), .. }] = scopes {
                for t in terms {
                    let op = if t.equal { "=" } else { "!=" };
                    matchers.push(format!("{}{op}{}", loki_label(&format!("{}{}", f.labels_prefix, t.key)), quote(&t.value)));
                }
            }
            let containers: BTreeSet<&str> = scopes.iter().filter_map(|s| s.container.as_deref()).collect();
            if containers.len() == 1 && scopes.iter().all(|s| s.container.is_some()) {
                matchers.push(format!("{}={}", loki_label(f.container), quote(containers.iter().next().copied().unwrap_or(""))));
            }
            let selector = format!("{{{}}}", matchers.join(", "));
            Ok(match filter {
                Some(text) => format!("{selector} |= {}", quote(text)),
                None => selector,
            })
        }
    }
}

/// The pod field of lines from a source with `cfg`, as the store names it:
/// Loki keys lines by stream label, so a dotted preset field becomes the
/// sanitised label (`kubernetes.pod_name` → `kubernetes_pod_name`).
pub fn pod_field(cfg: &LogSourceConfig) -> String {
    store_field(cfg, cfg.k8s_preset().map(|p| p.fields().pod).unwrap_or("pod"))
}

/// The namespace field of lines from a source with `cfg` (see [`pod_field`]).
pub fn namespace_field(cfg: &LogSourceConfig) -> String {
    store_field(cfg, cfg.k8s_preset().map(|p| p.fields().namespace).unwrap_or("namespace"))
}

fn store_field(cfg: &LogSourceConfig, field: &str) -> String {
    match cfg.source_kind() {
        Some(LogSourceKind::Loki) => loki_label(field),
        _ => field.to_string(),
    }
}

/// A fleet answer folded by pod: one row per expected `namespace/pod`
/// (lines, distinct messages, most frequent message), pods without lines
/// included, and lines from pods outside the fleet counted apart. A line
/// without a namespace field is matched by pod name when only one member has
/// that name.
pub fn fold_by_pod(
    source: &str,
    query: &LogQuery,
    result: &LogResult,
    pod_field: &str,
    namespace_field: &str,
    pods: &[(String, String)],
) -> String {
    let key = |ns: &str, pod: &str| format!("{ns}/{pod}");
    let mut per_pod: BTreeMap<String, Vec<&str>> = pods.iter().map(|(ns, p)| (key(ns, p), Vec::new())).collect();
    let mut other = 0usize;
    for line in &result.lines {
        let Some(pod) = line.fields.get(pod_field) else {
            other += 1;
            continue;
        };
        let member = match line.fields.get(namespace_field) {
            Some(ns) => Some(key(ns, pod)),
            None => {
                let mut same = pods.iter().filter(|(_, p)| p == pod);
                match (same.next(), same.next()) {
                    (Some((ns, p)), None) => Some(key(ns, p)),
                    _ => None,
                }
            }
        };
        match member.and_then(|m| per_pod.get_mut(&m)) {
            Some(lines) => lines.push(line.message.as_str()),
            None => other += 1,
        }
    }
    let mut out = format!(
        "pod_logs_history {source} {} … {} UTC over {} pod(s): {} line(s), limit {}",
        query.start.format("%Y-%m-%d %H:%M:%S"),
        query.end.format("%Y-%m-%d %H:%M:%S"),
        pods.len(),
        result.lines.len(),
        query.limit
    );
    if result.truncated {
        out.push_str(" — TRUNCATED: counts are of the returned lines only; narrow the window or add a filter");
    }
    out.push('\n');
    if let Some(note) = &result.note {
        out.push_str(&format!("Note: {note}\n"));
    }
    out.push_str("pod | lines | distinct | most frequent message\n");
    for (pod, lines) in &per_pod {
        if lines.is_empty() {
            out.push_str(&format!("{pod} | 0 | 0 | —\n"));
            continue;
        }
        let mut groups: BTreeMap<String, (usize, &str)> = BTreeMap::new();
        for message in lines {
            let entry = groups.entry(normalize_message(message)).or_insert((0, message));
            entry.0 += 1;
        }
        let (count, sample) = groups.values().max_by_key(|(n, _)| *n).copied().unwrap_or((0, ""));
        let sample: String = sample.chars().take(200).collect();
        out.push_str(&format!("{pod} | {} | {} | [{count}×] {sample}\n", lines.len(), groups.len()));
    }
    if other > 0 {
        out.push_str(&format!(
            "{other} line(s) came from pods outside the fleet (fields {namespace_field}, {pod_field})\n"
        ));
    }
    out
}

/// The `query_logs` arguments for a built `query` on `source`, carrying the
/// model's window, limit and explanation over from `args`.
pub fn query_logs_arguments(source: &str, query: &str, args: &Value) -> Value {
    let mut out = Map::new();
    out.insert("source".into(), json!(source));
    out.insert("query".into(), json!(query));
    for key in ["since", "start", "end", "limit", "explanation"] {
        if let Some(v) = args.get(key) {
            out.insert(key.into(), v.clone());
        }
    }
    Value::Object(out)
}

/// The `pod_logs_history` tool definition; in a fleet, pods come from the
/// fleet and `pod`/`selector` are not offered.
pub fn pod_logs_history_definition(mode: filar_core::CommandConfirmMode, fleet: bool) -> crate::ToolDef {
    let mut p = Map::new();
    if !fleet {
        p.insert("pod".into(), json!({"type": "string", "description": "Pod name (deleted pods too). Use this or selector."}));
        p.insert("selector".into(), json!({"type": "string", "description": "Equality label selector, e.g. app=web,tier!=batch."}));
        p.insert("namespace".into(), json!({"type": "string", "description": "Namespace; defaults to the tab's."}));
    }
    p.insert("container".into(), json!({"type": "string"}));
    p.insert("filter".into(), json!({"type": "string", "description": "Optional plain text the lines must contain."}));
    p.insert("since".into(), json!({"type": "string", "description": "Window ending now, e.g. 15m, 1h."}));
    p.insert("start".into(), json!({"type": "string", "description": "Window start, RFC 3339."}));
    p.insert("end".into(), json!({"type": "string", "description": "Window end, RFC 3339; defaults to now."}));
    p.insert("limit".into(), json!({"type": "integer", "minimum": 1}));
    p.insert("source".into(), json!({"type": "string", "description": "Log source; needed only when several are linked to the cluster."}));
    p.insert("explanation".into(), json!({"type": "string"}));
    let mut required: Vec<&str> = Vec::new();
    if mode == filar_core::CommandConfirmMode::Explain {
        required.push("explanation");
    }
    let description = if fleet {
        "History of every pod of this fleet from the centralised log store linked to their clusters: one query for all pods, answered as a per-pod table (lines, distinct messages, most frequent message; pods without lines included). A window is mandatory."
    } else {
        "History of a pod (or pods by label selector) from the centralised log store linked to this cluster, for a window, including deleted pods. filar writes the store query; the answer is folded like query_logs. A window is mandatory."
    };
    crate::ToolDef {
        name: TOOL_POD_LOGS_HISTORY.into(),
        description: description.into(),
        parameters: json!({"type": "object", "properties": p, "required": required, "additionalProperties": false}),
    }
}

/// The scope of a single-cluster call from its arguments and the tab's
/// default namespace and pod.
pub fn scope_from_arguments(args: &Value, default_namespace: Option<&str>, default_pod: Option<&str>) -> Result<PodScope> {
    let text = |key: &str| args.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    let namespace = text("namespace")
        .or(default_namespace)
        .ok_or_else(|| CoreError::Other("namespace is required (the tab has no default namespace)".into()))?;
    let pick = match (text("pod"), text("selector")) {
        (Some(_), Some(_)) => return Err(CoreError::Other("give either pod or selector, not both".into())),
        (Some(pod), None) => PodPick::Names(vec![pod.to_string()]),
        (None, Some(selector)) => PodPick::Labels(parse_selector(selector)?),
        (None, None) => match default_pod {
            Some(pod) => PodPick::Names(vec![pod.to_string()]),
            None => return Err(CoreError::Other("pod or selector is required".into())),
        },
    };
    Ok(PodScope {
        namespace: namespace.to_string(),
        pick,
        container: text("container").map(str::to_owned),
    })
}

/// A fleet member as `pod_logs_history` needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetLogMember {
    /// `[[kube_clusters]]` name.
    pub cluster: String,
    /// Namespace.
    pub namespace: String,
    /// Pod, when the member is a pod (not a whole namespace).
    pub pod: Option<String>,
}

/// Answer when a cluster has no linked source.
pub fn no_linked_source(cluster: &str) -> String {
    format!(
        "Для кластера {cluster} не настроен источник логов: pod_logs_history недоступен. \
         Чтобы включить, добавьте clusters = [\"{cluster}\"] в нужный [[log_sources]] в config.toml. \
         (No log source is linked to cluster {cluster}; use k8s_logs for the recent tail.)"
    )
}

impl super::LogSources {
    /// Sources whose `clusters` include `cluster`.
    pub fn linked(&self, cluster: &str) -> Vec<String> {
        self.names()
            .into_iter()
            .filter(|name| {
                self.source_config(name)
                    .is_some_and(|cfg| cfg.clusters.iter().any(|c| c.trim() == cluster))
            })
            .collect()
    }

    fn pick_linked(&self, cluster: &str, requested: Option<&str>) -> std::result::Result<String, String> {
        let linked = self.linked(cluster);
        match requested.map(str::trim).filter(|s| !s.is_empty()) {
            Some(name) if linked.iter().any(|l| l == name) => Ok(name.to_string()),
            Some(name) => Err(format!(
                "Error: log source \"{name}\" is not linked to cluster {cluster}; linked: {}",
                if linked.is_empty() { "none".to_string() } else { linked.join(", ") }
            )),
            None => match linked.as_slice() {
                [] => Err(no_linked_source(cluster)),
                [one] => Ok(one.clone()),
                many => Err(format!(
                    "Error: several log sources are linked to cluster {cluster} ({}); pass source",
                    many.join(", ")
                )),
            },
        }
    }

    /// `pod_logs_history` in a cluster or pod tab: build the store query from
    /// the tab's namespace (and pod), run it through the `query_logs` frame
    /// and gate, fold like `query_logs`.
    #[allow(clippy::too_many_arguments)]
    pub async fn process_pod_logs_history(
        &self,
        call: &crate::ToolCall,
        cluster: &str,
        default_namespace: Option<&str>,
        default_pod: Option<&str>,
        mode: filar_core::CommandConfirmMode,
        confirmer: &dyn crate::CommandConfirmer,
        emit: &(dyn Fn(crate::AgentEvent) + Send + Sync),
        cancellation: Option<&tokio_util::sync::CancellationToken>,
        proposal_id: u64,
    ) -> Result<crate::ChatMessage> {
        let args = &call.arguments;
        let source = match self.pick_linked(cluster, args.get("source").and_then(Value::as_str)) {
            Ok(source) => source,
            Err(message) => return Ok(crate::ChatMessage::tool(&call.id, message)),
        };
        let built = (|| -> Result<_> {
            let scope = scope_from_arguments(args, default_namespace, default_pod)?;
            let cfg = self
                .source_config(&source)
                .ok_or_else(|| CoreError::Other(format!("unknown log source \"{source}\"")))?;
            let filter = args.get("filter").and_then(Value::as_str);
            let query = build_scope_query(cfg, &[scope], filter)?;
            self.parse_call(&query_logs_arguments(&source, &query, args), chrono::Utc::now())
        })();
        let parsed = match built {
            Ok(parsed) => parsed,
            Err(error) => {
                return Ok(crate::ChatMessage::tool(&call.id, format!("Error: invalid pod_logs_history call: {error}")));
            }
        };
        let display = format!("{TOOL_POD_LOGS_HISTORY} {}", parsed.display_command().trim_start_matches("query_logs "));
        let render = |c: &super::QueryLogsCall, r: &LogResult| self.fold(c, r);
        self.run_framed(call, &parsed, display, &render, mode, confirmer, emit, cancellation, proposal_id)
            .await
    }

    /// `pod_logs_history` over a fleet: one query per linked source with all
    /// the fleet's pods, folded by pod. A fleet always asks once.
    pub async fn process_fleet_pod_logs(
        &self,
        call: &crate::ToolCall,
        members: &[FleetLogMember],
        confirmer: &dyn crate::CommandConfirmer,
        emit: &(dyn Fn(crate::AgentEvent) + Send + Sync),
        cancellation: Option<&tokio_util::sync::CancellationToken>,
        proposal_id: u64,
    ) -> Result<crate::ChatMessage> {
        let args = &call.arguments;
        // Group the members by the source linked to their cluster.
        let mut by_source: BTreeMap<String, Vec<&FleetLogMember>> = BTreeMap::new();
        let mut unlinked: BTreeSet<&str> = BTreeSet::new();
        // Other refusals (several linked sources and no `source`, a `source`
        // not linked to the cluster) keep their own message.
        let mut refused: BTreeMap<String, Vec<&str>> = BTreeMap::new();
        let requested = args.get("source").and_then(Value::as_str);
        for member in members {
            match self.pick_linked(&member.cluster, requested) {
                Ok(source) => by_source.entry(source).or_default().push(member),
                Err(_) if self.linked(&member.cluster).is_empty() => {
                    unlinked.insert(member.cluster.as_str());
                }
                Err(message) => refused.entry(message).or_default().push(member.cluster.as_str()),
            }
        }
        if by_source.is_empty() {
            // Nothing to query: every reason, not just the first.
            let mut reasons: Vec<String> = refused.keys().cloned().collect();
            if !unlinked.is_empty() {
                let clusters: Vec<&str> = unlinked.into_iter().collect();
                reasons.push(no_linked_source(&clusters.join(", ")));
            }
            return Ok(crate::ChatMessage::tool(&call.id, reasons.join("\n")));
        }
        let container = args.get("container").and_then(Value::as_str).map(str::trim).filter(|c| !c.is_empty());
        let filter = args.get("filter").and_then(Value::as_str);
        let mut answers = Vec::new();
        for (source, members) in by_source {
            let scopes: Vec<PodScope> = members
                .iter()
                .map(|m| PodScope {
                    namespace: m.namespace.clone(),
                    pick: m.pod.clone().map(|p| PodPick::Names(vec![p])).unwrap_or(PodPick::All),
                    container: container.map(str::to_owned),
                })
                .collect();
            let pods: Vec<(String, String)> = members
                .iter()
                .filter_map(|m| m.pod.clone().map(|p| (m.namespace.clone(), p)))
                .collect();
            let built = (|| -> Result<_> {
                let cfg = self
                    .source_config(&source)
                    .ok_or_else(|| CoreError::Other(format!("unknown log source \"{source}\"")))?;
                let query = build_scope_query(cfg, &scopes, filter)?;
                let parsed = self.parse_call(&query_logs_arguments(&source, &query, args), chrono::Utc::now())?;
                Ok((parsed, pod_field(cfg), namespace_field(cfg)))
            })();
            let (parsed, field, ns_field) = match built {
                Ok(built) => built,
                Err(error) => {
                    answers.push(format!("Error: invalid pod_logs_history call for {source}: {error}"));
                    continue;
                }
            };
            let display = format!(
                "{TOOL_POD_LOGS_HISTORY} [fleet: {} member(s)] {}",
                members.len(),
                parsed.display_command().trim_start_matches("query_logs ")
            );
            let render = |c: &super::QueryLogsCall, r: &LogResult| {
                if pods.is_empty() {
                    self.fold(c, r)
                } else {
                    fold_by_pod(&c.source, &c.query, r, &field, &ns_field, &pods)
                }
            };
            let message = self
                .run_framed(
                    call,
                    &parsed,
                    display,
                    &render,
                    filar_core::CommandConfirmMode::Always,
                    confirmer,
                    emit,
                    cancellation,
                    proposal_id,
                )
                .await?;
            answers.push(message.content);
        }
        if !unlinked.is_empty() {
            let clusters: Vec<&str> = unlinked.into_iter().collect();
            answers.push(format!("No log source is linked to: {}; their pods are not in this answer.", clusters.join(", ")));
        }
        for (message, clusters) in refused {
            let clusters: BTreeSet<&str> = clusters.into_iter().collect();
            let clusters: Vec<&str> = clusters.into_iter().collect();
            answers.push(format!("Pods of {} are not in this answer: {message}", clusters.join(", ")));
        }
        Ok(crate::ChatMessage::tool(&call.id, answers.join("\n")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(kind: &str, preset: Option<&str>) -> LogSourceConfig {
        LogSourceConfig {
            name: "s".into(),
            kind: kind.into(),
            url: "http://x".into(),
            index: matches!(kind, "opensearch" | "elasticsearch").then(|| "logs-*".into()),
            k8s_fields: preset.map(String::from),
            ..Default::default()
        }
    }

    fn pod(ns: &str, name: &str) -> PodScope {
        PodScope {
            namespace: ns.into(),
            pick: PodPick::Names(vec![name.into()]),
            container: None,
        }
    }

    #[test]
    fn selectors_are_equality_terms_only() {
        let terms = parse_selector("app=web, tier!=batch,env==prod").unwrap();
        assert_eq!(terms.len(), 3);
        assert!(terms[0].equal && !terms[1].equal && terms[2].equal);
        assert_eq!(terms[1].value, "batch");
        assert!(parse_selector("app in (web,api)").is_err());
        assert!(parse_selector("app").is_err());
        assert!(parse_selector(" ").is_err());
        assert!(parse_selector("app=").is_err(), "an empty value would match nothing useful");
    }

    #[test]
    fn logsql_for_a_pod_with_container_and_text() {
        let mut scope = pod("shop", "web-1");
        scope.container = Some("app".into());
        let q = build_scope_query(&cfg("victorialogs", None), &[scope], Some("timeout")).unwrap();
        assert_eq!(
            q,
            r#""kubernetes.pod_namespace":="shop" "kubernetes.pod_name":="web-1" "kubernetes.container_name":="app" "timeout""#
        );
    }

    #[test]
    fn values_are_escaped_in_every_language() {
        let evil = PodScope {
            namespace: "shop".into(),
            pick: PodPick::Labels(vec![LabelTerm { key: "app.kubernetes.io/name".into(), value: "we\"b\\x".into(), equal: true }]),
            container: None,
        };
        let vl = build_scope_query(&cfg("victorialogs", None), std::slice::from_ref(&evil), Some("say \"hi\"")).unwrap();
        assert!(vl.contains(r#""kubernetes.pod_labels.app.kubernetes.io/name":="we\"b\\x""#), "{vl}");
        assert!(vl.ends_with(r#""say \"hi\"""#), "{vl}");

        let os = build_scope_query(&cfg("opensearch", None), std::slice::from_ref(&evil), None).unwrap();
        assert_eq!(os, r#"kubernetes.namespace_name:"shop" AND kubernetes.labels.app.kubernetes.io\/name:"we\"b\\x""#);

        let loki = build_scope_query(&cfg("loki", None), &[evil], Some("x")).unwrap();
        assert_eq!(loki, r#"{namespace="shop", app_kubernetes_io_name="we\"b\\x"} |= "x""#);
    }

    #[test]
    fn negated_terms_and_presets() {
        let scope = PodScope {
            namespace: "shop".into(),
            pick: PodPick::Labels(parse_selector("app=web,tier!=batch").unwrap()),
            container: None,
        };
        let fb = build_scope_query(&cfg("elasticsearch", Some("filebeat")), std::slice::from_ref(&scope), None).unwrap();
        assert_eq!(fb, r#"kubernetes.namespace:"shop" AND kubernetes.labels.app:"web" AND NOT kubernetes.labels.tier:"batch""#);
        let vl = build_scope_query(&cfg("victorialogs", None), std::slice::from_ref(&scope), None).unwrap();
        assert!(vl.ends_with(r#"-"kubernetes.pod_labels.tier":="batch""#), "{vl}");
        let loki = build_scope_query(&cfg("loki", None), &[scope], None).unwrap();
        assert_eq!(loki, r#"{namespace="shop", app="web", tier!="batch"}"#);
    }

    #[test]
    fn a_fleet_is_one_query_for_all_its_pods() {
        let scopes = [pod("shop", "web-1"), pod("shop", "web-2"), pod("pay", "api.v2-1")];
        let vl = build_scope_query(&cfg("victorialogs", None), &scopes, Some("error")).unwrap();
        assert_eq!(vl.matches(" OR ").count(), 2, "{vl}");
        assert!(vl.ends_with(" \"error\""), "{vl}");
        let loki = build_scope_query(&cfg("loki", None), &scopes, None).unwrap();
        assert_eq!(loki, r#"{namespace=~"pay|shop", pod=~"api\\.v2-1|web-1|web-2"}"#);
        let os = build_scope_query(&cfg("opensearch", None), &scopes, None).unwrap();
        assert!(os.starts_with("((kubernetes.namespace_name:\"shop\" AND kubernetes.pod_name:(\"web-1\"))"), "{os}");
    }

    #[test]
    fn loki_never_widens_a_mix_of_namespaces_and_pods() {
        let whole = PodScope { namespace: "pay".into(), pick: PodPick::All, container: None };
        let err = build_scope_query(&cfg("loki", None), &[whole.clone(), pod("shop", "web-1")], None).unwrap_err();
        assert!(err.to_string().contains("cannot be one LogQL query"), "{err}");
        // A pod of a namespace that is wanted whole is covered.
        let covered = build_scope_query(
            &cfg("loki", None),
            &[whole.clone(), PodScope { namespace: "shop".into(), ..whole.clone() }, pod("shop", "web-1")],
            None,
        )
        .unwrap();
        assert_eq!(covered, r#"{namespace=~"pay|shop"}"#);
        // In one namespace, every pod of it is exactly what was asked.
        let same = build_scope_query(&cfg("loki", None), &[PodScope { namespace: "shop".into(), ..whole }, pod("shop", "web-1")], None).unwrap();
        assert_eq!(same, r#"{namespace="shop"}"#);
        // Other languages keep each scope apart.
        let vl = build_scope_query(&cfg("victorialogs", None), &[PodScope { namespace: "pay".into(), pick: PodPick::All, container: None }, pod("shop", "web-1")], None).unwrap();
        assert!(vl.contains(r#"("kubernetes.pod_namespace":="pay")"#) && vl.contains(r#""kubernetes.pod_name":="web-1""#), "{vl}");
    }

    #[test]
    fn same_pod_names_in_two_namespaces_stay_apart_and_loki_fields_are_labels() {
        use chrono::TimeZone;
        let line = |ns: &str, pod: &str, msg: &str| super::super::LogLine {
            message: msg.into(),
            fields: [
                ("kubernetes_namespace_name".to_string(), ns.to_string()),
                ("kubernetes_pod_name".to_string(), pod.to_string()),
            ]
            .into(),
            ..Default::default()
        };
        let loki = cfg("loki", Some("fluent-bit"));
        assert_eq!(pod_field(&loki), "kubernetes_pod_name");
        assert_eq!(namespace_field(&loki), "kubernetes_namespace_name");
        assert_eq!(pod_field(&cfg("victorialogs", None)), "kubernetes.pod_name");
        let result = LogResult {
            lines: vec![line("shop", "web-1", "a"), line("pay", "web-1", "b"), line("pay", "web-1", "b")],
            ..Default::default()
        };
        let query = LogQuery {
            query: String::new(),
            start: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
            end: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 11, 0, 0).unwrap(),
            limit: 100,
        };
        let pods = [("shop".to_string(), "web-1".to_string()), ("pay".to_string(), "web-1".to_string())];
        let text = fold_by_pod("loki", &query, &result, &pod_field(&loki), &namespace_field(&loki), &pods);
        assert!(text.contains("shop/web-1 | 1 | 1 | [1×] a"), "{text}");
        assert!(text.contains("pay/web-1 | 2 | 1 | [2×] b"), "{text}");
    }

    #[test]
    fn scope_from_arguments_uses_tab_defaults() {
        let s = scope_from_arguments(&json!({"selector": "app=web"}), Some("shop"), None).unwrap();
        assert_eq!(s.namespace, "shop");
        assert!(matches!(s.pick, PodPick::Labels(_)));
        let s = scope_from_arguments(&json!({}), Some("shop"), Some("web-1")).unwrap();
        assert_eq!(s.pick, PodPick::Names(vec!["web-1".into()]));
        assert!(scope_from_arguments(&json!({}), Some("shop"), None).is_err());
        assert!(scope_from_arguments(&json!({"pod": "a"}), None, None).is_err());
        assert!(scope_from_arguments(&json!({"pod": "a", "selector": "x=y"}), Some("n"), None).is_err());
    }

    #[test]
    fn fold_by_pod_lists_every_pod() {
        use chrono::TimeZone;
        let line = |pod: &str, msg: &str| super::super::LogLine {
            message: msg.into(),
            fields: [("kubernetes.pod_name".to_string(), pod.to_string())].into(),
            ..Default::default()
        };
        let result = LogResult {
            lines: vec![
                line("web-1", "timeout after 30s"),
                line("web-1", "timeout after 31s"),
                line("web-1", "ok"),
                line("web-9", "stray"),
            ],
            ..Default::default()
        };
        let query = LogQuery {
            query: String::new(),
            start: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
            end: chrono::Utc.with_ymd_and_hms(2026, 11, 1, 11, 0, 0).unwrap(),
            limit: 100,
        };
        let pods = [("shop".to_string(), "web-1".to_string()), ("shop".to_string(), "web-2".to_string())];
        let text = fold_by_pod("vlogs", &query, &result, "kubernetes.pod_name", "kubernetes.pod_namespace", &pods);
        assert!(text.contains("shop/web-1 | 3 | 2 | [2×] timeout after 30s"), "{text}");
        assert!(text.contains("shop/web-2 | 0 | 0 | —"), "{text}");
        assert!(text.contains("1 line(s) came from pods outside the fleet"), "{text}");
    }
}
