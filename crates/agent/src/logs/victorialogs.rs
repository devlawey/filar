//! VictoriaLogs adapter (#580): LogsQL over `POST /select/logsql/query`.
//!
//! The answer is a stream of JSON lines (`_msg`, `_time`, `_stream`, other
//! fields), read up to the source's byte cap and parsed line by line up to
//! the query's limit. VictoriaLogs returns results unsorted, so `| sort by
//! (_time) desc` is appended unless the query already sorts — then its own
//! order is kept. VictoriaLogs has no auth of its own: `basic`/`bearer` are
//! for a proxy in front of it (vmauth). Closing the connection stops the
//! query on the server, which is what dropping the request does.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use filar_core::{LogSourceConfig, Result};

use super::{cap_line, read_limited, LogHttp, LogLine, LogQuery, LogResult, LogSource};

/// Search endpoint.
pub const VLOGS_QUERY_PATH: &str = "/select/logsql/query";

/// Fields that are VictoriaLogs bookkeeping, not log data.
const SKIPPED_FIELDS: [&str; 1] = ["_stream_id"];

/// A VictoriaLogs source.
pub struct VictoriaLogsSource {
    cfg: LogSourceConfig,
    http: LogHttp,
    account: String,
    project: String,
}

impl VictoriaLogsSource {
    /// Build the adapter for `cfg` with its resolved `key`.
    pub fn new(cfg: &LogSourceConfig, key: &str) -> Result<Self> {
        let (account, project) = cfg
            .tenant
            .as_deref()
            .and_then(|t| t.trim().split_once(':'))
            .map(|(a, p)| (a.to_string(), p.to_string()))
            .unwrap_or_else(|| ("0".into(), "0".into()));
        Ok(Self {
            cfg: cfg.clone(),
            http: LogHttp::new(cfg, key)?,
            account,
            project,
        })
    }
}

/// The query's pipe stages: split on `|` outside quoted strings (`"…"`,
/// `'…'`, `` `…` ``, with `\` escapes), so a `|` inside a phrase or a
/// regex (`_msg:~"error|sort"`) is not a pipe.
fn pipe_stages(query: &str) -> Vec<&str> {
    let mut stages = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut start = 0;
    for (i, c) in query.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match (quote, c) {
            (Some(_), '\\') => escaped = true,
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'' | '`') => quote = Some(c),
            (None, '|') => {
                stages.push(&query[start..i]);
                start = i + 1;
            }
            (None, _) => {}
        }
    }
    stages.push(&query[start..]);
    stages
}

/// Whether a LogsQL query already orders its results with a `sort` (or
/// `order`) pipe.
pub fn sorts_itself(query: &str) -> bool {
    pipe_stages(query).into_iter().skip(1).any(|pipe| {
        let word = pipe.split_whitespace().next().unwrap_or("");
        word.eq_ignore_ascii_case("sort") || word.eq_ignore_ascii_case("order")
    })
}

/// The query sent to the server: the model's, newest first unless it sorts.
pub fn server_query(query: &str) -> String {
    let query = query.trim();
    if sorts_itself(query) {
        query.to_string()
    } else {
        format!("{query} | sort by (_time) desc")
    }
}

/// Parse a JSON-lines answer into at most `limit` lines, each message cut to
/// `max_line_bytes`. A line that is not a JSON object is kept as a message.
/// `body_cut` = the body was cut at the byte cap: its last, partial line is
/// dropped. Returns the lines and whether more existed.
pub fn parse_json_lines(body: &[u8], limit: usize, max_line_bytes: usize, body_cut: bool) -> (Vec<LogLine>, bool) {
    let text = String::from_utf8_lossy(body);
    let mut raw: Vec<&str> = text.split('\n').collect();
    if body_cut {
        raw.pop();
    }
    let mut lines = Vec::new();
    let mut more = body_cut;
    for row in raw.into_iter().map(str::trim).filter(|r| !r.is_empty()) {
        if lines.len() >= limit {
            more = true;
            break;
        }
        lines.push(parse_row(row, max_line_bytes));
    }
    (lines, more)
}

fn parse_row(row: &str, max_line_bytes: usize) -> LogLine {
    let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(row) else {
        let (message, truncated) = cap_line(row, max_line_bytes);
        return LogLine {
            message,
            truncated,
            ..Default::default()
        };
    };
    let mut line = LogLine::default();
    let mut fields = BTreeMap::new();
    for (key, value) in map {
        let text = match value {
            serde_json::Value::String(s) => s,
            other => other.to_string(),
        };
        match key.as_str() {
            "_msg" => {
                let (message, truncated) = cap_line(&text, max_line_bytes);
                line.message = message;
                line.truncated = truncated;
            }
            "_time" => {
                line.time = DateTime::parse_from_rfc3339(&text).ok().map(|t| t.with_timezone(&Utc));
            }
            k if SKIPPED_FIELDS.contains(&k) => {}
            _ => {
                let (value, _) = cap_line(&text, max_line_bytes);
                fields.insert(key, value);
            }
        }
    }
    line.fields = fields;
    line
}

