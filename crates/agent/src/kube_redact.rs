//! Redaction boundary for Kubernetes tool output (#573).
//!
//! `ClusterApi` deliberately returns operational data without Kubernetes Secret
//! objects. That alone is not enough: pod specifications, ConfigMaps and logs
//! commonly contain credentials too. Every structured object or log string must
//! pass through [`KubeToolOutput::redact_for_model`] before it is placed in an
//! agent message, session history or an export.

use std::collections::BTreeMap;

use filar_core::{redact_secrets, KubeCluster, SecretProvider};
use serde_json::{Map, Value};

/// Replacement used wherever a value must not reach the model.
pub const HIDDEN_VALUE: &str = "«скрыто»";
/// Visible suffix attached to values explicitly revealed by cluster config.
pub const REVEALED_BY_CONFIG: &str = "«раскрыто настройкой»";
/// ConfigMap values at or above this length are hidden even without another
/// secret signal. Shorter ordinary values remain useful to the model.
pub const CONFIGMAP_VALUE_MAX_CHARS: usize = 256;

const HIGH_ENTROPY_MIN_CHARS: usize = 20;
const HIGH_ENTROPY_THRESHOLD: f64 = 4.5;
const LAST_APPLIED_ANNOTATION: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Output produced by a Kubernetes agent tool, before it enters the model
/// conversation. This is the single redaction boundary used by the Kubernetes
/// tools added in #574; there is intentionally no Secret variant.
#[derive(Debug, Clone, PartialEq)]
pub enum KubeToolOutput {
    /// A Kubernetes object or a tool-built JSON summary retaining Kubernetes
    /// fields such as `env`, `args`, `metadata` and ConfigMap `data`.
    Object(Value),
    /// Bounded text returned by a pod log request.
    Logs(String),
}

impl KubeToolOutput {
    /// Apply all Kubernetes and known-secret redaction rules. The returned
    /// value is the only form suitable for model messages, history and exports.
    pub fn redact_for_model(self, target: &KubeCluster, secrets: &dyn SecretProvider) -> Self {
        match self {
            Self::Object(mut object) => {
                redact_node(&mut object, target, None);
                Self::Object(object)
            }
            Self::Logs(logs) => Self::Logs(redact_logs(&logs, secrets)),
        }
    }
}

/// Redact a pod log with the same known-secret mechanism as command output,
/// then mask bearer tokens and URL userinfo that are not configured secrets.
pub fn redact_logs(logs: &str, secrets: &dyn SecretProvider) -> String {
    let known = redact_secrets(logs, secrets);
    redact_bearer_and_urls(&known)
}

#[derive(Debug, Clone, Default)]
struct ObjectContext {
    kind: Option<String>,
    namespace: Option<String>,
    name: Option<String>,
}

impl ObjectContext {
    fn of(map: &Map<String, Value>, inherited: Option<&Self>) -> Self {
        let metadata = map.get("metadata").and_then(Value::as_object);
        Self {
            kind: string_field(map, "kind")
                .or_else(|| inherited.and_then(|context| context.kind.clone())),
            namespace: metadata
                .and_then(|metadata| string_field(metadata, "namespace"))
                .or_else(|| inherited.and_then(|context| context.namespace.clone())),
            name: metadata
                .and_then(|metadata| string_field(metadata, "name"))
                .or_else(|| inherited.and_then(|context| context.name.clone())),
        }
    }

    fn is(&self, kind: &str) -> bool {
        self.kind.as_deref() == Some(kind)
    }

    fn configmap_id(&self) -> Option<String> {
        Some(format!(
            "{}/{}",
            self.namespace.as_deref()?,
            self.name.as_deref()?
        ))
    }

    fn env_id(&self, variable: &str) -> Option<String> {
        Some(format!(
            "{}/{}/{}",
            self.namespace.as_deref()?,
            self.name.as_deref()?,
            variable
        ))
    }
}

fn string_field(map: &Map<String, Value>, key: &str) -> Option<String> {
    map.get(key).and_then(Value::as_str).map(str::to_owned)
}

