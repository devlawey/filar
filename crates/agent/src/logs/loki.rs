//! Grafana Loki adapter (#582): LogQL over `GET /loki/api/v1/query_range`.
//!
//! Log queries answer with `streams`: every stream's lines are collected with
//! the stream's labels as fields and ordered newest first (`direction =
//! backward`). Metric queries (`rate`, `count_over_time`, …) answer with a
//! `matrix`, kept as series and shown as a table. The tenant goes into
//! `X-Scope-OrgID`; auth is none, basic or bearer.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use filar_core::{CoreError, LogSourceConfig, Result};
use serde::Deserialize;

use super::{cap_line, read_limited, LogHttp, LogLine, LogQuery, LogResult, LogSeries, LogSource};

/// Range query endpoint.
pub const LOKI_QUERY_PATH: &str = "/loki/api/v1/query_range";

/// A Loki source.
pub struct LokiSource {
    cfg: LogSourceConfig,
    http: LogHttp,
}

impl LokiSource {
    /// Build the adapter for `cfg` with its resolved `key`.
    pub fn new(cfg: &LogSourceConfig, key: &str) -> Result<Self> {
        Ok(Self {
            cfg: cfg.clone(),
            http: LogHttp::new(cfg, key)?,
        })
    }
}

#[derive(Debug, Deserialize)]
struct Answer {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<String>,
    data: Option<Data>,
}

#[derive(Debug, Deserialize)]
struct Data {
    #[serde(rename = "resultType")]
    result_type: String,
    #[serde(default)]
    result: Vec<serde_json::Value>,
}

fn nanos_to_time(ns: &str) -> Option<DateTime<Utc>> {
    let ns: i64 = ns.trim().parse().ok()?;
    Some(Utc.timestamp_nanos(ns))
}

fn seconds_to_time(value: &serde_json::Value) -> Option<DateTime<Utc>> {
    let secs = value.as_f64().or_else(|| value.as_str()?.parse().ok())?;
    let millis = (secs * 1000.0).round() as i64;
    Utc.timestamp_millis_opt(millis).single()
}