#[async_trait]
impl LogSource for VictoriaLogsSource {
    fn config(&self) -> &LogSourceConfig {
        &self.cfg
    }

    fn describe(&self) -> String {
        "LogsQL basics: words and phrases filter the message (error, \"connection refused\"); field:value (level:error); \
         time is set by filar, do not add _time filters; pipes: | stats count() by (field), | sort by (_time) desc, | limit N, \
         | fields f1, f2. Results are newest first unless your query sorts.\n\
         Kubernetes fields (vlagent defaults): kubernetes.pod_name, kubernetes.pod_namespace, kubernetes.container_name, \
         kubernetes.pod_labels.<label>; e.g. kubernetes.pod_namespace:shop error."
            .into()
    }

    async fn query(&self, query: &LogQuery) -> Result<LogResult> {
        let limits = self.http.limits;
        let form = [
            ("query", server_query(&query.query)),
            ("start", query.start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            ("end", query.end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)),
            ("limit", query.limit.to_string()),
            ("timeout", format!("{}s", self.cfg.timeout_secs)),
        ];
        let request = self
            .http
            .request(reqwest::Method::POST, VLOGS_QUERY_PATH)
            .header("AccountID", &self.account)
            .header("ProjectID", &self.project)
            .form(&form);
        let response = self.http.send(request).await?;
        let body = read_limited(response, limits.max_response_bytes).await?;
        let (lines, more) = parse_json_lines(&body.bytes, query.limit, limits.max_line_bytes, body.truncated);
        let truncated = more || lines.len() >= query.limit;
        let note = body.truncated.then(|| {
            format!("the answer was cut at {} bytes (max_response_bytes)", limits.max_response_bytes)
        });
        Ok(LogResult { lines, truncated, note })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[test]
    fn a_sort_is_added_only_when_the_query_has_none() {
        assert_eq!(server_query("error"), "error | sort by (_time) desc");
        assert_eq!(server_query("error | sort by (level)"), "error | sort by (level)");
        assert_eq!(server_query("x | stats count() by (pod)"), "x | stats count() by (pod) | sort by (_time) desc");
        assert!(!sorts_itself("message:\"sort by\""), "a word in a filter is not a pipe");
        assert!(!sorts_itself(r#"_msg:~"error| sort by (x)""#), "a | inside a regex is not a pipe");
        assert!(!sorts_itself(r#"_msg:"a \" | order" x"#), "escaped quotes stay inside the string");
        assert!(sorts_itself(r#"_msg:~"a|b" | sort by (_time)"#));
        assert_eq!(
            server_query(r#"_msg:~"error|sort""#),
            r#"_msg:~"error|sort" | sort by (_time) desc"#
        );
    }

    #[test]
    fn json_lines_are_parsed_capped_and_limited() {
        let long = "x".repeat(5000);
        let body = format!(
            "{{\"_time\":\"2026-11-01T10:00:01Z\",\"_msg\":\"boom\",\"_stream\":\"{{app=\\\"web\\\"}}\",\"_stream_id\":\"00ab\",\"kubernetes.pod_name\":\"web-1\",\"code\":500}}\n\
             {{\"_time\":\"2026-11-01T10:00:00Z\",\"_msg\":\"{long}\"}}\n\
             not json\n\
             {{\"_msg\":\"third\"}}\n"
        );
        let (lines, more) = parse_json_lines(body.as_bytes(), 3, 4096, false);
        assert_eq!(lines.len(), 3);
        assert!(more, "a fourth line existed");
        assert_eq!(lines[0].message, "boom");
        assert_eq!(lines[0].time.unwrap().to_rfc3339(), "2026-11-01T10:00:01+00:00");
        assert_eq!(lines[0].fields["kubernetes.pod_name"], "web-1");
        assert_eq!(lines[0].fields["code"], "500");
        assert_eq!(lines[0].fields["_stream"], "{app=\"web\"}");
        assert!(!lines[0].fields.contains_key("_stream_id"));
        assert!(lines[1].truncated && lines[1].message.contains("line cut: 5000 bytes"));
        assert_eq!(lines[2].message, "not json");

        // A body cut at the byte cap loses its partial last line.
        let (lines, more) = parse_json_lines(b"{\"_msg\":\"a\"}\n{\"_msg\":\"b", 10, 4096, true);
        assert_eq!(lines.len(), 1);
        assert!(more);
    }

    struct Seen {
        head: String,
        body: String,
    }

    /// Serve one request with `status` and `body`; return what was received.
    async fn serve_once(status: &'static str, body: String) -> (String, tokio::task::JoinHandle<Seen>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = socket.read(&mut buf).await.unwrap();
                data.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&data).to_string();
                if let Some(split) = text.find("\r\n\r\n") {
                    let len = text[..split]
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if data.len() >= split + 4 + len || n == 0 {
                        let reply = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
                        socket.write_all(reply.as_bytes()).await.unwrap();
                        return Seen {
                            head: text[..split].to_string(),
                            body: text[split + 4..].to_string(),
                        };
                    }
                }
            }
        });
        (format!("http://{addr}"), handle)
    }

    fn cfg(url: String) -> LogSourceConfig {
        LogSourceConfig {
            name: "vlogs".into(),
            kind: "victorialogs".into(),
            url,
            auth: "bearer".into(),
            key_env: "FILAR_LOGS_VLOGS".into(),
            tenant: Some("12:34".into()),
            ..Default::default()
        }
    }

    fn query() -> LogQuery {
        use chrono::TimeZone;
        LogQuery {
            query: "kubernetes.pod_namespace:shop error".into(),
            start: Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 11, 1, 11, 0, 0).unwrap(),
            limit: 2,
        }
    }

    fn form_value(body: &str, key: &str) -> String {
        let pair = body.split('&').find(|p| p.starts_with(&format!("{key}="))).unwrap();
        let raw = &pair[key.len() + 1..];
        let raw = raw.replace('+', " ");
        let mut out = Vec::new();
        let bytes = raw.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            if bytes[i] == b'%' {
                out.push(u8::from_str_radix(&raw[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                out.push(bytes[i]);
                i += 1;
            }
        }
        String::from_utf8(out).unwrap()
    }

    #[tokio::test]
    async fn the_request_carries_query_window_limit_tenant_and_auth() {
        let answer = "{\"_time\":\"2026-11-01T10:30:00Z\",\"_msg\":\"e1\"}\n{\"_time\":\"2026-11-01T10:20:00Z\",\"_msg\":\"e2\"}\n".to_string();
        let (url, server) = serve_once("200 OK", answer).await;
        let source = VictoriaLogsSource::new(&cfg(url), "tok-123").unwrap();
        let result = source.query(&query()).await.unwrap();
        let seen = server.await.unwrap();
        assert!(seen.head.starts_with("POST /select/logsql/query HTTP/1.1"), "{}", seen.head);
        let head = seen.head.to_ascii_lowercase();
        assert!(head.contains("accountid: 12"), "{head}");
        assert!(head.contains("projectid: 34"), "{head}");
        assert!(head.contains("authorization: bearer tok-123"), "{head}");
        assert_eq!(form_value(&seen.body, "query"), "kubernetes.pod_namespace:shop error | sort by (_time) desc");
        assert_eq!(form_value(&seen.body, "start"), "2026-11-01T10:00:00Z");
        assert_eq!(form_value(&seen.body, "end"), "2026-11-01T11:00:00Z");
        assert_eq!(form_value(&seen.body, "limit"), "2");
        assert_eq!(form_value(&seen.body, "timeout"), "20s");
        assert_eq!(result.lines.len(), 2);
        assert!(result.truncated, "the limit was reached");
        assert_eq!(result.lines[0].message, "e1");
    }

    #[tokio::test]
    async fn the_default_tenant_is_zero_zero() {
        let (url, server) = serve_once("200 OK", String::new()).await;
        let mut c = cfg(url);
        c.tenant = None;
        c.auth = "none".into();
        c.key_env.clear();
        let result = VictoriaLogsSource::new(&c, "").unwrap().query(&query()).await.unwrap();
        let head = server.await.unwrap().head.to_ascii_lowercase();
        assert!(head.contains("accountid: 0") && head.contains("projectid: 0"), "{head}");
        assert!(!head.contains("authorization"), "{head}");
        assert!(result.lines.is_empty() && !result.truncated);
    }

    #[tokio::test]
    async fn server_errors_reach_the_model_without_the_key() {
        for (status, text) in [
            ("400 Bad Request", "cannot parse query [error | bad]: unknown pipe \"bad\""),
            ("422 Unprocessable Entity", "the time range exceeds -search.maxQueryTimeRange=24h"),
            ("502 Bad Gateway", "upstream rejected Bearer tok-123"),
        ] {
            let (url, server) = serve_once(status, text.to_string()).await;
            let err = VictoriaLogsSource::new(&cfg(url), "tok-123")
                .unwrap()
                .query(&query())
                .await
                .unwrap_err()
                .to_string();
            server.await.unwrap();
            assert!(err.contains(&status[..3]), "{err}");
            assert!(!err.contains("tok-123"), "{err}");
            if !text.contains("tok-123") {
                assert!(err.contains(text), "the server's reason must reach the model: {err}");
            }
        }
    }
}
