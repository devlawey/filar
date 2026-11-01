//! A minimal HTTP/1.1 `GET` over one port-forward stream (#578).
//!
//! Only what diagnostics need: one request, `Connection: close`, a status
//! line, headers, and a body that is either `Content-Length`, chunked or
//! read to EOF — capped at `max_bytes`.

use filar_core::{CoreError, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::PodHttpResponse;

/// Largest header block accepted.
const MAX_HEADER_BYTES: usize = 64 * 1024;

/// The request bytes for `path` (already validated: starts with `/`, no
/// whitespace or control characters).
pub(crate) fn request_bytes(path: &str) -> Vec<u8> {
    format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nUser-Agent: filar\r\nAccept: */*\r\nConnection: close\r\n\r\n"
    )
    .into_bytes()
}

/// Send `GET path` on `stream` and read the answer, keeping at most
/// `max_bytes` of body.
pub(crate) async fn get_over<S>(stream: &mut S, path: &str, max_bytes: usize) -> Result<PodHttpResponse>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let io = |e: std::io::Error| CoreError::ConnectionLost(format!("port-forward: {e}"));
    stream.write_all(&request_bytes(path)).await.map_err(io)?;
    stream.flush().await.map_err(io)?;
    // Read until the body is complete or clearly over the cap: headers,
    // then max_bytes of body plus chunk framing slack.
    let budget = MAX_HEADER_BYTES + max_bytes + 64 * 1024;
    let mut raw = Vec::new();
    let mut buf = [0u8; 16 * 1024];
    loop {
        let n = stream.read(&mut buf).await.map_err(io)?;
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
        if raw.len() >= budget || body_complete(&raw) {
            break;
        }
    }
    parse_response(&raw, max_bytes)
}

/// Whether `raw` already holds a whole response with a known length.
fn body_complete(raw: &[u8]) -> bool {
    let Some(end) = header_end(raw) else {
        return false;
    };
    let head = String::from_utf8_lossy(&raw[..end]).to_ascii_lowercase();
    if let Some(len) = header_value(&head, "content-length").and_then(|v| v.parse::<usize>().ok()) {
        return raw.len() - (end + 4) >= len;
    }
    if header_value(&head, "transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        // Walk the chunk framing: a `0\r\n\r\n` inside a chunk's data is
        // not the end.
        return chunked_complete(&raw[end + 4..]);
    }
    false
}

fn header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n")
}

fn header_value<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim() == name).then(|| value.trim())
    })
}

/// Parse a raw response into status, content type and a capped body.
pub(crate) fn parse_response(raw: &[u8], max_bytes: usize) -> Result<PodHttpResponse> {
    let end = header_end(raw).ok_or_else(|| {
        CoreError::Other("the pod answered without a complete HTTP header".into())
    })?;
    let head = String::from_utf8_lossy(&raw[..end]).to_string();
    let lower = head.to_ascii_lowercase();
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| CoreError::Other("the pod's answer is not HTTP".into()))?;
    let content_type = head.lines().skip(1).find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case("content-type")
            .then(|| value.trim().to_string())
    });
    let rest = &raw[end + 4..];
    // A body that stopped short of its own framing before the cap was
    // reached was cut off by the connection: an error, not a short answer.
    let cut_off = |got: usize| {
        CoreError::ConnectionLost(format!(
            "the pod's HTTP answer was cut off after {got} bytes of body"
        ))
    };
    let mut body = if header_value(&lower, "transfer-encoding").is_some_and(|v| v.contains("chunked")) {
        let (body, complete) = dechunk(rest);
        if !complete && body.len() <= max_bytes {
            return Err(cut_off(body.len()));
        }
        body
    } else if let Some(len) =
        header_value(&lower, "content-length").and_then(|v| v.parse::<usize>().ok())
    {
        if rest.len() < len && rest.len() <= max_bytes {
            return Err(cut_off(rest.len()));
        }
        rest[..rest.len().min(len)].to_vec()
    } else {
        rest.to_vec()
    };
    let truncated = body.len() > max_bytes;
    body.truncate(max_bytes);
    Ok(PodHttpResponse {
        status,
        content_type,
        body,
        truncated,
    })
}

/// After the zero-size chunk: the empty line that ends the (optional)
/// trailer section has arrived — `\r\n` at once, or after trailer lines.
fn trailer_complete(rest: &[u8]) -> bool {
    rest.starts_with(b"\r\n") || rest.windows(4).any(|w| w == b"\r\n\r\n")
}

/// Whether the chunk framing in `rest` reaches the terminating zero-size
/// chunk — walked without copying the data, as it runs on every read.
fn chunked_complete(mut rest: &[u8]) -> bool {
    loop {
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            return false;
        };
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let Ok(size) = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
        else {
            return false;
        };
        rest = &rest[line_end + 2..];
        if size == 0 {
            return trailer_complete(rest);
        }
        if rest.len() < size + 2 {
            return false;
        }
        rest = &rest[size + 2..];
    }
}

/// Decode a chunked body: the data, and whether the terminating zero-size
/// chunk was reached. A cut-off chunk keeps what arrived.
fn dechunk(mut rest: &[u8]) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    loop {
        let Some(line_end) = rest.windows(2).position(|w| w == b"\r\n") else {
            break;
        };
        let size_text = String::from_utf8_lossy(&rest[..line_end]);
        let size_hex = size_text.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            break;
        };
        rest = &rest[line_end + 2..];
        if size == 0 {
            return (out, trailer_complete(rest));
        }
        let take = size.min(rest.len());
        out.extend_from_slice(&rest[..take]);
        if take < size || rest.len() < size + 2 {
            break;
        }
        rest = &rest[size + 2..];
    }
    (out, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_is_a_plain_get_that_closes() {
        let text = String::from_utf8(request_bytes("/metrics")).unwrap();
        assert!(text.starts_with("GET /metrics HTTP/1.1\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
    }

    #[test]
    fn content_length_body_and_type() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: 5\r\n\r\nhelloEXTRA";
        let r = parse_response(raw, 1024).unwrap();
        assert_eq!(r.status, 200);
        assert_eq!(r.content_type.as_deref(), Some("text/plain; version=0.0.4"));
        assert_eq!(r.body, b"hello");
        assert!(!r.truncated);
        assert!(body_complete(raw));
    }

    #[test]
    fn chunked_body_is_decoded() {
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n4\r\nWiki\r\n5\r\npedia\r\n0\r\n\r\n";
        let r = parse_response(raw, 1024).unwrap();
        assert_eq!(r.body, b"Wikipedia");
        assert!(body_complete(raw));
    }

    #[test]
    fn the_zero_chunk_needs_its_closing_empty_line() {
        let head = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n".to_vec();
        assert!(!body_complete(&head), "0\\r\\n alone is not the end");
        let mut done = head.clone();
        done.extend_from_slice(b"\r\n");
        assert!(body_complete(&done));
        let mut trailer = head;
        trailer.extend_from_slice(b"X-Trace: 1\r\n\r\n");
        assert!(body_complete(&trailer), "trailers end with an empty line");
        assert_eq!(parse_response(&trailer, 64).unwrap().body, b"ok");
    }

    #[test]
    fn a_zero_chunk_pattern_inside_data_is_not_the_end() {
        // One 10-byte chunk whose data contains "0\r\n\r\n", no terminator yet.
        let raw = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\na\r\nab0\r\n\r\ncde\r\n";
        assert!(!body_complete(raw));
        assert!(parse_response(raw, 1024).is_err(), "cut off, not complete");
    }

    #[test]
    fn a_body_short_of_content_length_is_an_error_unless_capped() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabc";
        let err = parse_response(raw, 1024).unwrap_err().to_string();
        assert!(err.contains("cut off after 3 bytes"), "{err}");
        // Reading stopped at the cap: that is truncation, not a cut-off.
        let capped = parse_response(raw, 2).unwrap();
        assert!(capped.truncated);
        assert_eq!(capped.body, b"ab");
    }

    #[test]
    fn body_over_the_cap_is_truncated() {
        let raw = b"HTTP/1.0 503 Service Unavailable\r\n\r\n0123456789";
        let r = parse_response(raw, 4).unwrap();
        assert_eq!(r.status, 503);
        assert_eq!(r.body, b"0123");
        assert!(r.truncated);
    }

    #[test]
    fn non_http_answer_is_an_error() {
        assert!(parse_response(b"SSH-2.0-OpenSSH\r\n\r\n", 10).is_err());
        assert!(parse_response(b"HTTP/1.1 200", 10).is_err());
    }

    #[tokio::test]
    async fn get_over_a_duplex_stream() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let server_task = tokio::spawn(async move {
            let mut req = vec![0u8; 1024];
            let n = server.read(&mut req).await.unwrap();
            assert!(String::from_utf8_lossy(&req[..n]).starts_with("GET /healthz HTTP/1.1"));
            server
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await
                .unwrap();
        });
        let r = get_over(&mut client, "/healthz", 1024).await.unwrap();
        server_task.await.unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"ok"[..]));
    }
}
