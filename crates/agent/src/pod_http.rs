//! `pod_http_get` (#578): HTTP diagnostics of a pod through port-forward.
//!
//! Only `GET`, only to a port the container declares or the cluster allows
//! (`diagnostic_ports`), only a relative path. pprof is limited to its index
//! and text profiles (`?debug=1`) of at most 30 s. Prometheus text is shown
//! as the requested metrics or the top samples by value, never the whole
//! `/metrics`. Everything else about the output — redaction, size limits —
//! is shared with the other Kubernetes tools.

use filar_core::{CoreError, Result};
use filar_transport::PodInfo;

/// Tool name.
pub const TOOL_POD_HTTP_GET: &str = "pod_http_get";
/// Body bytes kept from one answer.
pub const MAX_POD_HTTP_BYTES: usize = 1024 * 1024;
/// Deadline of one request, seconds.
pub const POD_HTTP_TIMEOUT_SECS: u64 = 20;
/// Longest pprof `seconds=` accepted.
pub const MAX_PPROF_SECONDS: u64 = 30;
/// Samples shown from `/metrics` when no metric is named.
pub const PROMETHEUS_TOP: usize = 40;

/// The pod port: a number or a container port name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortRef {
    /// A port number.
    Number(u16),
    /// A `ports[].name` of the pod's containers.
    Name(String),
}

impl std::fmt::Display for PortRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PortRef::Number(n) => write!(f, "{n}"),
            PortRef::Name(name) => f.write_str(name),
        }
    }
}

/// Parse the `port` argument: a JSON number or a string (digits → number).
pub fn parse_port(value: &serde_json::Value) -> Result<PortRef> {
    let bad = || CoreError::Other("port must be a port number 1–65535 or a container port name".into());
    match value {
        serde_json::Value::Number(n) => n
            .as_u64()
            .and_then(|n| u16::try_from(n).ok())
            .filter(|n| *n > 0)
            .map(PortRef::Number)
            .ok_or_else(bad),
        serde_json::Value::String(s) => {
            let s = s.trim();
            if let Ok(n) = s.parse::<u16>() {
                return (n > 0).then_some(PortRef::Number(n)).ok_or_else(bad);
            }
            let valid = !s.is_empty()
                && s.len() <= 15
                && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            valid.then(|| PortRef::Name(s.to_string())).ok_or_else(bad)
        }
        _ => Err(bad()),
    }
}

/// Check a relative path with optional query: it starts with `/`, carries no
/// scheme or host, no whitespace or control characters, and pprof stays
/// within the index and text profiles of at most [`MAX_PPROF_SECONDS`].
pub fn validate_path(path: &str) -> Result<String> {
    let path = path.trim();
    let refuse = |why: &str| Err(CoreError::Other(format!("pod_http_get path refused: {why}")));
    if !path.starts_with('/') || path.starts_with("//") {
        return refuse("it must be a relative path starting with `/`, without scheme or host");
    }
    if path.contains("://") {
        return refuse("a URL with a scheme is not a path");
    }
    if path.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return refuse("whitespace and control characters are not allowed");
    }
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    if route.starts_with("/debug/pprof") {
        let index = route == "/debug/pprof" || route == "/debug/pprof/";
        let params: Vec<(&str, &str)> = query
            .split('&')
            .filter(|p| !p.is_empty())
            .map(|p| p.split_once('=').unwrap_or((p, "")))
            .collect();
        let debug_text = params.iter().any(|(k, v)| *k == "debug" && *v == "1");
        if !index && !debug_text {
            return refuse("pprof profiles are fetched only as text (`?debug=1`); binary profiles are not requested");
        }
        if let Some((_, seconds)) = params.iter().find(|(k, _)| *k == "seconds") {
            match seconds.parse::<u64>() {
                Ok(s) if s <= MAX_PPROF_SECONDS => {}
                _ => return refuse("pprof `seconds` must be at most 30"),
            }
        }
    }
    Ok(path.to_string())
}

