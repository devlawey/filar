//! `[[log_sources]]` — centralised log stores the agent may query (#579).
//!
//! A log source is an HTTP search API (VictoriaLogs, OpenSearch,
//! Elasticsearch, Loki) that filar queries directly from the user's machine
//! (invariant 5): nothing is installed anywhere and no relay sits between.
//! Only the *name* of the key is stored here (`key_env`) — the key itself
//! lives in the environment or the OS keyring (invariant 3).
//!
//! The limits of a source are the frame filar puts around every query the
//! model makes: a window no longer than `max_window_hours`, at most
//! `max_lines` lines, a byte cap per line and per answer, and a timeout.

use serde::{Deserialize, Serialize};

use crate::error::{CoreError, Result};

/// Default longest query window, hours.
pub const DEFAULT_LOG_MAX_WINDOW_HOURS: u64 = 24;
/// Largest `max_window_hours` accepted: a week.
pub const MAX_LOG_MAX_WINDOW_HOURS: u64 = 168;
/// Default line cap of one query.
pub const DEFAULT_LOG_MAX_LINES: usize = 500;
/// Largest `max_lines` accepted.
pub const MAX_LOG_MAX_LINES: usize = 10_000;
/// Default per-request timeout, seconds.
pub const DEFAULT_LOG_TIMEOUT_SECS: u64 = 20;
/// Largest `timeout_secs` accepted.
pub const MAX_LOG_TIMEOUT_SECS: u64 = 120;
/// Default byte cap of one log line; longer lines are cut and marked.
pub const DEFAULT_LOG_MAX_LINE_BYTES: usize = 4 * 1024;
/// Accepted range of `max_line_bytes`.
pub const LOG_MAX_LINE_BYTES_RANGE: (usize, usize) = (256, 64 * 1024);
/// Default byte cap of one answer; the body is read as a stream and cut there.
pub const DEFAULT_LOG_MAX_RESPONSE_BYTES: usize = 2 * 1024 * 1024;
/// Accepted range of `max_response_bytes`.
pub const LOG_MAX_RESPONSE_BYTES_RANGE: (usize, usize) = (64 * 1024, 32 * 1024 * 1024);

/// What kind of store a source is (`kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogSourceKind {
    /// VictoriaLogs, LogsQL (#580).
    VictoriaLogs,
    /// OpenSearch, `query_string` / query DSL (#581).
    OpenSearch,
    /// Elasticsearch, `query_string` / query DSL (#581).
    Elasticsearch,
    /// Grafana Loki, LogQL (#582).
    Loki,
}

impl LogSourceKind {
    /// Every kind, in the order the docs list them.
    pub const ALL: [LogSourceKind; 4] = [
        LogSourceKind::VictoriaLogs,
        LogSourceKind::OpenSearch,
        LogSourceKind::Elasticsearch,
        LogSourceKind::Loki,
    ];

    /// Parse the `kind` value.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s.trim())
    }

    /// The `kind` value as written in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VictoriaLogs => "victorialogs",
            Self::OpenSearch => "opensearch",
            Self::Elasticsearch => "elasticsearch",
            Self::Loki => "loki",
        }
    }

    /// The query language the model writes for this kind.
    pub fn language(self) -> &'static str {
        match self {
            Self::VictoriaLogs => "LogsQL",
            Self::OpenSearch | Self::Elasticsearch => "query_string / query DSL",
            Self::Loki => "LogQL",
        }
    }

    /// Authentication schemes this kind supports.
    pub fn auths(self) -> &'static [LogAuth] {
        match self {
            Self::Elasticsearch => &[LogAuth::None, LogAuth::Basic, LogAuth::Bearer, LogAuth::ApiKey],
            Self::VictoriaLogs | Self::OpenSearch | Self::Loki => {
                &[LogAuth::None, LogAuth::Basic, LogAuth::Bearer]
            }
        }
    }
}

/// How a source authenticates (`auth`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LogAuth {
    /// No credentials.
    None,
    /// HTTP basic: `user` plus the key as the password.
    Basic,
    /// `Authorization: Bearer <key>`.
    Bearer,
    /// `Authorization: ApiKey <key>` (Elasticsearch).
    ApiKey,
}

