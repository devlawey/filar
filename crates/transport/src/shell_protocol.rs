//! Marker protocol of the persistent shell (#561).
//!
//! A command is written to a long-lived POSIX shell followed by a `printf`
//! that prints a unique marker line with the exit code and `$PWD`. The
//! output before the marker is the command's output; stderr arriving right
//! after the marker is still collected. After a timeout the shell is
//! resynchronised with a bare marker, and a running command is interrupted
//! through the shell's `$$` with [`interrupt_command`].
//!
//! The protocol works over any [`EventSource`] — a stream of stdout, stderr
//! and close events — so it is shared by every transport that drives a shell
//! this way (SSH today, a container exec next) and has no dependency on a
//! particular transport library. Nothing here writes to the remote disk.

use std::future::Future;
use std::time::Duration;

use tokio::sync::mpsc;
use uuid::Uuid;

use filar_core::{CoreError, Result};

/// Unique prefix for marker lines. Chosen to be extremely unlikely to appear
/// in normal command output.
pub(crate) const MARKER_PREFIX: &str = "__FILAR_";

/// An event read from the shell's channel.
#[derive(Debug)]
pub(crate) enum ChannelEvent {
    /// stdout data.
    Data(String),
    /// stderr data.
    Stderr(String),
    /// The channel was closed (EOF or close).
    Closed,
}

/// A source of shell [`ChannelEvent`]s: the reader task's receiver, or a
/// transport channel read directly before the reader task starts.
pub(crate) trait EventSource {
    /// The next event; `None` when the source is gone for good.
    fn next_event(&mut self) -> impl Future<Output = Option<ChannelEvent>> + Send;
}

impl EventSource for mpsc::UnboundedReceiver<ChannelEvent> {
    fn next_event(&mut self) -> impl Future<Output = Option<ChannelEvent>> + Send {
        self.recv()
    }
}

/// The marker tag of request `req_id`: unique per command, so output of an
/// earlier command can never be taken for this one's marker.
pub(crate) fn command_marker_tag(req_id: u64) -> String {
    format!("{MARKER_PREFIX}req_{:08x}_{}", req_id, Uuid::new_v4().simple())
}

/// The bytes written for `command`: the command itself, then a `printf` of
/// the marker line `<tag>__<exit>__<pwd>__`.
pub(crate) fn command_payload(command: &str, marker_tag: &str) -> String {
    format!("{command}\nprintf '\\n{marker_tag}__%d__%s__\\n' \"$?\" \"$PWD\"\n")
}

/// A fresh sync request: the bytes to write and the marker to wait for. Used
/// to drain the shell's initial output and to resync after a timeout.
pub(crate) fn sync_request() -> (String, String) {
    let marker = format!("{MARKER_PREFIX}sync_{}", Uuid::new_v4().simple());
    let payload = format!("printf '\\n{marker}\\n'\n");
    (payload, marker)
}

/// A fresh request for the shell's PID (`$$`): the bytes to write and the
/// tag to pass to [`read_shell_pid`].
pub(crate) fn shell_pid_request() -> (String, String) {
    let tag = format!("{MARKER_PREFIX}pid_{}", Uuid::new_v4().simple());
    let payload = format!("printf '\\n{tag}__%s__\\n' \"$$\"\n");
    (payload, tag)
}

/// The remote command that interrupts whatever the shell `pid` is running:
/// `SIGINT` to each of its direct children. POSIX `ps -A -o` and `awk`, so
/// it works on any POSIX host without `pkill`; it writes nothing to disk.
///
/// Exits non-zero if `ps` fails or any `kill` fails. No children at all is
/// success: nothing is running, so there is nothing to interrupt — a
/// timeout that fires as the command finishes must not report a failure.
pub(crate) fn interrupt_command(pid: u32) -> String {
    format!(
        "pids=$(ps -A -o pid= -o ppid= | awk -v pp={pid} '$2 == pp {{ print $1 }}') || exit 1; \
         rc=0; for p in $pids; do kill -INT \"$p\" 2>/dev/null || rc=1; done; exit $rc"
    )
}