fn redact_node(value: &mut Value, target: &KubeCluster, inherited: Option<&ObjectContext>) {
    match value {
        Value::Object(map) => {
            remove_sensitive_metadata(map);
            let context = ObjectContext::of(map, inherited);
            if context.is("ConfigMap") {
                redact_configmap(map, target, &context);
            } else if context.is("Secret") {
                // Defence in depth only: ClusterApi and the agent tools cannot
                // request Secret objects in the first place.
                hide_payload(map, "data");
                hide_payload(map, "stringData");
            }

            for (key, child) in map.iter_mut() {
                match key.as_str() {
                    "env" => redact_env(child, target, &context),
                    "envFrom" => redact_env_from(child),
                    "command" | "args" => redact_command(child),
                    "data" | "binaryData" | "stringData"
                        if context.is("ConfigMap") || context.is("Secret") => {}
                    _ => redact_node(child, target, Some(&context)),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_node(item, target, inherited);
            }
        }
        Value::String(text) => {
            *text = redact_bearer_and_urls(text);
        }
        _ => {}
    }
}

fn remove_sensitive_metadata(map: &mut Map<String, Value>) {
    let Some(metadata) = map.get_mut("metadata").and_then(Value::as_object_mut) else {
        return;
    };
    metadata.remove("managedFields");
    if let Some(annotations) = metadata
        .get_mut("annotations")
        .and_then(Value::as_object_mut)
    {
        annotations.remove(LAST_APPLIED_ANNOTATION);
    }
}

fn redact_env(value: &mut Value, target: &KubeCluster, context: &ObjectContext) {
    let Some(entries) = value.as_array_mut() else {
        return;
    };
    for entry in entries.iter_mut().filter_map(Value::as_object_mut) {
        let variable = string_field(entry, "name").unwrap_or_default();
        if let Some(value_from) = entry.remove("valueFrom") {
            entry.remove("value");
            entry.insert(
                "valueFrom".to_string(),
                Value::String(value_from_reference(&value_from)),
            );
            continue;
        }
        let Some(literal) = entry.get_mut("value") else {
            continue;
        };
        let revealed = context
            .env_id(&variable)
            .is_some_and(|id| target.reveal_env.iter().any(|allowed| allowed == &id));
        if revealed {
            if let Some(text) = literal.as_str() {
                *literal = Value::String(format!("{text} {REVEALED_BY_CONFIG}"));
            } else {
                *literal = Value::String(HIDDEN_VALUE.to_string());
            }
        } else {
            *literal = Value::String(HIDDEN_VALUE.to_string());
        }
    }
}

fn value_from_reference(value: &Value) -> String {
    let Some(map) = value.as_object() else {
        return HIDDEN_VALUE.to_string();
    };
    for (field, label) in [
        ("secretKeyRef", "secretKeyRef"),
        ("configMapKeyRef", "configMapKeyRef"),
    ] {
        if let Some(reference) = map.get(field).and_then(Value::as_object) {
            let name = string_field(reference, "name").unwrap_or_else(|| "?".into());
            let key = string_field(reference, "key").unwrap_or_else(|| "?".into());
            return format!("{label}: {name}/{key}");
        }
    }
    if let Some(reference) = map.get("fieldRef").and_then(Value::as_object) {
        let path = string_field(reference, "fieldPath").unwrap_or_else(|| "?".into());
        return format!("fieldRef: {path}");
    }
    if let Some(reference) = map.get("resourceFieldRef").and_then(Value::as_object) {
        let resource = string_field(reference, "resource").unwrap_or_else(|| "?".into());
        return format!("resourceFieldRef: {resource}");
    }
    HIDDEN_VALUE.to_string()
}

fn redact_env_from(value: &mut Value) {
    let Some(entries) = value.as_array_mut() else {
        return;
    };
    for entry in entries.iter_mut().filter_map(Value::as_object_mut) {
        for (field, label) in [("secretRef", "secretRef"), ("configMapRef", "configMapRef")] {
            if let Some(reference) = entry.get(field).and_then(Value::as_object) {
                let name = string_field(reference, "name").unwrap_or_else(|| "?".into());
                entry.clear();
                entry.insert(label.to_string(), Value::String(name));
                break;
            }
        }
    }
}

fn redact_configmap(map: &mut Map<String, Value>, target: &KubeCluster, context: &ObjectContext) {
    let revealed = context.configmap_id().is_some_and(|id| {
        target
            .reveal_configmaps
            .iter()
            .any(|allowed| allowed == &id)
    });
    if let Some(data) = map.get_mut("data").and_then(Value::as_object_mut) {
        for (key, value) in data {
            let Some(text) = value.as_str() else {
                *value = Value::String(HIDDEN_VALUE.to_string());
                continue;
            };
            if revealed {
                *value = Value::String(format!("{text} {REVEALED_BY_CONFIG}"));
            } else if configmap_value_is_sensitive(key, text) {
                *value = Value::String(HIDDEN_VALUE.to_string());
            }
        }
    }
    // Binary payloads are not useful model input and are never revealed by the
    // string-data exception.
    hide_payload(map, "binaryData");
}

fn hide_payload(map: &mut Map<String, Value>, field: &str) {
    if let Some(data) = map.get_mut(field).and_then(Value::as_object_mut) {
        for value in data.values_mut() {
            *value = Value::String(HIDDEN_VALUE.to_string());
        }
    }
}

fn configmap_value_is_sensitive(key: &str, value: &str) -> bool {
    let key = key.to_ascii_lowercase();
    if ["password", "secret", "token", "key", "dsn"]
        .iter()
        .any(|marker| key.contains(marker))
    {
        return true;
    }
    if value.chars().count() >= CONFIGMAP_VALUE_MAX_CHARS {
        return true;
    }
    let has_credentials = redact_bearer_and_urls(value) != value;
    has_credentials || has_high_entropy(value)
}

fn has_high_entropy(value: &str) -> bool {
    let length = value.chars().count();
    if length < HIGH_ENTROPY_MIN_CHARS || value.chars().any(char::is_whitespace) {
        return false;
    }
    let mut counts = BTreeMap::<char, usize>::new();
    for character in value.chars() {
        *counts.entry(character).or_default() += 1;
    }
    let entropy = counts.values().fold(0.0, |entropy, count| {
        let probability = *count as f64 / length as f64;
        entropy - probability * probability.log2()
    });
    entropy >= HIGH_ENTROPY_THRESHOLD
}

fn redact_command(value: &mut Value) {
    match value {
        Value::String(command) => *command = redact_command_text(command),
        Value::Array(arguments) => {
            for argument in arguments {
                if let Some(text) = argument.as_str() {
                    *argument = Value::String(redact_command_text(text));
                }
            }
        }
        _ => {}
    }
}

fn redact_command_text(text: &str) -> String {
    let flags = redact_assignment_flags(text);
    redact_bearer_and_urls(&flags)
}

fn redact_bearer_and_urls(text: &str) -> String {
    let bearer = redact_bearer_tokens(text);
    redact_url_credentials(&bearer)
}

fn redact_assignment_flags(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut searched = 0;
    while let Some((start, flag_length)) = find_next_flag(text, searched) {
        let value_start = start + flag_length;
        if start > 0 && text.as_bytes()[start - 1].is_ascii_alphanumeric() {
            searched = value_start;
            continue;
        }
        let value_end = text[value_start..]
            .find(char::is_whitespace)
            .map_or(text.len(), |offset| value_start + offset);
        if value_end == value_start {
            searched = value_start;
            continue;
        }
        output.push_str(&text[copied..value_start]);
        output.push_str(HIDDEN_VALUE);
        copied = value_end;
        searched = value_end;
    }
    output.push_str(&text[copied..]);
    output
}

fn find_next_flag(text: &str, from: usize) -> Option<(usize, usize)> {
    ["--password=", "--token="]
        .iter()
        .filter_map(|flag| {
            find_ascii_case_insensitive(&text[from..], flag)
                .map(|offset| (from + offset, flag.len()))
        })
        .min_by_key(|(start, _)| *start)
}

fn redact_bearer_tokens(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut searched = 0;
    while let Some(offset) = find_ascii_case_insensitive(&text[searched..], "bearer") {
        let start = searched + offset;
        let after_word = start + "bearer".len();
        if start > 0 && is_identifier_byte(text.as_bytes()[start - 1]) {
            searched = after_word;
            continue;
        }
        let mut token_start = after_word;
        while token_start < text.len() && matches!(text.as_bytes()[token_start], b' ' | b'\t') {
            token_start += 1;
        }
        if token_start == after_word {
            searched = after_word;
            continue;
        }
        let token_end = text[token_start..]
            .find(|character: char| !is_bearer_token_character(character))
            .map_or(text.len(), |offset| token_start + offset);
        if token_end == token_start {
            searched = after_word;
            continue;
        }
        output.push_str(&text[copied..token_start]);
        output.push_str(HIDDEN_VALUE);
        copied = token_end;
        searched = token_end;
    }
    output.push_str(&text[copied..]);
    output
}

fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

fn is_bearer_token_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || "-._~+/=".contains(character)
}

