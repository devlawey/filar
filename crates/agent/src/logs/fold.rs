//! Folding a log answer for the model (#579).
//!
//! Hundreds of near-identical lines would burn the model's context. Lines
//! whose messages are equal after normalisation — numbers, UUIDs, hex ids
//! and timestamps replaced by placeholders — form one group with a count,
//! the first and last time it was seen and the first and last original
//! message. A coarse distribution over the window shows when things happened.

use std::collections::HashMap;

use chrono::{DateTime, Utc};

use super::{LogLine, LogQuery, LogResult, LogSeries};

/// Series shown from a metric query, largest last value first.
pub const MAX_FOLD_SERIES: usize = 30;
/// Values sampled per series.
const SERIES_SAMPLES: usize = 12;

/// Groups shown, most frequent first.
pub const MAX_FOLD_GROUPS: usize = 40;
/// Buckets of the time distribution.
pub const TIME_BUCKETS: usize = 12;
/// Characters of a message shown per sample.
const SAMPLE_CHARS: usize = 400;
/// Fields shown with a group's sample.
const SAMPLE_FIELDS: usize = 6;

/// Replace the variable parts of `message` with placeholders: timestamps
/// `<time>`, UUIDs `<uuid>`, hex ids (8+ hex digits) `<hex>`, numbers `<n>`.
pub fn normalize_message(message: &str) -> String {
    let chars: Vec<char> = message.chars().collect();
    let mut out = String::with_capacity(message.len());
    let mut i = 0;
    while i < chars.len() {
        let at_boundary = i == 0 || !chars[i - 1].is_ascii_alphanumeric();
        if at_boundary {
            if let Some(len) = timestamp_len(&chars[i..]) {
                out.push_str("<time>");
                i += len;
                continue;
            }
            if let Some(len) = uuid_len(&chars[i..]) {
                out.push_str("<uuid>");
                i += len;
                continue;
            }
            if let Some(len) = hex_len(&chars[i..]) {
                out.push_str("<hex>");
                i += len;
                continue;
            }
        }
        if chars[i].is_ascii_digit() {
            while i < chars.len() && (chars[i].is_ascii_digit() || (chars[i] == '.' && chars.get(i + 1).is_some_and(char::is_ascii_digit))) {
                i += 1;
            }
            out.push_str("<n>");
            continue;
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

fn digits(chars: &[char], at: usize, n: usize) -> bool {
    chars.len() >= at + n && chars[at..at + n].iter().all(char::is_ascii_digit)
}

/// `YYYY-MM-DD[T ]HH:MM:SS[.frac][Z|±HH[:]MM]`.
fn timestamp_len(c: &[char]) -> Option<usize> {
    let date = digits(c, 0, 4) && c.get(4) == Some(&'-') && digits(c, 5, 2) && c.get(7) == Some(&'-') && digits(c, 8, 2);
    if !date {
        return None;
    }
    let time = matches!(c.get(10), Some('T' | ' '))
        && digits(c, 11, 2)
        && c.get(13) == Some(&':')
        && digits(c, 14, 2)
        && c.get(16) == Some(&':')
        && digits(c, 17, 2);
    if !time {
        return Some(10);
    }
    let mut i = 19;
    if matches!(c.get(i), Some('.' | ',')) && c.get(i + 1).is_some_and(char::is_ascii_digit) {
        i += 1;
        while c.get(i).is_some_and(char::is_ascii_digit) {
            i += 1;
        }
    }
    match c.get(i) {
        Some('Z') => i += 1,
        Some('+' | '-') if digits(c, i + 1, 2) => {
            i += 3;
            if c.get(i) == Some(&':') && digits(c, i + 1, 2) {
                i += 3;
            } else if digits(c, i, 2) {
                i += 2;
            }
        }
        _ => {}
    }
    Some(i)
}

/// `8-4-4-4-12` hex digits.
fn uuid_len(c: &[char]) -> Option<usize> {
    let mut i = 0;
    for (n, group) in [8, 4, 4, 4, 12].into_iter().enumerate() {
        if n > 0 {
            if c.get(i) != Some(&'-') {
                return None;
            }
            i += 1;
        }
        if c.len() < i + group || !c[i..i + group].iter().all(char::is_ascii_hexdigit) {
            return None;
        }
        i += group;
    }
    (!c.get(i).is_some_and(char::is_ascii_alphanumeric)).then_some(i)
}

/// `0x` and hex digits, or a run of 8+ hex digits with at least one digit
/// and one letter, ending at a word boundary.
fn hex_len(c: &[char]) -> Option<usize> {
    let start = if c.first() == Some(&'0') && matches!(c.get(1), Some('x' | 'X')) { 2 } else { 0 };
    let run = c[start..].iter().take_while(|ch| ch.is_ascii_hexdigit()).count();
    let min = if start == 2 { 1 } else { 8 };
    if run < min || c.get(start + run).is_some_and(char::is_ascii_alphanumeric) {
        return None;
    }
    let body = &c[start..start + run];
    let mixed = body.iter().any(char::is_ascii_digit) && body.iter().any(char::is_ascii_alphabetic);
    (mixed || start == 2).then_some(start + run)
}

struct Group<'a> {
    count: usize,
    first: &'a LogLine,
    last: &'a LogLine,
    order: usize,
}

fn earlier(a: &LogLine, b: &LogLine) -> bool {
    match (a.time, b.time) {
        (Some(a), Some(b)) => a < b,
        _ => false,
    }
}

fn clock(time: Option<DateTime<Utc>>, multi_day: bool) -> String {
    match time {
        Some(t) if multi_day => t.format("%m-%d %H:%M:%S").to_string(),
        Some(t) => t.format("%H:%M:%S").to_string(),
        None => "—".into(),
    }
}

fn sample(text: &str) -> String {
    let flat: String = text.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    if flat.chars().count() <= SAMPLE_CHARS {
        return flat;
    }
    let mut cut: String = flat.chars().take(SAMPLE_CHARS).collect();
    cut.push('…');
    cut
}

/// The model-facing view of `result` for `query` from source `name`
/// (language `language`): a header, the time distribution and the groups.
pub fn fold_logs(name: &str, language: &str, query: &LogQuery, result: &LogResult) -> String {
    let multi_day = query.end - query.start > chrono::Duration::hours(24);
    let mut out = format!(
        "query_logs {name} ({language}) {} … {} UTC: {} line(s), limit {}",
        query.start.format("%Y-%m-%d %H:%M:%S"),
        query.end.format("%Y-%m-%d %H:%M:%S"),
        result.lines.len(),
        query.limit
    );
    if result.truncated {
        out.push_str(" — TRUNCATED: more lines exist; narrow the query or the window");
    }
    out.push('\n');
    if let Some(note) = &result.note {
        out.push_str(&format!("Note: {note}\n"));
    }
    let cut_lines = result.lines.iter().filter(|l| l.truncated).count();
    if cut_lines > 0 {
        out.push_str(&format!("{cut_lines} line(s) were longer than the per-line cap and are cut.\n"));
    }
    if !result.series.is_empty() {
        out.push_str(&fold_series(&result.series, multi_day));
        if result.lines.is_empty() {
            return out;
        }
    }
    if result.lines.is_empty() {
        out.push_str("No lines matched in this window.\n");
        return out;
    }

    // Time distribution.
    let span = (query.end - query.start).num_milliseconds().max(1);
    let mut buckets = [0usize; TIME_BUCKETS];
    let mut untimed = 0;
    for line in &result.lines {
        match line.time {
            Some(t) if t >= query.start && t < query.end => {
                let offset = (t - query.start).num_milliseconds();
                let index = ((offset as i128 * TIME_BUCKETS as i128) / span as i128) as usize;
                buckets[index.min(TIME_BUCKETS - 1)] += 1;
            }
            _ => untimed += 1,
        }
    }
    let labels: Vec<String> = buckets
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let at = query.start + chrono::Duration::milliseconds(span * i as i64 / TIME_BUCKETS as i64);
            let label = if multi_day { at.format("%m-%d %H:%M") } else { at.format("%H:%M") };
            format!("{label} {n}")
        })
        .collect();
    out.push_str(&format!("Distribution: {}", labels.join(" · ")));
    if untimed > 0 {
        out.push_str(&format!(" · outside/without time {untimed}"));
    }
    out.push('\n');

    // Groups.
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut groups: Vec<Group> = Vec::new();
    for line in &result.lines {
        let key = normalize_message(&line.message);
        match index.get(&key) {
            Some(&g) => {
                let group = &mut groups[g];
                group.count += 1;
                if earlier(line, group.first) {
                    group.first = line;
                }
                if earlier(group.last, line) {
                    group.last = line;
                }
            }
            None => {
                index.insert(key, groups.len());
                groups.push(Group { count: 1, first: line, last: line, order: groups.len() });
            }
        }
    }
    let distinct = groups.len();
    groups.sort_by(|a, b| b.count.cmp(&a.count).then(a.order.cmp(&b.order)));
    out.push_str(&format!(
        "Groups: {distinct} distinct message(s) after normalising numbers, ids and times{}\n",
        if distinct > MAX_FOLD_GROUPS { format!(", top {MAX_FOLD_GROUPS} shown") } else { String::new() }
    ));
    for group in groups.iter().take(MAX_FOLD_GROUPS) {
        let first_time = clock(group.first.time, multi_day);
        if group.count == 1 {
            out.push_str(&format!("[1×] {first_time}  {}\n", sample(&group.first.message)));
        } else {
            out.push_str(&format!(
                "[{}×] {first_time} … {}  first: {}\n",
                group.count,
                clock(group.last.time, multi_day),
                sample(&group.first.message)
            ));
            if group.last.message != group.first.message {
                out.push_str(&format!("      last: {}\n", sample(&group.last.message)));
            }
        }
        if !group.first.fields.is_empty() {
            let fields: Vec<String> = group
                .first
                .fields
                .iter()
                .take(SAMPLE_FIELDS)
                .map(|(k, v)| format!("{k}={}", sample(v)))
                .collect();
            out.push_str(&format!("      fields: {}\n", fields.join(" ")));
        }
    }
    if distinct > MAX_FOLD_GROUPS {
        let rest: usize = groups.iter().skip(MAX_FOLD_GROUPS).map(|g| g.count).sum();
        out.push_str(&format!("… {} more group(s), {rest} line(s)\n", distinct - MAX_FOLD_GROUPS));
    }
    out
}