impl LogAuth {
    /// Parse the `auth` value.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "none" | "" => Some(Self::None),
            "basic" => Some(Self::Basic),
            "bearer" => Some(Self::Bearer),
            "api-key" => Some(Self::ApiKey),
            _ => None,
        }
    }

    /// The `auth` value as written in the config.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Basic => "basic",
            Self::Bearer => "bearer",
            Self::ApiKey => "api-key",
        }
    }

    /// Whether the scheme needs a key from `key_env`.
    pub fn needs_key(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// One `[[log_sources]]` entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogSourceConfig {
    /// Name the agent and the user refer to the source by; unique.
    #[serde(default)]
    pub name: String,
    /// `victorialogs` | `opensearch` | `elasticsearch` | `loki`.
    #[serde(default)]
    pub kind: String,
    /// Base URL, `http://` or `https://`.
    #[serde(default)]
    pub url: String,
    /// `none` | `basic` | `bearer` | `api-key`.
    #[serde(default = "default_auth")]
    pub auth: String,
    /// Name of the environment variable / OS credential holding the key.
    /// The key itself is never stored in the config.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub key_env: String,
    /// User name for `basic`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    /// Tenant: VictoriaLogs `AccountID:ProjectID`, Loki `X-Scope-OrgID`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Index pattern (OpenSearch / Elasticsearch).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index: Option<String>,
    /// Longest query window, hours (1–168).
    #[serde(default = "default_max_window_hours")]
    pub max_window_hours: u64,
    /// Most lines one query returns (1–10000).
    #[serde(default = "default_max_lines")]
    pub max_lines: usize,
    /// Per-request timeout, seconds (1–120).
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    /// Byte cap of one line (256 B – 64 KiB); longer lines are cut and marked.
    #[serde(default = "default_max_line_bytes")]
    pub max_line_bytes: usize,
    /// Byte cap of one answer (64 KiB – 32 MiB); the body is cut there.
    #[serde(default = "default_max_response_bytes")]
    pub max_response_bytes: usize,
    /// Names of `[[kube_clusters]]` whose logs this source holds (#583).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub clusters: Vec<String>,
    /// Free-form tags: they feed tag policies like a target's tags.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

fn default_auth() -> String {
    "none".into()
}
fn default_max_window_hours() -> u64 {
    DEFAULT_LOG_MAX_WINDOW_HOURS
}
fn default_max_lines() -> usize {
    DEFAULT_LOG_MAX_LINES
}
fn default_timeout_secs() -> u64 {
    DEFAULT_LOG_TIMEOUT_SECS
}
fn default_max_line_bytes() -> usize {
    DEFAULT_LOG_MAX_LINE_BYTES
}
fn default_max_response_bytes() -> usize {
    DEFAULT_LOG_MAX_RESPONSE_BYTES
}

/// Manual `Default` matching the serde defaults.
impl Default for LogSourceConfig {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: String::new(),
            url: String::new(),
            auth: default_auth(),
            key_env: String::new(),
            user: None,
            tenant: None,
            index: None,
            max_window_hours: default_max_window_hours(),
            max_lines: default_max_lines(),
            timeout_secs: default_timeout_secs(),
            max_line_bytes: default_max_line_bytes(),
            max_response_bytes: default_max_response_bytes(),
            clusters: Vec::new(),
            tags: Vec::new(),
        }
    }
}

impl LogSourceConfig {
    /// The parsed `kind`; [`validate`](Self::validate) guarantees it.
    pub fn source_kind(&self) -> Option<LogSourceKind> {
        LogSourceKind::parse(&self.kind)
    }

    /// The parsed `auth`; [`validate`](Self::validate) guarantees it.
    pub fn auth_kind(&self) -> Option<LogAuth> {
        LogAuth::parse(&self.auth)
    }

