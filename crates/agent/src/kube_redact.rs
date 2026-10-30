//! Redaction boundary for Kubernetes tool output (#573).
//!
//! `ClusterApi` deliberately returns operational data without Kubernetes Secret
//! objects. That alone is not enough: pod specifications, ConfigMaps and logs
//! commonly contain credentials too. Every structured object or log string must
//! pass through [`KubeToolOutput::redact_for_model`] before it is placed in an
//! agent message, session history or an export.

use std::collections::BTreeMap;

use filar_core::{redact_secrets, KubeCluster, SecretProvider};
use filar_transport::{ConfigMapInfo, PodLogs};
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

impl From<ConfigMapInfo> for KubeToolOutput {
    fn from(info: ConfigMapInfo) -> Self {
        let binary_data = info
            .binary_data_keys
            .into_iter()
            .map(|key| (key, Value::String(HIDDEN_VALUE.to_string())))
            .collect::<Map<_, _>>();
        Self::Object(serde_json::json!({
            "kind": "ConfigMap",
            "metadata": {"name": info.name, "namespace": info.namespace},
            "data": info.data,
            "binaryData": binary_data,
        }))
    }
}

impl From<PodLogs> for KubeToolOutput {
    fn from(logs: PodLogs) -> Self {
        Self::Logs(logs.text)
    }
}