fn labels(value: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    value
        .and_then(serde_json::Value::as_object)
        .map(|map| {
            map.iter()
                .map(|(k, v)| (k.clone(), v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// Parse a `query_range` answer into lines (streams) or series (matrix,
/// vector), lines newest first and at most `limit`, messages cut to
/// `max_line_bytes`.
pub fn parse_answer(body: &[u8], limit: usize, max_line_bytes: usize) -> Result<LogResult> {
    let answer: Answer = serde_json::from_slice(body)
        .map_err(|e| CoreError::Other(format!("unexpected Loki answer: {e}")))?;
    if answer.status != "success" {
        return Err(CoreError::Other(format!(
            "Loki answered {}: {}",
            answer.status,
            answer.error.unwrap_or_default()
        )));
    }
    let data = answer
        .data
        .ok_or_else(|| CoreError::Other("Loki answer has no data".into()))?;
    let mut result = LogResult::default();
    match data.result_type.as_str() {
        "streams" => {
            let mut lines = Vec::new();
            for stream in &data.result {
                let fields = labels(stream.get("stream"));
                for value in stream.get("values").and_then(serde_json::Value::as_array).into_iter().flatten() {
                    let Some(pair) = value.as_array() else { continue };
                    let time = pair.first().and_then(serde_json::Value::as_str).and_then(nanos_to_time);
                    let text = pair.get(1).and_then(serde_json::Value::as_str).unwrap_or("");
                    let (message, truncated) = cap_line(text, max_line_bytes);
                    lines.push(LogLine {
                        time,
                        message,
                        fields: fields.clone(),
                        truncated,
                    });
                }
            }
            lines.sort_by_key(|l| std::cmp::Reverse(l.time));
            result.truncated = lines.len() >= limit;
            lines.truncate(limit);
            result.lines = lines;
        }
        "matrix" | "vector" => {
            for item in &data.result {
                let points_raw: Vec<&serde_json::Value> = match data.result_type.as_str() {
                    "matrix" => item.get("values").and_then(serde_json::Value::as_array).map(|v| v.iter().collect()).unwrap_or_default(),
                    _ => item.get("value").into_iter().collect(),
                };
                let points = points_raw
                    .into_iter()
                    .filter_map(|p| {
                        let p = p.as_array()?;
                        let time = seconds_to_time(p.first()?)?;
                        let value = p.get(1)?.as_str()?.parse::<f64>().ok()?;
                        Some((time, value))
                    })
                    .collect();
                result.series.push(LogSeries {
                    labels: labels(item.get("metric")),
                    points,
                });
            }
        }
        other => {
            return Err(CoreError::Other(format!("Loki result type \"{other}\" is not supported")));
        }
    }
    Ok(result)
}

fn nanos(t: DateTime<Utc>) -> String {
    t.timestamp_nanos_opt()
        .map(|n| n.to_string())
        .unwrap_or_else(|| format!("{}000000000", t.timestamp()))
}

#[async_trait]
impl LogSource for LokiSource {
    fn config(&self) -> &LogSourceConfig {
        &self.cfg
    }

    fn describe(&self) -> String {
        "LogQL: every query starts with a stream selector in braces, e.g. {namespace=\"shop\", pod=~\"web-.*\"}; \
         then line filters |= \"text\", != \"text\", |~ \"regex\"; parsers | json, | logfmt, then label filters (| level=\"error\"). \
         Metric queries: sum by (pod) (count_over_time({namespace=\"shop\"} |= \"error\" [5m])), rate(...[1m]) — answered as series. \
         Time is set by filar. Label names depend on the collector (Promtail/Alloy): commonly namespace, pod, container, app, job; \
         if unsure, start with a broad selector and look at the labels in the answer."
            .into()
    }

    async fn query(&self, query: &LogQuery) -> Result<LogResult> {
        let limits = self.http.limits;
        let params = [
            ("query", query.query.trim().to_string()),
            ("start", nanos(query.start)),
            ("end", nanos(query.end)),
            ("limit", query.limit.to_string()),
            ("direction", "backward".to_string()),
        ];
        let mut request = self.http.request(reqwest::Method::GET, LOKI_QUERY_PATH).query(&params);
        if let Some(tenant) = self.cfg.tenant.as_deref().map(str::trim).filter(|t| !t.is_empty()) {
            request = request.header("X-Scope-OrgID", tenant);
        }
        let response = self.http.send(request).await?;
        let body = read_limited(response, limits.max_response_bytes).await?;
        if body.truncated {
            // A cut JSON document cannot be parsed: say so instead of guessing.
            return Ok(LogResult {
                truncated: true,
                note: Some(format!(
                    "the answer was larger than {} bytes (max_response_bytes) and was dropped; lower the limit or narrow the selector",
                    limits.max_response_bytes
                )),
                ..Default::default()
            });
        }
        parse_answer(&body.bytes, query.limit, limits.max_line_bytes).map_err(|e| self.http.error(&e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    const STREAMS: &str = r#"{"status":"success","data":{"resultType":"streams","result":[
        {"stream":{"namespace":"shop","pod":"web-1"},"values":[["1793557516000000000","error a"],["1793557510000000000","error b"]]},
        {"stream":{"namespace":"shop","pod":"web-2"},"values":[["1793557514000000000","error c"]]}
    ]}}"#;

    #[test]
    fn streams_become_lines_newest_first_with_labels() {
        let result = parse_answer(STREAMS.as_bytes(), 10, 4096).unwrap();
        let messages: Vec<&str> = result.lines.iter().map(|l| l.message.as_str()).collect();
        assert_eq!(messages, ["error a", "error c", "error b"]);
        assert_eq!(result.lines[1].fields["pod"], "web-2");
        assert_eq!(result.lines[0].time.unwrap().timestamp(), 1793557516);
        assert!(!result.truncated);
        let limited = parse_answer(STREAMS.as_bytes(), 2, 4096).unwrap();
        assert_eq!(limited.lines.len(), 2);
        assert!(limited.truncated);
    }

    #[test]
    fn a_matrix_becomes_series() {
        let body = r#"{"status":"success","data":{"resultType":"matrix","result":[
            {"metric":{"pod":"web-1"},"values":[[1793557500,"2"],[1793557560,"5"]]},
            {"metric":{"pod":"web-2"},"values":[[1793557560.5,"0.25"]]}
        ]}}"#;
        let result = parse_answer(body.as_bytes(), 10, 4096).unwrap();
        assert!(result.lines.is_empty());
        assert_eq!(result.series.len(), 2);
        assert_eq!(result.series[0].labels["pod"], "web-1");
        assert_eq!(result.series[0].points[1].1, 5.0);
        assert_eq!(result.series[1].points[0].0.timestamp_millis(), 1793557560500);
        let text = super::super::fold::fold_series(&result.series, false);
        assert!(text.contains("Metric result: 2 series"), "{text}");
        assert!(text.contains("{pod=\"web-1\"} 2 point(s)"), "{text}");
        assert!(text.contains("min 2 max 5 last 5"), "{text}");
    }

    #[test]
    fn long_lines_are_cut_and_bad_answers_are_errors() {
        let long = "y".repeat(9000);
        let body = format!(r#"{{"status":"success","data":{{"resultType":"streams","result":[{{"stream":{{}},"values":[["1","{long}"]]}}]}}}}"#);
        let result = parse_answer(body.as_bytes(), 10, 4096).unwrap();
        assert!(result.lines[0].truncated);
        assert!(parse_answer(b"not json", 10, 4096).is_err());
        assert!(parse_answer(br#"{"status":"error","error":"boom"}"#, 10, 4096)
            .unwrap_err()
            .to_string()
            .contains("boom"));
    }

    async fn serve_once(status: &'static str, body: String) -> (String, tokio::task::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut data = Vec::new();
            let mut buf = [0u8; 4096];
            while !data.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = socket.read(&mut buf).await.unwrap();
                if n == 0 {
                    break;
                }
                data.extend_from_slice(&buf[..n]);
            }
            let reply = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}", body.len());
            socket.write_all(reply.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&data).into_owned()
        });
        (format!("http://{addr}"), handle)
    }

    fn cfg(url: String) -> LogSourceConfig {
        LogSourceConfig {
            name: "loki".into(),
            kind: "loki".into(),
            url,
            auth: "basic".into(),
            user: Some("reader".into()),
            key_env: "FILAR_LOGS_LOKI".into(),
            tenant: Some("team-a".into()),
            ..Default::default()
        }
    }

    fn query() -> LogQuery {
        LogQuery {
            query: "{namespace=\"shop\"} |= \"error\"".into(),
            start: Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 11, 1, 11, 0, 0).unwrap(),
            limit: 100,
        }
    }

    #[tokio::test]
    async fn the_request_carries_logql_nanosecond_window_tenant_and_auth() {
        let (url, server) = serve_once("200 OK", STREAMS.to_string()).await;
        let result = LokiSource::new(&cfg(url), "pw-9").unwrap().query(&query()).await.unwrap();
        let head = server.await.unwrap();
        let first = head.lines().next().unwrap();
        assert!(first.starts_with("GET /loki/api/v1/query_range?"), "{first}");
        assert!(first.contains("query=%7Bnamespace%3D%22shop%22%7D+%7C%3D+%22error%22"), "{first}");
        assert!(first.contains("start=1793527200000000000"), "{first}");
        assert!(first.contains("end=1793530800000000000"), "{first}");
        assert!(first.contains("limit=100") && first.contains("direction=backward"), "{first}");
        let lower = head.to_ascii_lowercase();
        assert!(lower.contains("x-scope-orgid: team-a"), "{head}");
        // reader:pw-9 in base64.
        assert!(lower.contains("authorization: basic cmvhzgvyonb3ltk="), "{head}");
        assert_eq!(result.lines.len(), 3);
    }

    #[tokio::test]
    async fn loki_errors_reach_the_model_without_the_key() {
        for (status, text) in [
            ("400 Bad Request", "parse error at line 1, col 23: syntax error: unexpected $end"),
            ("429 Too Many Requests", "too many outstanding requests"),
            ("500 Internal Server Error", "max entries limit per query exceeded, limit > max_entries_limit (5000 > 1000)"),
        ] {
            let (url, server) = serve_once(status, text.to_string()).await;
            let err = LokiSource::new(&cfg(url), "pw-9").unwrap().query(&query()).await.unwrap_err().to_string();
            server.await.unwrap();
            assert!(err.contains(&status[..3]) && err.contains(text), "{err}");
            assert!(!err.contains("pw-9") && !err.contains("cmVhZGVyOnB3LTk="), "{err}");
        }
    }

    #[tokio::test]
    async fn an_answer_over_the_byte_cap_is_reported_not_parsed() {
        let big = format!(r#"{{"status":"success","data":{{"resultType":"streams","result":[{{"stream":{{}},"values":[["1","{}"]]}}]}}}}"#, "z".repeat(200_000));
        let (url, server) = serve_once("200 OK", big).await;
        let mut c = cfg(url);
        c.max_response_bytes = 64 * 1024;
        let result = LokiSource::new(&c, "pw-9").unwrap().query(&query()).await.unwrap();
        let _ = server.await;
        assert!(result.truncated && result.lines.is_empty());
        assert!(result.note.unwrap().contains("max_response_bytes"));
    }
}