/// The port number for `port` in `pod`: declared by a container (by number
/// or name), or listed in the cluster's `diagnostic_ports`.
pub fn resolve_port(pod: &PodInfo, port: &PortRef, diagnostic_ports: &[u16]) -> Result<u16> {
    let declared = pod.containers.iter().flat_map(|c| c.ports.iter());
    match port {
        PortRef::Number(n) => {
            if declared.clone().any(|p| p.port == *n) || diagnostic_ports.contains(n) {
                Ok(*n)
            } else {
                Err(CoreError::Other(format!(
                    "port {n} is neither declared by pod {} nor listed in this cluster's diagnostic_ports",
                    pod.name
                )))
            }
        }
        PortRef::Name(name) => declared
            .clone()
            .find(|p| p.name.as_deref() == Some(name.as_str()))
            .map(|p| p.port)
            .ok_or_else(|| {
                CoreError::Other(format!("pod {} declares no port named {name}", pod.name))
            }),
    }
}

/// Whether an answer is Prometheus text exposition.
pub fn is_prometheus(path: &str, content_type: Option<&str>) -> bool {
    content_type.is_some_and(|ct| ct.contains("version=0.0.4") || ct.contains("openmetrics"))
        || path.split('?').next().is_some_and(|p| p.ends_with("/metrics"))
}

/// Mask label values whose name says they hold a secret
/// (`password="…"` → `password="***"`), in Prometheus text.
pub fn redact_sensitive_labels(text: &str) -> String {
    const SENSITIVE: [&str; 5] = ["password", "passwd", "secret", "token", "apikey"];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(eq) = rest.find("=\"") {
        let key_start = rest[..eq]
            .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let key = rest[key_start..eq].to_ascii_lowercase();
        out.push_str(&rest[..eq + 2]);
        rest = &rest[eq + 2..];
        // The closing quote, skipping `\"` (and `\\`) escapes.
        let mut end = rest.len();
        let mut escaped = false;
        for (i, c) in rest.char_indices() {
            match c {
                '\\' if !escaped => escaped = true,
                '"' if !escaped => {
                    end = i;
                    break;
                }
                _ => escaped = false,
            }
        }
        if SENSITIVE.iter().any(|s| key.replace('_', "").contains(s)) {
            out.push_str("***");
        } else {
            out.push_str(&rest[..end]);
        }
        rest = &rest[end..];
    }
    out.push_str(rest);
    out
}