fn redact_url_credentials(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut searched = 0;
    while let Some(offset) = text[searched..].find("://") {
        let separator = searched + offset;
        let mut scheme_start = separator;
        while scheme_start > 0 && is_scheme_byte(text.as_bytes()[scheme_start - 1]) {
            scheme_start -= 1;
        }
        if scheme_start == separator
            || !text.as_bytes()[scheme_start].is_ascii_alphabetic()
            || scheme_start < copied
        {
            searched = separator + 3;
            continue;
        }
        let authority_start = separator + 3;
        let authority_end = text[authority_start..]
            .find(|character: char| {
                character.is_whitespace()
                    || matches!(character, '/' | '?' | '#' | '\'' | '"' | '<' | '>' | '\\')
            })
            .map_or(text.len(), |offset| authority_start + offset);
        let authority = &text[authority_start..authority_end];
        let Some(at) = authority.rfind('@') else {
            searched = authority_start;
            continue;
        };
        let userinfo = &authority[..at];
        let Some(colon) = userinfo.find(':') else {
            searched = authority_start;
            continue;
        };
        if colon == 0 || colon + 1 == userinfo.len() || at + 1 == authority.len() {
            searched = authority_start;
            continue;
        }
        output.push_str(&text[copied..authority_start]);
        output.push_str(HIDDEN_VALUE);
        output.push('@');
        copied = authority_start + at + 1;
        searched = authority_end;
    }
    output.push_str(&text[copied..]);
    output
}

