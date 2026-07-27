//! Neutral parsing and safety checks for byte-oriented Git path output.
//!
//! Git emits NUL-delimited paths as raw bytes on Unix. Callers keep their own
//! command invocation and domain errors, while this module owns the shared
//! attribute-triple grammar and repository-relative path boundary.

use std::path::{Component, PathBuf};

#[cfg(unix)]
use std::{ffi::OsString, os::unix::ffi::OsStringExt};

/// Failure while parsing path-bearing Git output.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum GitPathOutputError {
    /// `git check-attr -z` did not return complete path/attribute/value triples.
    MalformedAttributeOutput,
    /// Git returned a path that cannot be represented on this platform.
    #[cfg(not(unix))]
    NonUtf8Path,
    /// Git returned an absolute, empty, or traversal path.
    PathOutsideWorktree,
}

/// Parses the paths whose `filter` attribute is exactly `lfs`.
pub(crate) fn parse_lfs_filter_attribute_paths(
    stdout: &[u8],
) -> Result<Vec<PathBuf>, GitPathOutputError> {
    let mut paths = Vec::new();
    let mut fields = stdout.split(|byte| *byte == b'\0').peekable();
    while let Some(relative_path) = fields.next() {
        if relative_path.is_empty() {
            if fields.peek().is_none() {
                break;
            }
            return Err(GitPathOutputError::MalformedAttributeOutput);
        }

        let Some(attribute) = fields.next() else {
            return Err(GitPathOutputError::MalformedAttributeOutput);
        };
        let Some(value) = fields.next() else {
            return Err(GitPathOutputError::MalformedAttributeOutput);
        };

        if attribute == b"filter" && value == b"lfs" {
            paths.push(safe_git_relative_path(relative_path)?);
        }
    }

    Ok(paths)
}

/// Converts raw Git path bytes and enforces a contained relative path.
pub(crate) fn safe_git_relative_path(relative_path: &[u8]) -> Result<PathBuf, GitPathOutputError> {
    let path = git_path_bytes_to_path_buf(relative_path)?;
    let contained = !path.is_absolute()
        && path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));

    contained
        .then_some(path)
        .ok_or(GitPathOutputError::PathOutsideWorktree)
}

#[cfg(unix)]
fn git_path_bytes_to_path_buf(relative_path: &[u8]) -> Result<PathBuf, GitPathOutputError> {
    Ok(PathBuf::from(OsString::from_vec(relative_path.to_owned())))
}

#[cfg(not(unix))]
fn git_path_bytes_to_path_buf(relative_path: &[u8]) -> Result<PathBuf, GitPathOutputError> {
    String::from_utf8(relative_path.to_owned())
        .map(PathBuf::from)
        .map_err(|_| GitPathOutputError::NonUtf8Path)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{GitPathOutputError, parse_lfs_filter_attribute_paths, safe_git_relative_path};

    #[test]
    fn parses_only_lfs_filter_triples() {
        let paths = parse_lfs_filter_attribute_paths(
            b"docs/readme.md\0filter\0unspecified\0asset/model.bin\0filter\0lfs\0",
        )
        .expect("attribute triples should parse");

        assert_eq!(paths, vec![PathBuf::from("asset/model.bin")]);
    }

    #[test]
    fn rejects_malformed_attribute_triples() {
        assert_eq!(
            parse_lfs_filter_attribute_paths(b"asset/model.bin\0filter"),
            Err(GitPathOutputError::MalformedAttributeOutput)
        );
        assert_eq!(
            parse_lfs_filter_attribute_paths(b"\0filter\0lfs\0"),
            Err(GitPathOutputError::MalformedAttributeOutput)
        );
    }

    #[test]
    fn rejects_paths_outside_the_worktree() {
        for path in [b"" as &[u8], b"/tmp/model.bin", b"../model.bin"] {
            assert_eq!(
                safe_git_relative_path(path),
                Err(GitPathOutputError::PathOutsideWorktree)
            );
        }
    }

    #[test]
    fn accepts_dot_prefixed_relative_components() {
        assert_eq!(
            safe_git_relative_path(b".github/assets.bin"),
            Ok(PathBuf::from(".github/assets.bin"))
        );
    }

    #[cfg(unix)]
    #[test]
    fn preserves_non_utf8_unix_paths() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};

        assert_eq!(
            safe_git_relative_path(b"asset/nonutf-\xff.bin"),
            Ok(PathBuf::from(OsString::from_vec(
                b"asset/nonutf-\xff.bin".to_vec()
            )))
        );
    }
}