/// Prometheus text for the model: comments dropped; the named metrics (by
/// name prefix) when `metrics` is set, else the [`PROMETHEUS_TOP`] samples
/// with the largest values. A header line says how much was left out.
pub fn prometheus_view(text: &str, metrics: &[String]) -> String {
    let samples: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .collect();
    let total = samples.len();
    let name_of = |line: &str| -> String {
        line.split(|c: char| c == '{' || c.is_whitespace())
            .next()
            .unwrap_or("")
            .to_string()
    };
    if !metrics.is_empty() {
        let shown: Vec<&str> = samples
            .iter()
            .copied()
            .filter(|l| {
                let name = name_of(l);
                metrics.iter().any(|m| name == *m || name.starts_with(&format!("{m}_")))
            })
            .collect();
        return format!(
            "Prometheus metrics: {} of {total} samples matching {}\n{}",
            shown.len(),
            metrics.join(", "),
            shown.join("\n")
        );
    }
    let value_of = |line: &str| -> f64 {
        let after_labels = match line.rfind('}') {
            Some(i) => &line[i + 1..],
            None => line.split_once(char::is_whitespace).map_or("", |(_, v)| v),
        };
        after_labels
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .unwrap_or(f64::MIN)
    };
    let mut ranked = samples.clone();
    ranked.sort_by(|a, b| value_of(b).total_cmp(&value_of(a)));
    ranked.truncate(PROMETHEUS_TOP);
    format!(
        "Prometheus metrics: top {} of {total} samples by value (name metrics to see specific ones)\n{}",
        ranked.len(),
        ranked.join("\n")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use filar_transport::{ContainerInfo, ContainerPortInfo};
    use serde_json::json;

    fn pod() -> PodInfo {
        PodInfo {
            namespace: "shop".into(),
            name: "web-1".into(),
            phase: "Running".into(),
            ready: true,
            ready_containers: 1,
            container_count: 1,
            restarts: 0,
            last_termination: None,
            node: None,
            age_seconds: None,
            containers: vec![ContainerInfo {
                name: "main".into(),
                image: "busybox".into(),
                resource_requests: Default::default(),
                resource_limits: Default::default(),
                ports: vec![ContainerPortInfo {
                    name: Some("http".into()),
                    port: 8080,
                }],
            }],
        }
    }

    #[test]
    fn ports_parse_as_numbers_or_names() {
        assert_eq!(parse_port(&json!(8080)).unwrap(), PortRef::Number(8080));
        assert_eq!(parse_port(&json!("8080")).unwrap(), PortRef::Number(8080));
        assert_eq!(parse_port(&json!("http")).unwrap(), PortRef::Name("http".into()));
        assert!(parse_port(&json!(0)).is_err());
        assert!(parse_port(&json!(70000)).is_err());
        assert!(parse_port(&json!("Bad Name")).is_err());
    }

    #[test]
    fn only_declared_or_allowed_ports_are_reachable() {
        let pod = pod();
        assert_eq!(resolve_port(&pod, &PortRef::Number(8080), &[]).unwrap(), 8080);
        assert_eq!(resolve_port(&pod, &PortRef::Name("http".into()), &[]).unwrap(), 8080);
        assert_eq!(resolve_port(&pod, &PortRef::Number(6060), &[6060]).unwrap(), 6060);
        let err = resolve_port(&pod, &PortRef::Number(22), &[6060]).unwrap_err().to_string();
        assert!(err.contains("diagnostic_ports"), "{err}");
        assert!(resolve_port(&pod, &PortRef::Name("admin".into()), &[]).is_err());
    }

    #[test]
    fn paths_must_be_relative_and_plain() {
        assert_eq!(validate_path("/metrics").unwrap(), "/metrics");
        assert_eq!(validate_path("/healthz?verbose=1").unwrap(), "/healthz?verbose=1");
        for bad in [
            "metrics",
            "http://evil/metrics",
            "//evil/metrics",
            "/a b",
            "/x\r\nHost: evil",
            "/redirect?to=http://x",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn pprof_only_index_and_short_text_profiles() {
        assert!(validate_path("/debug/pprof/").is_ok());
        assert!(validate_path("/debug/pprof/goroutine?debug=1").is_ok());
        assert!(validate_path("/debug/pprof/profile?debug=1&seconds=30").is_ok());
        assert!(validate_path("/debug/pprof/heap").is_err(), "binary profile");
        assert!(validate_path("/debug/pprof/profile?debug=1&seconds=60").is_err());
        assert!(validate_path("/debug/pprof/profile?seconds=10").is_err());
    }

    #[test]
    fn prometheus_named_metrics_and_top_by_value() {
        let text = "# HELP http_requests_total Requests\n# TYPE http_requests_total counter\n\
                    http_requests_total{code=\"200\"} 1027\nhttp_requests_total{code=\"500\"} 3\n\
                    go_goroutines 42\nprocess_resident_memory_bytes 1.5e+08\n";
        let named = prometheus_view(text, &["http_requests".into(), "go_goroutines".into()]);
        assert!(named.contains("3 of 4 samples"), "{named}");
        assert!(!named.contains("process_resident"), "{named}");
        let top = prometheus_view(text, &[]);
        let first = top.lines().nth(1).unwrap();
        assert!(first.starts_with("process_resident_memory_bytes"), "{top}");
        assert!(!top.contains("# HELP"), "{top}");
    }

    #[test]
    fn sensitive_labels_are_masked() {
        let text = "db_up{user=\"app\",password=\"hunter2\",db_token=\"t0k\"} 1";
        let out = redact_sensitive_labels(text);
        assert_eq!(out, "db_up{user=\"app\",password=\"***\",db_token=\"***\"} 1");
    }

    #[test]
    fn escaped_quotes_do_not_leak_the_rest_of_a_secret_label() {
        let text = r#"db_up{password="a\"b\\",user="app"} 1"#;
        let out = redact_sensitive_labels(text);
        assert_eq!(out, r#"db_up{password="***",user="app"} 1"#);
    }

    #[test]
    fn prometheus_is_detected_by_type_or_path() {
        assert!(is_prometheus("/x", Some("text/plain; version=0.0.4; charset=utf-8")));
        assert!(is_prometheus("/metrics?x=1", None));
        assert!(!is_prometheus("/healthz", Some("text/plain")));
    }
}