fn is_scheme_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.')
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

#[cfg(test)]
mod tests {
    use filar_core::StaticSecretProvider;
    use serde_json::json;

    use super::*;

    fn target() -> KubeCluster {
        KubeCluster {
            name: "prod-eu".into(),
            context: "prod-eu".into(),
            ..Default::default()
        }
    }

    fn secrets() -> StaticSecretProvider {
        StaticSecretProvider::new()
    }

    fn redacted_object(object: Value, target: &KubeCluster) -> Value {
        match KubeToolOutput::Object(object).redact_for_model(target, &secrets()) {
            KubeToolOutput::Object(object) => object,
            KubeToolOutput::Logs(_) => unreachable!("object stays an object"),
        }
    }

    #[test]
    fn env_values_are_hidden_and_value_from_is_only_a_reference() {
        let object = redacted_object(
            json!({
                "kind": "Pod",
                "metadata": {"name": "api", "namespace": "shop"},
                "spec": {"containers": [{
                    "env": [
                        {"name": "DB_PASSWORD", "value": "hunter2"},
                        {"name": "DB_FROM_SECRET", "valueFrom": {
                            "secretKeyRef": {"name": "db-creds", "key": "password"}
                        }},
                        {"name": "POD_NAME", "valueFrom": {
                            "fieldRef": {"fieldPath": "metadata.name"}
                        }}
                    ],
                    "envFrom": [
                        {"secretRef": {"name": "db-creds"}, "prefix": "DB_"},
                        {"configMapRef": {"name": "app-config"}}
                    ]
                }]}
            }),
            &target(),
        );
        let container = &object["spec"]["containers"][0];
        assert_eq!(container["env"][0]["value"], HIDDEN_VALUE);
        assert_eq!(
            container["env"][1]["valueFrom"],
            "secretKeyRef: db-creds/password"
        );
        assert_eq!(container["env"][2]["valueFrom"], "fieldRef: metadata.name");
        assert_eq!(container["envFrom"][0], json!({"secretRef": "db-creds"}));
        assert_eq!(
            container["envFrom"][1],
            json!({"configMapRef": "app-config"})
        );
        let serialized = object.to_string();
        assert!(!serialized.contains("hunter2"));
        assert!(!serialized.contains("\"prefix\""));
    }