    /// Reject entries that cannot work or would surprise. Every error names
    /// the source and the field — never a key value (there is none here).
    pub fn validate(&self) -> Result<()> {
        let name = self.name.trim();
        let err = |msg: String| Err(CoreError::Config(format!("log_sources: \"{name}\": {msg}")));
        if name.is_empty() {
            return Err(CoreError::Config("log_sources: name must not be empty".into()));
        }
        if name.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return err("name must not contain whitespace".into());
        }
        let Some(kind) = self.source_kind() else {
            let all: Vec<&str> = LogSourceKind::ALL.iter().map(|k| k.as_str()).collect();
            return err(format!("kind \"{}\" is not one of {}", self.kind, all.join(", ")));
        };
        let url = self.url.trim();
        let rest = url
            .strip_prefix("https://")
            .or_else(|| url.strip_prefix("http://"));
        match rest {
            Some(host) if !host.is_empty() && !host.starts_with('/') => {}
            _ => return err("url must start with http:// or https:// and name a host".into()),
        }
        if url.chars().any(|c| c.is_whitespace()) {
            return err("url must not contain whitespace".into());
        }
        if rest.is_some_and(|r| r.split('/').next().is_some_and(|host| host.contains('@'))) {
            return err("url must not carry credentials; use auth and key_env".into());
        }
        let Some(auth) = self.auth_kind() else {
            return err(format!("auth \"{}\" is not one of none, basic, bearer, api-key", self.auth));
        };
        if !kind.auths().contains(&auth) {
            let allowed: Vec<&str> = kind.auths().iter().map(|a| a.as_str()).collect();
            return err(format!(
                "auth \"{}\" is not supported for {}; use {}",
                auth.as_str(),
                kind.as_str(),
                allowed.join(", ")
            ));
        }
        let key_env = self.key_env.trim();
        if auth.needs_key() && key_env.is_empty() {
            return err(format!("auth = \"{}\" needs key_env (the name of the key, not the key)", auth.as_str()));
        }
        if !key_env.is_empty() && !is_key_name(key_env) {
            return err("key_env must be a variable name (letters, digits, _), not a key value".into());
        }
        if auth == LogAuth::Basic && self.user.as_deref().is_none_or(|u| u.trim().is_empty()) {
            return err("auth = \"basic\" needs user".into());
        }
        if let Some(tenant) = self.tenant.as_deref() {
            let tenant = tenant.trim();
            match kind {
                LogSourceKind::VictoriaLogs => {
                    let ok = tenant.split_once(':').is_some_and(|(a, p)| {
                        a.parse::<u32>().is_ok() && p.parse::<u32>().is_ok()
                    });
                    if !ok {
                        return err("tenant must be AccountID:ProjectID, e.g. \"0:0\"".into());
                    }
                }
                LogSourceKind::Loki => {
                    if tenant.is_empty() || tenant.chars().any(|c| c.is_whitespace() || c.is_control()) {
                        return err("tenant must be a non-empty X-Scope-OrgID without spaces".into());
                    }
                }
                LogSourceKind::OpenSearch | LogSourceKind::Elasticsearch => {
                    return err(format!("tenant is not used by {}", kind.as_str()));
                }
            }
        }
        match kind {
            LogSourceKind::OpenSearch | LogSourceKind::Elasticsearch => {
                let index = self.index.as_deref().map(str::trim).unwrap_or("");
                if index.is_empty() {
                    return err(format!("{} needs index (e.g. \"logs-*\")", kind.as_str()));
                }
                if index.chars().any(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#')) {
                    return err("index must be an index name or pattern without spaces, /, ? or #".into());
                }
            }
            LogSourceKind::VictoriaLogs | LogSourceKind::Loki => {
                if self.index.is_some() {
                    return err(format!("index is not used by {}", kind.as_str()));
                }
            }
        }
        if !(1..=MAX_LOG_MAX_WINDOW_HOURS).contains(&self.max_window_hours) {
            return err(format!("max_window_hours must be between 1 and {MAX_LOG_MAX_WINDOW_HOURS}"));
        }
        if !(1..=MAX_LOG_MAX_LINES).contains(&self.max_lines) {
            return err(format!("max_lines must be between 1 and {MAX_LOG_MAX_LINES}"));
        }
        if !(1..=MAX_LOG_TIMEOUT_SECS).contains(&self.timeout_secs) {
            return err(format!("timeout_secs must be between 1 and {MAX_LOG_TIMEOUT_SECS}"));
        }
        let (lo, hi) = LOG_MAX_LINE_BYTES_RANGE;
        if !(lo..=hi).contains(&self.max_line_bytes) {
            return err(format!("max_line_bytes must be between {lo} and {hi}"));
        }
        let (lo, hi) = LOG_MAX_RESPONSE_BYTES_RANGE;
        if !(lo..=hi).contains(&self.max_response_bytes) {
            return err(format!("max_response_bytes must be between {lo} and {hi}"));
        }
        if self.clusters.iter().any(|c| c.trim().is_empty()) {
            return err("clusters entries must not be empty".into());
        }
        Ok(())
    }
}

