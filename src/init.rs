//! Route planning for `lfs-cloud init`.
//!
//! The init command eventually owns Git LFS configuration writes. This module
//! currently keeps the first step narrow: derive the LFS Cloud endpoint for the
//! current repository from a trusted server base URL and the parsed Git remote.

use url::Url;

use crate::{CliError, CliResult, GitRemote, http_transport::uses_protected_http_transport};

/// Resolved Git LFS endpoint for an `lfs-cloud init --server` invocation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsInitRoute {
    /// Server base URL supplied by the user.
    pub server_url: String,
    /// Parsed Git remote used to derive the repository route.
    pub remote: GitRemote,
    /// Full Git LFS endpoint that should be written to `.lfsconfig` or local Git config.
    pub lfs_url: String,
}

impl LfsInitRoute {
    /// Resolves the Git LFS endpoint for a parsed repository remote.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `server_url` is not a safe HTTP(S) base URL.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{GitRemote, LfsInitRoute};
    ///
    /// let remote = GitRemote::parse("origin", "git@github.com:owner/repo.git")?;
    /// let route = LfsInitRoute::resolve("http://127.0.0.1:8080", &remote)?;
    ///
    /// assert_eq!(
    ///     route.lfs_url,
    ///     "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs"
    /// );
    /// # Ok::<(), lfs_cloud::CliError>(())
    /// ```
    pub fn resolve(server_url: impl AsRef<str>, remote: &GitRemote) -> CliResult<Self> {
        Self::resolve_with_insecure_http(server_url, remote, false)
    }

    /// Resolves a Git LFS endpoint with an explicit plaintext-HTTP policy.
    ///
    /// This is kept crate-private so only CLI commands that expose the named
    /// unsafe opt-in can bypass the default transport protection.
    pub(crate) fn resolve_with_insecure_http(
        server_url: impl AsRef<str>,
        remote: &GitRemote,
        allow_insecure_http: bool,
    ) -> CliResult<Self> {
        let mut lfs_url = validate_server_url(server_url.as_ref(), allow_insecure_http)?;
        let server_url = normalized_server_url(&lfs_url);
        append_lfs_route_segments(&mut lfs_url, remote)?;

        Ok(Self {
            server_url,
            remote: remote.clone(),
            lfs_url: lfs_url.to_string(),
        })
    }
}

pub(crate) fn validate_server_url(value: &str, allow_insecure_http: bool) -> CliResult<Url> {
    if value.is_empty() {
        return invalid_server_url("server URL must not be blank");
    }
    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control() || character == '\\')
    {
        return invalid_server_url(
            "server URL must not include whitespace, control characters, or backslashes",
        );
    }
    if value.ends_with('/') {
        return invalid_server_url("server URL must not end with a trailing slash");
    }
    if raw_server_path_has_dot_segments(value) {
        return invalid_server_url("server URL path must not include dot segments");
    }

    let parsed = Url::parse(value).map_err(|source| CliError::InvalidArguments {
        message: format!("server URL is not valid: {source}"),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return invalid_server_url("server URL must be a valid http or https URL");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return invalid_server_url("server URL must not include credentials");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return invalid_server_url("server URL must not include a query string or fragment");
    }
    if !allow_insecure_http && !uses_protected_http_transport(&parsed) {
        return invalid_server_url(
            "server URL must use HTTPS unless it targets an exact loopback IP; pass --allow-insecure-http only on a trusted development network",
        );
    }

    Ok(parsed)
}

fn normalized_server_url(url: &Url) -> String {
    url.to_string().trim_end_matches('/').to_owned()
}

fn append_lfs_route_segments(url: &mut Url, remote: &GitRemote) -> CliResult<()> {
    let repo_path = format!("{}.git", remote.name);
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| CliError::InvalidArguments {
            message: "server URL cannot be used as a route base".to_owned(),
        })?;

    segments.extend([
        remote.host.as_str(),
        remote.owner.as_str(),
        repo_path.as_str(),
        "info",
        "lfs",
    ]);

    Ok(())
}