/// Read the answer to a [`shell_pid_request`] with tag `tag`, bounded by
/// `timeout`. `None` if it does not arrive or does not parse.
pub(crate) async fn read_shell_pid<E: EventSource>(
    source: &mut E,
    tag: &str,
    timeout: Duration,
) -> Option<u32> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut buf = String::new();
    loop {
        match tokio::time::timeout_at(deadline, source.next_event()).await {
            Ok(Some(ChannelEvent::Data(text))) => {
                buf.push_str(&text);
                if let Some(pid) = parse_shell_pid(&buf, tag) {
                    return Some(pid);
                }
            }
            Ok(Some(ChannelEvent::Stderr(_))) => {}
            Ok(Some(ChannelEvent::Closed)) | Ok(None) | Err(_) => return None,
        }
    }
}

/// Parse `<tag>__<pid>__` out of `buf` — the marker line, not the echo of
/// the `printf` that produced it (a PTY-less shell does not echo, but the
/// parse does not rely on that).
pub(crate) fn parse_shell_pid(buf: &str, tag: &str) -> Option<u32> {
    let mut rest = buf;
    while let Some(pos) = rest.find(tag) {
        let after = &rest[pos + tag.len()..];
        if let Some(digits) = after.strip_prefix("__") {
            if let Some(end) = digits.find("__") {
                if let Ok(pid) = digits[..end].parse() {
                    return Some(pid);
                }
            }
        }
        rest = after;
    }
    None
}

/// Read events until a marker of the form `<marker_tag>__<exit_code>__[<pwd>__]`
/// is found. Returns stdout, stderr, exit code, and optional `$PWD` from the
/// marker. stderr arriving within a short window after the marker is still
/// collected: the shell may flush it after stdout.
pub(crate) async fn recv_until_marker<E: EventSource>(
    source: &mut E,
    marker_tag: &str,
    timeout: Duration,
) -> Result<(String, String, Option<i32>, Option<String>)> {
    let mut buf = String::new();
    let mut stderr_buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let event = tokio::time::timeout_at(deadline, source.next_event())
            .await
            .map_err(|_| CoreError::Other(format!("timeout waiting for command marker `{marker_tag}`")))?
            .ok_or_else(|| CoreError::Other("channel closed before marker received".into()))?;

        match event {
            ChannelEvent::Data(text) => {
                buf.push_str(&text);

                if let Some((code, cwd, line_start)) = parse_filar_marker(&buf, marker_tag) {
                    let output = buf[..line_start].to_string();
                    let output = output
                        .strip_suffix('\n')
                        .map(str::to_string)
                        .unwrap_or(output);

                    while let Ok(Some(ChannelEvent::Stderr(text))) =
                        tokio::time::timeout(Duration::from_millis(50), source.next_event()).await
                    {
                        stderr_buf.push_str(&text);
                    }
                    return Ok((output, stderr_buf, Some(code), cwd));
                }
            }
            ChannelEvent::Stderr(text) => {
                stderr_buf.push_str(&text);
            }
            ChannelEvent::Closed => {
                return Err(CoreError::Other(
                    "channel closed before command marker received".into(),
                ));
            }
        }
    }
}

/// Parse `<marker_tag>__<exit>__` or `<marker_tag>__<exit>__<pwd>__`.
/// Returns the exit code, the `$PWD` (if present and safe) and the byte
/// offset where the marker line starts.
///
/// `pwd` is the text before the last `__` on the marker line (so names like
/// `my__project` still parse).
pub(crate) fn parse_filar_marker(
    buf: &str,
    marker_tag: &str,
) -> Option<(i32, Option<String>, usize)> {
    let pos = buf.find(marker_tag)?;
    let after_tag = &buf[pos + marker_tag.len()..];
    let rest = after_tag.strip_prefix("__")?;
    let end = rest.find("__")?;
    let code: i32 = rest[..end].trim().parse().ok()?;
    let after_code = &rest[end + 2..];
    // The marker line is complete only once its terminator has arrived;
    // otherwise a read split inside `$PWD` would drop the cwd.
    let line_end = after_code.find(['\n', '\r'])?;
    let line = &after_code[..line_end];
    let cwd = line.strip_suffix("__").map(str::trim).and_then(|pwd| {
        if crate::is_safe_cwd(pwd) {
            Some(pwd.to_string())
        } else {
            None
        }
    });
    let line_start = buf[..pos].rfind('\n').map(|p| p + 1).unwrap_or(0);
    Some((code, cwd, line_start))
}

