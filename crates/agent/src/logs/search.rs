//! OpenSearch / Elasticsearch adapter (#581): `POST {url}/{index}/_search`.
//!
//! Only `_search`, from a fixed URL template — never `_delete_by_query` or
//! any other endpoint. The body is a `bool` query: the window is a `range`
//! filter on the time field that filar adds itself; the model's text goes
//! into `query_string`, or — when it sent a JSON query DSL object — into
//! `bool.must` as is. Hits are sorted newest first, `size` is the limit,
//! `_source` is limited to the configured `fields`, and `track_total_hits`
//! is capped so a count never scans the whole index.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use filar_core::{CoreError, LogSourceConfig, LogSourceKind, Result};
use serde_json::{json, Value};

use super::{cap_line, read_limited, LogHttp, LogLine, LogQuery, LogResult, LogSource};

/// Highest total hit count the store is asked to compute.
pub const TRACK_TOTAL_HITS_CAP: u64 = 10_000;

/// Document fields tried, in order, for the log message.
const MESSAGE_FIELDS: [&str; 5] = ["message", "log", "msg", "@message", "_msg"];

/// An OpenSearch or Elasticsearch source.
pub struct SearchSource {
    cfg: LogSourceConfig,
    http: LogHttp,
}

impl SearchSource {
    /// Build the adapter for `cfg` with its resolved `key`.
    pub fn new(cfg: &LogSourceConfig, key: &str) -> Result<Self> {
        Ok(Self {
            cfg: cfg.clone(),
            http: LogHttp::new(cfg, key)?,
        })
    }

    /// The only path this adapter calls: `/{index}/_search`.
    pub fn search_path(&self) -> String {
        format!("/{}/_search", self.cfg.index.as_deref().unwrap_or("").trim())
    }
}

/// The `_search` body for `query` on a source with `cfg`.
///
/// A query text that parses as a JSON object is query DSL and goes into
/// `bool.must`; anything else is a `query_string`. A JSON value that is not
/// an object is refused rather than guessed.
pub fn build_body(cfg: &LogSourceConfig, query: &LogQuery) -> Result<Value> {
    let time_field = cfg.time_field_name();
    let text = query.query.trim();
    let must = if text.starts_with('{') {
        match serde_json::from_str::<Value>(text) {
            Ok(Value::Object(dsl)) => {
                // A full request body pasted by habit: take its query.
                match dsl.get("query") {
                    Some(inner @ Value::Object(_)) if dsl.len() == 1 => inner.clone(),
                    _ => Value::Object(dsl),
                }
            }
            Ok(_) => return Err(CoreError::Other("a JSON query must be a query DSL object".into())),
            Err(e) => return Err(CoreError::Other(format!("the query looks like JSON but does not parse: {e}"))),
        }
    } else {
        json!({"query_string": {"query": text}})
    };
    let mut body = json!({
        "size": query.limit,
        "sort": [{ time_field: {"order": "desc"} }],
        "track_total_hits": TRACK_TOTAL_HITS_CAP,
        "query": {"bool": {
            "filter": [{"range": {time_field: {
                "gte": query.start.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "lt": query.end.to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
                "format": "strict_date_optional_time",
            }}}],
            "must": [must],
        }},
    });
    if !cfg.fields.is_empty() {
        let mut source: Vec<String> = cfg.fields.clone();
        for wanted in [time_field.to_string()] {
            if !source.contains(&wanted) {
                source.push(wanted);
            }
        }
        body["_source"] = json!(source);
    }
    Ok(body)
}

fn flatten(prefix: &str, value: &Value, out: &mut BTreeMap<String, String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                let key = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&key, v, out);
            }
        }
        Value::String(s) => {
            out.insert(prefix.to_string(), s.clone());
        }
        Value::Null => {}
        other => {
            out.insert(prefix.to_string(), other.to_string());
        }
    }
}

