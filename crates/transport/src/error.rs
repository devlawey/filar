//! Transport-level error classification.
//!
//! SSH channel/session teardown can surface through several distinct error
//! values (a typed [`CoreError::ConnectionLost`] raised before dispatch, or a
//! stringly-typed [`CoreError::Other`] coming from `russh` / the reader task).
//! [`is_connection_lost`] centralises that decision so the rest of the code
//! never matches raw substrings inline.

use filar_core::CoreError;

/// Substrings that mark an error as "the SSH channel or session went away".
///
/// Kept lowercase; matching is case-insensitive. These are matched against the
/// generic [`CoreError::Other`] message because `russh` and the reader task
/// report closure as free-form text.
const CONNECTION_LOST_MARKERS: &[&str] = &[
    "channel task closed",
    "channel closed",
    "connection lost",
    "connection reset",
    "broken pipe",
    "session closed",
    "disconnected",
    "not connected",
];

/// Returns `true` when `err` represents a lost SSH connection/channel.
///
/// The typed [`CoreError::ConnectionLost`] always qualifies. For the generic
/// [`CoreError::Other`] variant the message is scanned for known markers so
/// that errors bubbling up from `russh` are still recognised.
pub fn is_connection_lost(err: &CoreError) -> bool {
    match err {
        CoreError::ConnectionLost(_) => true,
        CoreError::Other(msg) => {
            let lower = msg.to_ascii_lowercase();
            CONNECTION_LOST_MARKERS
                .iter()
                .any(|marker| lower.contains(marker))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_connection_lost_is_detected() {
        assert!(is_connection_lost(&CoreError::ConnectionLost(
            "ssh channel task closed".into()
        )));
    }

    #[test]
    fn other_with_marker_is_detected() {
        assert!(is_connection_lost(&CoreError::Other(
            "channel closed before command marker received".into()
        )));
        assert!(is_connection_lost(&CoreError::Other(
            "channel task closed".into()
        )));
        // Case-insensitive.
        assert!(is_connection_lost(&CoreError::Other(
            "Connection Reset by peer".into()
        )));
    }

    #[test]
    fn unrelated_errors_are_not_connection_lost() {
        assert!(!is_connection_lost(&CoreError::Other(
            "timeout waiting for command marker `__FILAR_req_1`".into()
        )));
        assert!(!is_connection_lost(&CoreError::Config("bad config".into())));
        assert!(!is_connection_lost(&CoreError::Secret("missing key".into())));
    }
}