impl KubeToolOutput {
    /// Apply all Kubernetes and known-secret redaction rules. The returned
    /// value is the only form suitable for model messages, history and exports.
    pub fn redact_for_model(self, target: &KubeCluster, secrets: &dyn SecretProvider) -> Self {
        match self {
            Self::Object(mut object) => {
                redact_node(&mut object, target, secrets, None);
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

fn redact_node(
    value: &mut Value,
    target: &KubeCluster,
    secrets: &dyn SecretProvider,
    inherited: Option<&ObjectContext>,
) {
    match value {
        Value::Object(map) => {
            remove_sensitive_metadata(map);
            let context = ObjectContext::of(map, inherited);
            if context.is("ConfigMap") {
                redact_configmap(map, target, secrets, &context);
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
                    "command" | "args" => redact_command(child, secrets),
                    "data" | "binaryData" | "stringData"
                        if context.is("ConfigMap") || context.is("Secret") => {}
                    _ => redact_node(child, target, secrets, Some(&context)),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                redact_node(item, target, secrets, inherited);
            }
        }
        Value::String(text) => {
            *text = redact_bearer_and_urls(&redact_secrets(text, secrets));
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
        let revealed = context.is("Pod")
            && context
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

fn redact_configmap(
    map: &mut Map<String, Value>,
    target: &KubeCluster,
    secrets: &dyn SecretProvider,
    context: &ObjectContext,
) {
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
            } else {
                *value = Value::String(redact_secrets(text, secrets));
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
    if key_is_sensitive(key) || value.chars().count() >= CONFIGMAP_VALUE_MAX_CHARS {
        return true;
    }
    let has_credentials = redact_bearer_and_urls(value) != value;
    has_credentials
        || has_high_entropy(value)
        || embedded_json_is_sensitive(value)
        || has_sensitive_assignment(value)
}

fn has_sensitive_assignment(value: &str) -> bool {
    value.lines().any(|line| {
        let line = line.trim_start();
        let line = line
            .strip_prefix("export")
            .filter(|rest| rest.chars().next().is_some_and(char::is_whitespace))
            .map_or(line, |line| line.trim_start());
        let Some(separator) = line.find(|character| matches!(character, '=' | ':')) else {
            return false;
        };
        let key = trim_matching_quotes(line[..separator].trim());
        let assigned_value = line[separator + 1..].trim();
        !key.is_empty() && !assigned_value.is_empty() && key_is_sensitive(key)
    })
}

fn trim_matching_quotes(text: &str) -> &str {
    if text.len() >= 2 {
        let bytes = text.as_bytes();
        if matches!(
            (bytes[0], bytes[text.len() - 1]),
            (b'\'', b'\'') | (b'"', b'"')
        ) {
            return text[1..text.len() - 1].trim();
        }
    }
    text
}

fn key_is_sensitive(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    ["password", "secret", "token", "key", "dsn"]
        .iter()
        .any(|marker| key.contains(marker))
}

fn embedded_json_is_sensitive(value: &str) -> bool {
    serde_json::from_str::<Value>(value)
        .ok()
        .is_some_and(|value| embedded_json_node_is_sensitive(&value))
}

fn embedded_json_node_is_sensitive(value: &Value) -> bool {
    match value {
        Value::Object(map) => map
            .iter()
            .any(|(key, value)| key_is_sensitive(key) || embedded_json_node_is_sensitive(value)),
        Value::Array(items) => items.iter().any(embedded_json_node_is_sensitive),
        Value::String(text) => {
            redact_bearer_and_urls(text) != text.as_str() || has_high_entropy(text)
        }
        _ => false,
    }
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

fn redact_command(value: &mut Value, secrets: &dyn SecretProvider) {
    match value {
        Value::String(command) => *command = redact_secrets(&redact_command_text(command), secrets),
        Value::Array(arguments) => {
            let mut hide_next = false;
            for argument in arguments {
                if hide_next {
                    *argument = Value::String(HIDDEN_VALUE.to_string());
                    hide_next = false;
                    continue;
                }
                if let Some(text) = argument.as_str() {
                    hide_next = is_credential_flag(text);
                    *argument = Value::String(redact_secrets(&redact_command_text(text), secrets));
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
    while let Some((_, name_end)) = find_next_sensitive_assignment(text, searched) {
        let Some(delimiter) = text[name_end..].chars().next() else {
            break;
        };
        let mut value_start = name_end;
        if delimiter == '=' {
            value_start += 1;
        } else if delimiter.is_whitespace() {
            value_start += delimiter.len_utf8();
            while value_start < text.len() {
                let character = text[value_start..].chars().next().unwrap_or(' ');
                if !character.is_whitespace() {
                    break;
                }
                value_start += character.len_utf8();
            }
        } else {
            searched = name_end;
            continue;
        }
        let value_end = command_value_end(text, value_start);
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

fn command_value_end(text: &str, start: usize) -> usize {
    let Some(first) = text[start..].chars().next() else {
        return start;
    };
    if matches!(first, '\'' | '"') {
        let bytes = text.as_bytes();
        let quote = first as u8;
        let mut index = start + 1;
        while index < bytes.len() {
            if bytes[index] == quote {
                let mut backslashes = 0;
                let mut previous = index;
                while previous > start + 1 && bytes[previous - 1] == b'\\' {
                    backslashes += 1;
                    previous -= 1;
                }
                if backslashes % 2 == 0 {
                    return index + 1;
                }
            }
            index += 1;
        }
        // An unterminated quoted value is hidden to the end: exposing a suffix
        // is worse than hiding the rest of a malformed command.
        return text.len();
    }
    text[start..]
        .find(char::is_whitespace)
        .map_or(text.len(), |offset| start + offset)
}

fn is_credential_flag(text: &str) -> bool {
    sensitive_flag_name_end(text, 0)
        .is_some_and(|name_end| text[name_end..].chars().all(char::is_whitespace))
}

fn sensitive_flag_name_end(text: &str, start: usize) -> Option<usize> {
    if !text[start..].starts_with("--") {
        return None;
    }
    let name_start = start + 2;
    let name_end = text[name_start..]
        .find(|character: char| character == '=' || character.is_whitespace())
        .map_or(text.len(), |offset| name_start + offset);
    (name_end > name_start && key_is_sensitive(&text[name_start..name_end])).then_some(name_end)
}

fn find_next_sensitive_assignment(text: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut index = from;
    while index < bytes.len() {
        if bytes[index] == b'-'
            && bytes.get(index + 1) == Some(&b'-')
            && (index == 0 || (!is_identifier_byte(bytes[index - 1]) && bytes[index - 1] != b'-'))
        {
            if let Some(name_end) = sensitive_flag_name_end(text, index) {
                return Some((index, name_end));
            }
        }
        if is_identifier_start(bytes[index]) && is_env_assignment_boundary(bytes, index) {
            let mut name_end = index + 1;
            while name_end < bytes.len() && is_identifier_byte(bytes[name_end]) {
                name_end += 1;
            }
            if bytes.get(name_end) == Some(&b'=') && key_is_sensitive(&text[index..name_end]) {
                return Some((index, name_end));
            }
        }
        index += 1;
    }
    None
}

fn is_identifier_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_env_assignment_boundary(bytes: &[u8], start: usize) -> bool {
    start == 0
        || bytes[start - 1].is_ascii_whitespace()
        || matches!(bytes[start - 1], b';' | b'|' | b'&' | b'(' | b'\'' | b'"')
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
        if userinfo.is_empty() || at + 1 == authority.len() {
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
        redacted_object_with_secrets(object, target, &secrets())
    }

    fn redacted_object_with_secrets(
        object: Value,
        target: &KubeCluster,
        secrets: &dyn SecretProvider,
    ) -> Value {
        match KubeToolOutput::Object(object).redact_for_model(target, secrets) {
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
    fn env_reveal_does_not_apply_to_a_deployment_pod_template() {
        let mut target = target();
        target.reveal_env = vec!["shop/api/PUBLIC_MODE".into()];
        let object = redacted_object(
            json!({
                "kind": "Deployment",
                "metadata": {"name": "api", "namespace": "shop"},
                "spec": {"template": {"spec": {"containers": [{"env": [
                    {"name": "PUBLIC_MODE", "value": "readonly"}
                ]}]}}}
            }),
            &target,
        );
        assert_eq!(
            object["spec"]["template"]["spec"]["containers"][0]["env"][0]["value"],
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
    fn configmap_embedded_json_credentials_hide_the_whole_document() {
        let object = redacted_object(
            json!({
                "kind": "ConfigMap",
                "metadata": {"name": "settings", "namespace": "shop"},
                "data": {
                    "app.json": "{\"database\":{\"password\":\"hunter2\"}}",
                    "ordinary.json": "{\"host\":\"db.internal\",\"port\":5432}"
                }
            }),
            &target(),
        );
        assert_eq!(object["data"]["app.json"], HIDDEN_VALUE);
        assert_eq!(
            object["data"]["ordinary.json"],
            "{\"host\":\"db.internal\",\"port\":5432}"
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
    fn separated_credential_flags_hide_the_following_value() {
        let object = redacted_object(
            json!({
                "kind": "Pod",
                "metadata": {"name": "api", "namespace": "shop"},
                "spec": {"containers": [{
                    "command": "start --password hunter2 --token   abc123 --password=\"my secret\" --token 'other secret' --verbose",
                    "args": ["--password", "hunter2", "--token", "abc123", "--verbose"]
                }]}
            }),
            &target(),
        );
        let container = &object["spec"]["containers"][0];
        assert_eq!(
            container["command"],
            format!("start --password {HIDDEN_VALUE} --token   {HIDDEN_VALUE} --password={HIDDEN_VALUE} --token {HIDDEN_VALUE} --verbose")
        );
        assert_eq!(
            container["args"],
            json!([
                "--password",
                HIDDEN_VALUE,
                "--token",
                HIDDEN_VALUE,
                "--verbose"
            ])
        );
        assert!(!object.to_string().contains("hunter2"));
        assert!(!object.to_string().contains("abc123"));
        assert!(!object.to_string().contains("my secret"));
        assert!(!object.to_string().contains("other secret"));
    }

    #[test]
    fn configmap_multiline_sensitive_assignments_hide_the_whole_value() {
        let object = redacted_object(
            json!({
                "kind": "ConfigMap",
                "metadata": {"name": "settings", "namespace": "shop"},
                "data": {
                    "app.properties": "db.host=localhost\ndb.password=hunter2",
                    "app.yaml": "database:\n  host: localhost\n  password: hunter2",
                    ".env": "export MODE=prod\nexport  DB_PASSWORD=x\nexport\tAPI_TOKEN=y",
                    "ordinary.yaml": "database:\n  host: localhost\n  port: 5432"
                }
            }),
            &target(),
        );
        for key in ["app.properties", "app.yaml", ".env"] {
            assert_eq!(object["data"][key], HIDDEN_VALUE, "{key}");
        }
        assert_eq!(
            object["data"]["ordinary.yaml"],
            "database:\n  host: localhost\n  port: 5432"
        );
    }

    #[test]
    fn credential_flag_names_and_sensitive_assignments_are_masked() {
        let object = redacted_object(
            json!({
                "kind": "Pod",
                "metadata": {"name": "api", "namespace": "shop"},
                "spec": {"containers": [{
                    "command": "DB_PASSWORD=x HOST=db.internal ./app --db-password=hunter2 --api-key abc --client-secret='two words'",
                    "args": ["--api-token", "abc123", "--db-password=hunter2", "--verbose"]
                }]}
            }),
            &target(),
        );
        let container = &object["spec"]["containers"][0];
        assert_eq!(
            container["command"],
            format!("DB_PASSWORD={HIDDEN_VALUE} HOST=db.internal ./app --db-password={HIDDEN_VALUE} --api-key {HIDDEN_VALUE} --client-secret={HIDDEN_VALUE}")
        );
        assert_eq!(
            container["args"],
            json!([
                "--api-token",
                HIDDEN_VALUE,
                format!("--db-password={HIDDEN_VALUE}"),
                "--verbose"
            ])
        );
        let serialized = object.to_string();
        for secret in ["hunter2", "abc123", "abc", "two words", "DB_PASSWORD=x"] {
            assert!(
                !serialized.contains(secret),
                "{secret} leaked: {serialized}"
            );
        }
        assert!(serialized.contains("HOST=db.internal"));
    }

    #[test]
    fn command_value_end_handles_quoted_edge_cases() {
        let cases = [
            (r#""a\"b" tail"#, r#""a\"b""#),
            (r#""a\\" tail"#, r#""a\\""#),
            (r#""a\\\"b" tail"#, r#""a\\\"b""#),
            ("\"abc", "\"abc"),
            ("\"\" tail", "\"\""),
            ("\"it's fine\" tail", "\"it's fine\""),
            ("'say \"hi\"' tail", "'say \"hi\"'"),
        ];
        for (input, expected_prefix) in cases {
            let end = command_value_end(input, 0);
            assert_eq!(&input[..end], expected_prefix, "{input}");
        }
    }

    #[test]
    fn structured_objects_mask_known_secrets_without_overriding_reveals() {
        let secrets = StaticSecretProvider::new();
        secrets.insert("$FILAR_SECRET_1", "hunter2");
        let mut target = target();
        target.reveal_configmaps = vec!["shop/revealed".into()];
        let object = redacted_object_with_secrets(
            json!({
                "kind": "Pod",
                "metadata": {
                    "name": "api",
                    "namespace": "shop",
                    "annotations": {"example.com/credential": "hunter2"}
                },
                "spec": {"description": "password=hunter2", "args": ["hunter2"]}
            }),
            &target,
            &secrets,
        );
        assert!(!object.to_string().contains("hunter2"));
        let configmap = redacted_object_with_secrets(
            json!({
                "kind": "ConfigMap",
                "metadata": {"name": "settings", "namespace": "shop"},
                "data": {"ordinary": "hunter2"}
            }),
            &target,
            &secrets,
        );
        assert!(!configmap.to_string().contains("hunter2"));
        let revealed = redacted_object_with_secrets(
            json!({
                "kind": "ConfigMap",
                "metadata": {"name": "revealed", "namespace": "shop"},
                "data": {"ordinary": "hunter2"}
            }),
            &target,
            &secrets,
        );
        assert_eq!(
            revealed["data"]["ordinary"],
            format!("hunter2 {REVEALED_BY_CONFIG}")
        );
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
    fn colon_less_url_userinfo_is_masked() {
        let output = redact_logs("https://access-token@service.example/health", &secrets());
        assert_eq!(
            output,
            format!("https://{HIDDEN_VALUE}@service.example/health")
        );
        assert!(!output.contains("access-token"));
    }

    #[test]
    fn ordinary_hosts_ports_flags_and_urls_are_not_masked() {
        let input = "host=db.internal port=5432 flags=--verbose,--dry-run \
                     https://db.internal:8443/health";
        assert_eq!(redact_logs(input, &secrets()), input);
    }

    #[test]
    fn cluster_api_outputs_pass_through_the_agent_tool_boundary() {
        let info = ConfigMapInfo {
            namespace: "shop".into(),
            name: "settings".into(),
            data: BTreeMap::from([
                ("app.json".into(), "{\"password\":\"hunter2\"}".into()),
                ("host".into(), "db.internal".into()),
            ]),
            binary_data_keys: vec!["blob".into()],
            age_seconds: None,
        };
        let output = KubeToolOutput::from(info).redact_for_model(&target(), &secrets());
        let KubeToolOutput::Object(object) = output else {
            unreachable!("ConfigMap stays an object")
        };
        assert_eq!(object["data"]["app.json"], HIDDEN_VALUE);
        assert_eq!(object["data"]["host"], "db.internal");
        assert_eq!(object["binaryData"]["blob"], HIDDEN_VALUE);
        assert!(!object.to_string().contains("hunter2"));

        let logs = PodLogs {
            text: "Bearer abc123 https://token@service.example".into(),
            truncated: false,
        };
        let output = KubeToolOutput::from(logs).redact_for_model(&target(), &secrets());
        let KubeToolOutput::Logs(logs) = output else {
            unreachable!("pod logs stay logs")
        };
        assert!(!logs.contains("abc123"));
        assert!(!logs.contains("token@"));
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