fn number(v: f64) -> String {
    if v.fract() == 0.0 && v.abs() < 1e15 {
        format!("{}", v as i64)
    } else if v.abs() >= 1.0 {
        format!("{v:.3}")
    } else {
        // Three significant digits, so a small rate never reads as zero.
        let decimals = (-v.abs().log10()).ceil() as usize + 2;
        format!("{v:.decimals$}")
    }
}

/// A metric answer as a table: one row per series with its labels, point
/// count, min/max/last and up to 12 evenly sampled values.
pub fn fold_series(series: &[LogSeries], multi_day: bool) -> String {
    let last = |s: &LogSeries| s.points.last().map(|p| p.1).unwrap_or(f64::MIN);
    let mut ranked: Vec<&LogSeries> = series.iter().collect();
    ranked.sort_by(|a, b| last(b).total_cmp(&last(a)));
    let mut out = format!(
        "Metric result: {} series{}\n",
        series.len(),
        if series.len() > MAX_FOLD_SERIES { format!(", top {MAX_FOLD_SERIES} by last value shown") } else { String::new() }
    );
    for s in ranked.iter().take(MAX_FOLD_SERIES) {
        let labels: Vec<String> = s.labels.iter().map(|(k, v)| format!("{k}=\"{v}\"")).collect();
        let values: Vec<f64> = s.points.iter().map(|p| p.1).collect();
        let min = values.iter().copied().fold(f64::INFINITY, f64::min);
        let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let (Some(first), Some(end)) = (s.points.first(), s.points.last()) else {
            out.push_str(&format!("{{{}}} no points\n", labels.join(", ")));
            continue;
        };
        // Evenly over the whole series, first and newest point included.
        let k = values.len().min(SERIES_SAMPLES);
        let sampled: Vec<String> = (0..k)
            .map(|i| if k == 1 { 0 } else { i * (values.len() - 1) / (k - 1) })
            .map(|i| number(values[i]))
            .collect();
        out.push_str(&format!(
            "{{{}}} {} point(s) {} … {}: min {} max {} last {}; values {}\n",
            labels.join(", "),
            values.len(),
            clock(Some(first.0), multi_day),
            clock(Some(end.0), multi_day),
            number(min),
            number(max),
            number(end.1),
            sampled.join(" ")
        ));
    }
    out
}

#[cfg(test)]
mod series_tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn small_values_keep_significant_digits() {
        assert_eq!(number(0.0004), "0.000400");
        assert_eq!(number(0.25), "0.250");
        assert_eq!(number(2.5), "2.500");
        assert_eq!(number(5.0), "5");
        assert_eq!(number(0.0), "0");
    }

    #[test]
    fn samples_span_the_whole_series() {
        let points: Vec<(DateTime<Utc>, f64)> = (0..13)
            .map(|i| (Utc.timestamp_opt(1_793_557_500 + i * 60, 0).unwrap(), i as f64))
            .collect();
        let text = fold_series(&[LogSeries { labels: Default::default(), points }], false);
        assert!(text.contains("values 0 "), "{text}");
        assert!(text.trim_end().ends_with(" 12"), "the newest point is sampled: {text}");
    }
}