/// Parse a `_search` answer: each hit's `_source` flattened to dotted
/// fields, the message taken from the first of `message`, `log`, `msg`,
/// `@message`, `_msg`, the time from the configured time field.
pub fn parse_hits(body: &[u8], time_field: &str, limit: usize, max_line_bytes: usize) -> Result<LogResult> {
    let answer: Value = serde_json::from_slice(body)
        .map_err(|e| CoreError::Other(format!("unexpected search answer: {e}")))?;
    let hits = answer
        .pointer("/hits/hits")
        .and_then(Value::as_array)
        .ok_or_else(|| CoreError::Other("search answer has no hits".into()))?;
    let mut lines = Vec::new();
    for hit in hits.iter().take(limit) {
        let mut fields = BTreeMap::new();
        if let Some(source) = hit.get("_source") {
            flatten("", source, &mut fields);
        }
        let time = fields
            .remove(time_field)
            .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
            .map(|t| t.with_timezone(&Utc));
        let message = MESSAGE_FIELDS
            .iter()
            .find_map(|f| fields.remove(*f))
            .unwrap_or_else(|| {
                // No known message field: the document itself, compactly.
                hit.get("_source").map(Value::to_string).unwrap_or_default()
            });
        let (message, truncated) = cap_line(&message, max_line_bytes);
        let fields = fields
            .into_iter()
            .map(|(k, v)| (k, cap_line(&v, max_line_bytes).0))
            .collect();
        lines.push(LogLine { time, message, fields, truncated });
    }
    let total = answer.pointer("/hits/total/value").and_then(Value::as_u64)
        .or_else(|| answer.pointer("/hits/total").and_then(Value::as_u64));
    let relation_gte = answer.pointer("/hits/total/relation").and_then(Value::as_str) == Some("gte");
    let truncated = relation_gte || total.is_some_and(|t| t > lines.len() as u64) || hits.len() > limit;
    let note = match (total, relation_gte) {
        (Some(t), true) => Some(format!("at least {t} documents match")),
        (Some(t), false) if t > lines.len() as u64 => Some(format!("{t} documents match")),
        _ => None,
    };
    Ok(LogResult { lines, truncated, note, ..Default::default() })
}

/// `type: reason` of an OpenSearch / Elasticsearch error reply, root cause
/// first (`parsing_exception`, `index_not_found_exception`, …).
pub fn explain_error(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    let error = value.get("error")?;
    if let Some(text) = error.as_str() {
        return Some(text.to_string());
    }
    let cause = error
        .pointer("/root_cause/0")
        .filter(|c| c.get("reason").is_some())
        .unwrap_or(error);
    let kind = cause.get("type").and_then(Value::as_str).unwrap_or("error");
    let reason = cause.get("reason").and_then(Value::as_str).unwrap_or("");
    let mut text = format!("{kind}: {reason}");
    if let Some(index) = cause.get("index").and_then(Value::as_str) {
        text.push_str(&format!(" (index {index})"));
    }
    Some(text)
}

#[async_trait]
impl LogSource for SearchSource {
    fn config(&self) -> &LogSourceConfig {
        &self.cfg
    }

    fn describe(&self) -> String {
        let store = match self.cfg.source_kind() {
            Some(LogSourceKind::Elasticsearch) => "Elasticsearch",
            _ => "OpenSearch",
        };
        let mut out = format!(
            "{store} index {}: write a query_string (field:value, AND/OR/NOT, \"quoted phrases\", wildcards *) \
             or a JSON query DSL object (it goes into bool.must). The time window and newest-first sort are added by filar \
             on field {}; do not add them.",
            self.cfg.index.as_deref().unwrap_or("?"),
            self.cfg.time_field_name()
        );
        if let Some(preset) = self.cfg.k8s_preset() {
            let f = preset.fields();
            out.push_str(&format!(
                "\nKubernetes fields ({}): pod {}, namespace {}, container {}, labels {}<label>; e.g. {}:shop AND error.",
                preset.as_str(),
                f.pod,
                f.namespace,
                f.container,
                f.labels_prefix,
                f.namespace
            ));
        }
        if !self.cfg.fields.is_empty() {
            out.push_str(&format!("\nOnly these fields are fetched: {}.", self.cfg.fields.join(", ")));
        }
        out
    }