    #[test]
    fn env_reveal_is_scoped_and_visibly_marked() {
        let mut target = target();
        target.reveal_env = vec!["shop/api/PUBLIC_MODE".into()];
        let pod = || {
            json!({
                "kind": "Pod",
                "metadata": {"name": "api", "namespace": "shop"},
                "spec": {"containers": [{"env": [
                    {"name": "PUBLIC_MODE", "value": "readonly"},
                    {"name": "DB_PASSWORD", "value": "hunter2"}
                ]}]}
            })
        };
        let object = redacted_object(pod(), &target);
        assert_eq!(
            object["spec"]["containers"][0]["env"][0]["value"],
            format!("readonly {REVEALED_BY_CONFIG}")
        );
        assert_eq!(
            object["spec"]["containers"][0]["env"][1]["value"],
            HIDDEN_VALUE
        );
        target.reveal_env = vec!["shop/other/PUBLIC_MODE".into()];
        let object = redacted_object(pod(), &target);
        assert_eq!(
            object["spec"]["containers"][0]["env"][0]["value"],
            HIDDEN_VALUE
        );
    }

    #[test]
    fn last_applied_and_managed_fields_are_removed() {
        let object = redacted_object(
            json!({
                "kind": "Pod",
                "metadata": {
                    "name": "api",
                    "namespace": "shop",
                    "managedFields": [{"manager": "kubectl"}],
                    "annotations": {
                        LAST_APPLIED_ANNOTATION: "{\"secret\":\"hunter2\"}",
                        "safe.example/owner": "platform"
                    }
                }
            }),
            &target(),
        );
        assert!(object["metadata"].get("managedFields").is_none());
        assert_eq!(
            object["metadata"]["annotations"],
            json!({"safe.example/owner": "platform"})
        );
        assert!(!object.to_string().contains("hunter2"));
    }

    #[test]
    fn configmap_key_heuristics_hide_values_but_keep_keys() {
        let object = redacted_object(
            json!({
                "kind": "ConfigMap",
                "metadata": {"name": "settings", "namespace": "shop"},
                "data": {
                    "db.password": "short",
                    "api_secret": "x",
                    "auth.token": "x",
                    "tls.key": "x",
                    "database.dsn": "x",
                    "port": "5432",
                    "host": "db.internal"
                }
            }),
            &target(),
        );
        for key in [
            "db.password",
            "api_secret",
            "auth.token",
            "tls.key",
            "database.dsn",
        ] {
            assert_eq!(object["data"][key], HIDDEN_VALUE, "{key}");
        }
        assert_eq!(object["data"]["port"], "5432");
        assert_eq!(object["data"]["host"], "db.internal");
    }

