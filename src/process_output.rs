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

    let prefix = &bytes[..limit];
    let complete_prefix_len = complete_utf8_prefix_len(prefix);
    let mut message = String::from_utf8_lossy(&prefix[..complete_prefix_len]).into_owned();
    message.push_str("\n[truncated]");
    message
}

/// Truncates a UTF-8 string's content to at most `limit` bytes.
///
/// The appended three-byte ellipsis is not included in `limit`, so a truncated
/// result can contain up to `limit + 3` bytes.
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

fn complete_utf8_prefix_len(bytes: &[u8]) -> usize {
    let mut offset = 0;
    while offset < bytes.len() {
        match std::str::from_utf8(&bytes[offset..]) {
            Ok(_) => return bytes.len(),
            Err(error) => {
                let invalid_start = offset + error.valid_up_to();
                let Some(invalid_len) = error.error_len() else {
                    return invalid_start;
                };
                offset = invalid_start + invalid_len;
            }
        }
    }

    bytes.len()
}

#[cfg(test)]
mod tests {
    use super::{truncate_with_ellipsis, truncated_lossy_message};

    #[test]
    fn truncate_with_ellipsis_leaves_short_input_unchanged() {
        let mut message = "short".to_owned();
        let message_len = message.len();

        truncate_with_ellipsis(&mut message, message_len);

        assert_eq!(message, "short");
    }

    #[test]
    fn truncate_with_ellipsis_drops_a_split_multibyte_character() {
        let mut message = "abécd".to_owned();

        truncate_with_ellipsis(&mut message, 3);

        assert_eq!(message, "ab...");
    }

    #[test]
    fn truncated_lossy_message_marks_only_input_past_the_limit() {
        assert_eq!(truncated_lossy_message(b"exact", 5), "exact");
        assert_eq!(truncated_lossy_message(b"beyond", 5), "beyon\n[truncated]");
    }

    #[test]
    fn truncated_lossy_message_drops_a_split_multibyte_character() {
        assert_eq!(
            truncated_lossy_message("abécd".as_bytes(), 3),
            "ab\n[truncated]"
        );
    }
}