    async fn query(&self, query: &LogQuery) -> Result<LogResult> {
        let limits = self.http.limits;
        let body = build_body(&self.cfg, query)?;
        let request = self
            .http
            .request(reqwest::Method::POST, &self.search_path())
            .json(&body);
        let response = self.http.send_explained(request, explain_error).await?;
        let raw = read_limited(response, limits.max_response_bytes).await?;
        if raw.truncated {
            return Ok(LogResult {
                truncated: true,
                note: Some(format!(
                    "the answer was larger than {} bytes (max_response_bytes) and was dropped; lower the limit or set fields",
                    limits.max_response_bytes
                )),
                ..Default::default()
            });
        }
        parse_hits(&raw.bytes, self.cfg.time_field_name(), query.limit, limits.max_line_bytes)
            .map_err(|e| self.http.error(&e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn cfg() -> LogSourceConfig {
        LogSourceConfig {
            name: "os".into(),
            kind: "opensearch".into(),
            url: "https://os:9200".into(),
            index: Some("logs-*".into()),
            ..Default::default()
        }
    }

    fn query(text: &str) -> LogQuery {
        LogQuery {
            query: text.into(),
            start: Utc.with_ymd_and_hms(2026, 11, 1, 10, 0, 0).unwrap(),
            end: Utc.with_ymd_and_hms(2026, 11, 1, 11, 0, 0).unwrap(),
            limit: 50,
        }
    }

    #[test]
    fn a_text_query_becomes_query_string_inside_the_window() {
        let body = build_body(&cfg(), &query("kubernetes.namespace_name:shop AND error")).unwrap();
        assert_eq!(body["size"], 50);
        assert_eq!(body["sort"][0]["@timestamp"]["order"], "desc");
        assert_eq!(body["track_total_hits"], 10_000);
        let range = &body["query"]["bool"]["filter"][0]["range"]["@timestamp"];
        assert_eq!(range["gte"], "2026-11-01T10:00:00.000Z");
        assert_eq!(range["lt"], "2026-11-01T11:00:00.000Z");
        assert_eq!(body["query"]["bool"]["must"][0]["query_string"]["query"], "kubernetes.namespace_name:shop AND error");
        assert!(body.get("_source").is_none());
    }

    #[test]
    fn a_dsl_query_goes_into_must_and_the_window_is_still_filars() {
        let mut c = cfg();
        c.time_field = Some("ts".into());
        c.fields = vec!["message".into(), "kubernetes.pod_name".into()];
        let dsl = r#"{"match": {"level": "error"}}"#;
        let body = build_body(&c, &query(dsl)).unwrap();
        assert_eq!(body["query"]["bool"]["must"][0], json!({"match": {"level": "error"}}));
        assert!(body["query"]["bool"]["filter"][0]["range"]["ts"].is_object());
        assert_eq!(body["sort"][0]["ts"]["order"], "desc");
        assert_eq!(body["_source"], json!(["message", "kubernetes.pod_name", "ts"]));
        // A whole request body pasted by habit contributes only its query.
        let pasted = build_body(&c, &query(r#"{"query": {"term": {"pod": "web-1"}}}"#)).unwrap();
        assert_eq!(pasted["query"]["bool"]["must"][0], json!({"term": {"pod": "web-1"}}));
        assert!(build_body(&c, &query("{ not json")).unwrap_err().to_string().contains("does not parse"));
        assert!(build_body(&c, &query("[1, 2]")).is_ok(), "not starting with {{ is query_string text");
    }

    #[test]
    fn only_the_search_endpoint_is_used() {
        let source = SearchSource::new(&cfg(), "").unwrap();
        assert_eq!(source.search_path(), "/logs-*/_search");
    }

    #[test]
    fn describe_names_the_preset_fields() {
        let mut c = cfg();
        c.k8s_fields = Some("filebeat".into());
        let text = SearchSource::new(&c, "").unwrap().describe();
        assert!(text.contains("OpenSearch index logs-*"), "{text}");
        assert!(text.contains("pod kubernetes.pod.name, namespace kubernetes.namespace"), "{text}");
        c.k8s_fields = None;
        let text = SearchSource::new(&c, "").unwrap().describe();
        assert!(text.contains("(fluent-bit): pod kubernetes.pod_name, namespace kubernetes.namespace_name"), "{text}");
    }

    #[test]
    fn error_replies_are_explained() {
        let parsing = r#"{"error":{"root_cause":[{"type":"parsing_exception","reason":"unknown query [mtch]","line":1,"col":51}],"type":"x_content_parse_exception","reason":"[1:51] [bool] failed to parse field [must]"},"status":400}"#;
        assert_eq!(explain_error(parsing).unwrap(), "parsing_exception: unknown query [mtch]");
        let missing = r#"{"error":{"root_cause":[{"type":"index_not_found_exception","reason":"no such index [nope]","index":"nope"}],"type":"index_not_found_exception","reason":"no such index [nope]","index":"nope"},"status":404}"#;
        assert_eq!(explain_error(missing).unwrap(), "index_not_found_exception: no such index [nope] (index nope)");
        assert!(explain_error("plain text").is_none());
    }
}