/// Whether `s` looks like a variable name rather than a pasted key value.
fn is_key_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vlogs() -> LogSourceConfig {
        LogSourceConfig {
            name: "vlogs-prod".into(),
            kind: "victorialogs".into(),
            url: "https://vlogs.example.com".into(),
            ..Default::default()
        }
    }

    fn err(cfg: &LogSourceConfig) -> String {
        cfg.validate().unwrap_err().to_string()
    }

    #[test]
    fn a_minimal_source_is_valid_with_defaults() {
        let cfg = vlogs();
        cfg.validate().unwrap();
        assert_eq!(cfg.max_window_hours, 24);
        assert_eq!(cfg.max_lines, 500);
        assert_eq!(cfg.max_line_bytes, 4096);
        assert_eq!(cfg.max_response_bytes, 2 * 1024 * 1024);
        assert_eq!(cfg.source_kind(), Some(LogSourceKind::VictoriaLogs));
    }

    #[test]
    fn the_url_must_be_http_with_a_host_and_no_credentials() {
        for bad in ["vlogs.example.com", "ftp://x", "https://", "https:///x", "https://u:p@x"] {
            let mut cfg = vlogs();
            cfg.url = bad.into();
            assert!(err(&cfg).contains("url"), "{bad}");
        }
        let mut cfg = vlogs();
        cfg.url = "http://127.0.0.1:9428/".into();
        cfg.validate().unwrap();
    }

    #[test]
    fn kind_and_auth_must_be_known_and_compatible() {
        let mut cfg = vlogs();
        cfg.kind = "splunk".into();
        assert!(err(&cfg).contains("kind \"splunk\""));
        let mut cfg = vlogs();
        cfg.auth = "api-key".into();
        cfg.key_env = "K".into();
        assert!(err(&cfg).contains("not supported for victorialogs"));
        let mut cfg = vlogs();
        cfg.auth = "token".into();
        assert!(err(&cfg).contains("auth \"token\""));
    }

    #[test]
    fn keyed_auth_needs_a_key_name_not_a_key() {
        let mut cfg = vlogs();
        cfg.auth = "bearer".into();
        assert!(err(&cfg).contains("needs key_env"));
        cfg.key_env = "eyJhbGciOi.secret-value".into();
        let e = err(&cfg);
        assert!(e.contains("variable name"), "{e}");
        assert!(!e.contains("secret-value"), "the value must not be echoed: {e}");
        cfg.key_env = "FILAR_LOGS_VLOGS_PROD".into();
        cfg.validate().unwrap();
    }

    #[test]
    fn basic_auth_needs_a_user() {
        let mut cfg = vlogs();
        cfg.auth = "basic".into();
        cfg.key_env = "K".into();
        assert!(err(&cfg).contains("needs user"));
        cfg.user = Some("reader".into());
        cfg.validate().unwrap();
    }

    #[test]
    fn fields_follow_the_kind() {
        let mut cfg = vlogs();
        cfg.tenant = Some("bad".into());
        assert!(err(&cfg).contains("AccountID:ProjectID"));
        cfg.tenant = Some("0:0".into());
        cfg.validate().unwrap();
        cfg.index = Some("logs-*".into());
        assert!(err(&cfg).contains("index is not used"));

        let mut os = LogSourceConfig {
            name: "os".into(),
            kind: "opensearch".into(),
            url: "https://os:9200".into(),
            ..Default::default()
        };
        assert!(err(&os).contains("needs index"));
        os.index = Some("logs-*".into());
        os.validate().unwrap();
        os.tenant = Some("t".into());
        assert!(err(&os).contains("tenant is not used"));

        let loki = LogSourceConfig {
            name: "loki".into(),
            kind: "loki".into(),
            url: "http://loki:3100".into(),
            tenant: Some("team-a".into()),
            ..Default::default()
        };
        loki.validate().unwrap();
    }

    #[test]
    fn limits_stay_in_range() {
        let cases: [(&str, fn(&mut LogSourceConfig)); 6] = [
            ("max_window_hours", |c| c.max_window_hours = 0),
            ("max_window_hours", |c| c.max_window_hours = 24 * 30),
            ("max_lines", |c| c.max_lines = 100_000),
            ("timeout_secs", |c| c.timeout_secs = 0),
            ("max_line_bytes", |c| c.max_line_bytes = 10),
            ("max_response_bytes", |c| c.max_response_bytes = 1 << 30),
        ];
        for (field, set) in cases {
            let mut cfg = vlogs();
            set(&mut cfg);
            assert!(err(&cfg).contains(field), "{field}");
        }
    }

    #[test]
    fn parses_from_toml_with_every_field() {
        let cfg: LogSourceConfig = toml::from_str(
            r#"
name = "vlogs-prod"
kind = "victorialogs"
url = "https://vlogs.example.com"
auth = "bearer"
key_env = "FILAR_LOGS_VLOGS_PROD"
tenant = "0:0"
max_window_hours = 12
max_lines = 200
timeout_secs = 15
clusters = ["prod-eu"]
tags = ["prod"]
"#,
        )
        .unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.auth_kind(), Some(LogAuth::Bearer));
        assert_eq!(cfg.clusters, vec!["prod-eu".to_string()]);
        assert_eq!(cfg.max_lines, 200);
    }
}
