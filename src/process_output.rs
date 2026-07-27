//! Shared formatting for bounded external-process output.
//!
//! Callers retain their domain-specific error types and redaction rules while
//! sharing byte limits, UTF-8 boundary handling, and exit-status rendering.

use std::process::ExitStatus;

/// Renders a child exit code without exposing platform-specific status text.
pub(crate) fn command_status_text(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| code.to_string(),
    )
}

/// Lossily decodes at most `limit` bytes and marks truncated diagnostics.
pub(crate) fn truncated_lossy_message(bytes: &[u8], limit: usize) -> String {
    if bytes.len() <= limit {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let mut message = String::from_utf8_lossy(&bytes[..limit]).into_owned();
    message.push_str("\n[truncated]");
    message
}

/// Truncates a UTF-8 string to at most `limit` bytes and appends an ellipsis.
pub(crate) fn truncate_with_ellipsis(message: &mut String, limit: usize) {
    if message.len() <= limit {
        return;
    }

    let boundary = (0..=limit)
        .rev()
        .find(|&index| message.is_char_boundary(index))
        .expect("zero is always a valid string boundary");
    message.truncate(boundary);
    message.push_str("...");
}