/// Read events until `marker` is found; output is discarded. Used to drain
/// the initial output after connect and to resync after a timeout/cancel.
pub(crate) async fn recv_until_marker_simple<E: EventSource>(
    source: &mut E,
    marker: &str,
    timeout: Duration,
) -> Result<()> {
    let mut buf = String::new();
    let deadline = tokio::time::Instant::now() + timeout;

    loop {
        let event = tokio::time::timeout_at(deadline, source.next_event())
            .await
            .map_err(|_| CoreError::Other(format!("timeout waiting for marker `{marker}`")))?
            .ok_or_else(|| CoreError::Other("channel closed before marker received".into()))?;

        match event {
            ChannelEvent::Data(text) => {
                buf.push_str(&text);
                if buf.contains(marker) {
                    return Ok(());
                }
            }
            ChannelEvent::Stderr(_) => {
                // Ignore stderr during sync.
            }
            ChannelEvent::Closed => {
                return Err(CoreError::Other("channel closed before marker received".into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_format() {
        let marker_id = "req_00000001";
        let marker_tag = format!("{}{}", MARKER_PREFIX, marker_id);
        assert_eq!(marker_tag, "__FILAR_req_00000001");
        let tag = command_marker_tag(1);
        assert!(tag.starts_with("__FILAR_req_00000001_"), "{tag}");
        assert_ne!(command_marker_tag(1), command_marker_tag(1), "tags are unique");
    }

    #[test]
    fn payload_format() {
        let payload = command_payload("echo hello", "__FILAR_req_00000001");
        assert_eq!(
            payload,
            "echo hello\nprintf '\\n__FILAR_req_00000001__%d__%s__\\n' \"$?\" \"$PWD\"\n"
        );
    }

    #[test]
    fn sync_and_pid_requests_print_their_own_marker() {
        let (payload, marker) = sync_request();
        assert!(marker.starts_with("__FILAR_sync_"), "{marker}");
        assert_eq!(payload, format!("printf '\\n{marker}\\n'\n"));

        let (payload, tag) = shell_pid_request();
        assert!(tag.starts_with("__FILAR_pid_"), "{tag}");
        assert_eq!(payload, format!("printf '\\n{tag}__%s__\\n' \"$$\"\n"));
    }

    #[test]
    fn parse_marker_with_pwd() {
        let tag = "__FILAR_req_00000001";
        let buf = "hello\n__FILAR_req_00000001__0__/tmp/work__\n";
        let (code, cwd, start) = parse_filar_marker(buf, tag).unwrap();
        assert_eq!(code, 0);
        assert_eq!(cwd.as_deref(), Some("/tmp/work"));
        assert_eq!(&buf[..start], "hello\n");
    }

    #[test]
    fn parse_marker_exit_only() {
        let tag = "__FILAR_req_00000001";
        let buf = "out\n__FILAR_req_00000001__1__\n";
        let (code, cwd, _) = parse_filar_marker(buf, tag).unwrap();
        assert_eq!(code, 1);
        assert!(cwd.is_none());
    }

    #[test]
    fn parse_marker_pwd_allows_double_underscore_in_name() {
        let tag = "__FILAR_req_00000001";
        let buf = "out\n__FILAR_req_00000001__0__/home/u/my__project__\n";
        let (code, cwd, _) = parse_filar_marker(buf, tag).unwrap();
        assert_eq!(code, 0);
        assert_eq!(cwd.as_deref(), Some("/home/u/my__project"));
    }

    #[test]
    fn an_unterminated_marker_line_is_not_accepted_yet() {
        let tag = "__FILAR_req_1";
        assert!(parse_filar_marker("out\n__FILAR_req_1__0__/sr", tag).is_none());
        assert!(parse_filar_marker("out\n__FILAR_req_1__0__", tag).is_none());
        let (code, cwd, _) = parse_filar_marker("out\n__FILAR_req_1__0__/srv__\r\n", tag).unwrap();
        assert_eq!((code, cwd.as_deref()), (0, Some("/srv")));
    }

    #[tokio::test]
    async fn a_marker_split_inside_pwd_keeps_the_cwd() {
        let mut rx = source(vec![
            ChannelEvent::Data("hi\n__FILAR_req_1__0__/sr".into()),
            ChannelEvent::Data("v__\n".into()),
        ]);
        let (out, _, code, cwd) =
            recv_until_marker(&mut rx, "__FILAR_req_1", Duration::from_secs(1)).await.unwrap();
        assert_eq!((out.as_str(), code, cwd.as_deref()), ("hi", Some(0), Some("/srv")));
    }

    #[test]
    fn the_shell_pid_is_read_from_its_marker_line() {
        let tag = "__FILAR_pid_abc";
        assert_eq!(parse_shell_pid("noise\n__FILAR_pid_abc__4242__\n", tag), Some(4242));
        // An echo of the printf (a shell with echo on) is skipped.
        let echoed = "printf '\\n__FILAR_pid_abc__%s__\\n' \"$$\"\n__FILAR_pid_abc__77__\n";
        assert_eq!(parse_shell_pid(echoed, tag), Some(77));
        assert_eq!(parse_shell_pid("__FILAR_pid_abc__", tag), None, "incomplete");
        assert_eq!(parse_shell_pid("__FILAR_pid_other__1__", tag), None);
    }

    #[test]
    fn the_interrupt_signals_the_shells_children_and_writes_nothing() {
        let cmd = interrupt_command(4242);
        assert!(cmd.contains("awk -v pp=4242"), "{cmd}");
        assert!(cmd.contains("kill -INT"), "{cmd}");
        // A failed `ps` or `kill` is reported, not swallowed.
        assert!(cmd.contains("|| exit 1") && cmd.contains("|| rc=1") && cmd.ends_with("exit $rc"), "{cmd}");
        // Zero-install: the only redirection is stderr to /dev/null.
        assert_eq!(cmd.matches('>').count(), 1, "{cmd}");
        assert!(cmd.contains("2>/dev/null"), "{cmd}");
    }

    fn source(events: Vec<ChannelEvent>) -> mpsc::UnboundedReceiver<ChannelEvent> {
        let (tx, rx) = mpsc::unbounded_channel();
        for e in events {
            tx.send(e).unwrap();
        }
        // `tx` is kept alive only by the events already queued: once they are
        // read, `next_event` yields `None`.
        rx
    }

    #[tokio::test]
    async fn output_exit_code_cwd_and_late_stderr_are_collected() {
        let tag = "__FILAR_req_1";
        let mut rx = source(vec![
            ChannelEvent::Data("line1\nli".into()),
            ChannelEvent::Stderr("warn\n".into()),
            ChannelEvent::Data("ne2\n__FILAR_req_1__3__/srv__\n".into()),
            ChannelEvent::Stderr("late\n".into()),
        ]);
        let (out, err, code, cwd) =
            recv_until_marker(&mut rx, tag, Duration::from_secs(1)).await.unwrap();
        assert_eq!(out, "line1\nline2");
        assert_eq!(err, "warn\nlate\n");
        assert_eq!(code, Some(3));
        assert_eq!(cwd.as_deref(), Some("/srv"));
    }

    #[tokio::test]
    async fn a_closed_channel_before_the_marker_is_an_error() {
        let mut rx = source(vec![ChannelEvent::Data("partial".into()), ChannelEvent::Closed]);
        let err = recv_until_marker(&mut rx, "__FILAR_req_1", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("closed before command marker"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_marker_times_out() {
        let (_tx, mut rx) = mpsc::unbounded_channel::<ChannelEvent>();
        let err = recv_until_marker(&mut rx, "__FILAR_req_1", Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timeout"), "{err}");
    }

    #[tokio::test]
    async fn resync_skips_output_and_stderr_until_the_marker() {
        let mut rx = source(vec![
            ChannelEvent::Data("stale output\n__FILAR_sy".into()),
            ChannelEvent::Stderr("noise".into()),
            ChannelEvent::Data("nc_x\n".into()),
        ]);
        recv_until_marker_simple(&mut rx, "__FILAR_sync_x", Duration::from_secs(1))
            .await
            .unwrap();
        let mut closed = source(vec![ChannelEvent::Closed]);
        assert!(recv_until_marker_simple(&mut closed, "__FILAR_sync_x", Duration::from_secs(1))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn the_shell_pid_is_read_from_events_and_missing_is_none() {
        let mut rx = source(vec![
            ChannelEvent::Stderr("motd".into()),
            ChannelEvent::Data("\n__FILAR_pid_t__9".into()),
            ChannelEvent::Data("1__\n".into()),
        ]);
        assert_eq!(read_shell_pid(&mut rx, "__FILAR_pid_t", Duration::from_secs(1)).await, Some(91));
        let mut closed = source(vec![ChannelEvent::Closed]);
        assert_eq!(read_shell_pid(&mut closed, "__FILAR_pid_t", Duration::from_secs(1)).await, None);
    }
}