    #[test]
    fn configmap_url_credentials_entropy_and_length_are_hidden() {
        let random = "a9F3kQ7zX2vB8nM4rT6yU1pL5sD0gH";
        let object = redacted_object(
            json!({
                "kind": "ConfigMap",
                "metadata": {"name": "settings", "namespace": "shop"},
                "data": {
                    "database.url": "postgres://app:hunter2@db.internal/shop",
                    "random": random,
                    "long": "x".repeat(CONFIGMAP_VALUE_MAX_CHARS),
                    "home.url": "https://db.internal:5432",
                    "ordinary": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                }
            }),
            &target(),
        );
        assert_eq!(object["data"]["database.url"], HIDDEN_VALUE);
        assert_eq!(object["data"]["random"], HIDDEN_VALUE);
        assert_eq!(object["data"]["long"], HIDDEN_VALUE);
        assert_eq!(object["data"]["home.url"], "https://db.internal:5432");
        assert_eq!(
            object["data"]["ordinary"],
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
    }

    #[test]
    fn configmap_reveal_is_explicit_and_marked() {
        let mut target = target();
        target.reveal_configmaps = vec!["shop/feature-flags".into()];
        let object = redacted_object(
            json!({
                "kind": "ConfigMap",
                "metadata": {"name": "feature-flags", "namespace": "shop"},
                "data": {"api_token": "intentionally-visible"},
                "binaryData": {"blob": "aHVudGVyMg=="}
            }),
            &target,
        );
        assert_eq!(
            object["data"]["api_token"],
            format!("intentionally-visible {REVEALED_BY_CONFIG}")
        );
        assert_eq!(object["binaryData"]["blob"], HIDDEN_VALUE);
    }

    #[test]
    fn command_and_argument_secrets_are_masked() {
        let object = redacted_object(
            json!({
                "kind": "Pod",
                "metadata": {"name": "api", "namespace": "shop"},
                "spec": {"containers": [{
                    "command": ["/app", "--password=hunter2"],
                    "args": [
                        "--token=abc123",
                        "postgres://app:dbpass@db.internal/shop",
                        "--port=5432",
                        "--host=db.internal",
                        "--verbose"
                    ]
                }]}
            }),
            &target(),
        );
        let container = &object["spec"]["containers"][0];
        assert_eq!(
            container["command"][1],
            format!("--password={HIDDEN_VALUE}")
        );
        assert_eq!(container["args"][0], format!("--token={HIDDEN_VALUE}"));
        assert_eq!(
            container["args"][1],
            format!("postgres://{HIDDEN_VALUE}@db.internal/shop")
        );
        assert_eq!(container["args"][2], "--port=5432");
        assert_eq!(container["args"][3], "--host=db.internal");
        assert_eq!(container["args"][4], "--verbose");
        let serialized = object.to_string();
        assert!(!serialized.contains("hunter2"));
        assert!(!serialized.contains("abc123"));
        assert!(!serialized.contains("dbpass"));
    }

    #[test]
    fn logs_mask_known_secrets_bearer_tokens_and_url_userinfo() {
        let secrets = StaticSecretProvider::new();
        secrets.insert("$FILAR_SECRET_1", "hunter2");
        let logs = "password=hunter2 Authorization: Bearer abc.def-123 \
                    postgres://app:dbpass@db.internal/shop host=db.internal port=5432";
        let output = KubeToolOutput::Logs(logs.into()).redact_for_model(&target(), &secrets);
        let KubeToolOutput::Logs(output) = output else {
            unreachable!("logs stay logs")
        };
        assert!(output.contains("$FILAR_SECRET_1"));
        assert!(output.contains(&format!("Bearer {HIDDEN_VALUE}")));
        assert!(output.contains(&format!("postgres://{HIDDEN_VALUE}@db.internal/shop")));
        assert!(output.contains("host=db.internal port=5432"));
        for secret in ["hunter2", "abc.def-123", "dbpass"] {
            assert!(!output.contains(secret), "{secret} leaked: {output}");
        }
    }

    #[test]
    fn ordinary_hosts_ports_flags_and_urls_are_not_masked() {
        let input = "host=db.internal port=5432 flags=--verbose,--dry-run \
                     https://db.internal:8443/health";
        assert_eq!(redact_logs(input, &secrets()), input);
    }

    #[test]
    fn secret_payload_is_fail_closed_even_though_api_cannot_request_it() {
        let object = redacted_object(
            json!({
                "kind": "Secret",
                "metadata": {"name": "db-creds", "namespace": "shop"},
                "data": {"password": "aHVudGVyMg=="},
                "stringData": {"token": "abc123"}
            }),
            &target(),
        );
        assert_eq!(object["data"]["password"], HIDDEN_VALUE);
        assert_eq!(object["stringData"]["token"], HIDDEN_VALUE);
    }
}