fn raw_server_path_has_dot_segments(value: &str) -> bool {
    let Some((_, after_scheme)) = value.split_once("://") else {
        return false;
    };
    let Some(path_start) = after_scheme.find('/') else {
        return false;
    };
    let path = after_scheme[path_start + 1..]
        .split(['?', '#'])
        .next()
        .unwrap_or_default();

    path.split('/').any(is_dot_segment)
}

fn is_dot_segment(segment: &str) -> bool {
    matches!(
        segment.to_ascii_lowercase().as_str(),
        "." | ".." | "%2e" | ".%2e" | "%2e." | "%2e%2e"
    )
}

fn invalid_server_url<T>(message: impl Into<String>) -> CliResult<T> {
    Err(CliError::InvalidArguments {
        message: message.into(),
    })
}

#[cfg(test)]
mod tests {
    use super::LfsInitRoute;
    use crate::{CliError, GitRemote};

    #[test]
    fn resolves_lfs_url_from_server_base_and_remote() {
        let remote = GitRemote::parse("origin", "git@github.com:owner/repo.git")
            .expect("remote should parse");
        let route =
            LfsInitRoute::resolve("http://127.0.0.1:8080", &remote).expect("route should resolve");

        assert_eq!(route.server_url, "http://127.0.0.1:8080");
        assert_eq!(route.remote, remote);
        assert_eq!(
            route.lfs_url,
            "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs"
        );
    }

    #[test]
    fn preserves_server_base_paths() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/.github.git")
            .expect("remote should parse");
        let route = LfsInitRoute::resolve("https://lfs.example.com/custom/base", &remote)
            .expect("route should resolve");

        assert_eq!(
            route.lfs_url,
            "https://lfs.example.com/custom/base/github.com/owner/.github.git/info/lfs"
        );
    }

    #[test]
    fn normalizes_server_base_url() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
            .expect("remote should parse");
        let route =
            LfsInitRoute::resolve("HTTPS://LOCALHOST:8080", &remote).expect("route should resolve");

        assert_eq!(route.server_url, "https://localhost:8080");
        assert_eq!(
            route.lfs_url,
            "https://localhost:8080/github.com/owner/repo.git/info/lfs"
        );
    }

    #[test]
    fn requires_https_except_for_exact_loopback_or_explicit_opt_in() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
            .expect("remote should parse");

        for rejected in [
            "http://localhost:8080",
            "http://192.168.1.25:8080",
            "http://lfs.example.com",
        ] {
            let error = LfsInitRoute::resolve(rejected, &remote)
                .expect_err("non-loopback HTTP should require explicit opt-in");
            assert!(error.to_string().contains("must use HTTPS"), "{rejected}");
        }

        let route =
            LfsInitRoute::resolve_with_insecure_http("http://192.168.1.25:8080", &remote, true)
                .expect("explicit unsafe opt-in should permit a trusted LAN endpoint");
        assert_eq!(route.server_url, "http://192.168.1.25:8080");
    }

    #[test]
    fn rejects_unsafe_server_urls() {
        for url in [
            "",
            " http://127.0.0.1:8080",
            "http://127.0.0.1:8080/",
            "ftp://127.0.0.1:8080",
            "http://user:secret@127.0.0.1:8080",
            "http://127.0.0.1:8080?token=secret",
            "http://127.0.0.1:8080#fragment",
            "http://127.0.0.1:8080/foo bar",
            "http://127.0.0.1:8080/foo\nbar",
            "http://127.0.0.1:8080\\foo",
            "http://127.0.0.1:8080/../base",
            "http://127.0.0.1:8080/./base",
            "http://127.0.0.1:8080/%2e%2e/base",
        ] {
            let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
                .expect("remote should parse");
            let error = LfsInitRoute::resolve(url, &remote).expect_err("URL should be rejected");

            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }
}
