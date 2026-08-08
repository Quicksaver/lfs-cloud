//! Route planning for `lfscloud init`.
//!
//! The init command eventually owns Git LFS configuration writes. This module
//! currently keeps the first step narrow: derive the LFS Cloud endpoint for the
//! current repository from a trusted server base URL and the parsed Git remote.

use url::Url;

use crate::{
    CliError, CliResult, GitRemote,
    http_transport::{HttpUrlPolicy, HttpUrlValidationError, validate_http_url},
};

/// Resolved Git LFS endpoint for an `lfscloud init --server` invocation.
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
    /// use lfscloud::{GitRemote, LfsInitRoute};
    ///
    /// let remote = GitRemote::parse("origin", "git@github.com:owner/repo.git")?;
    /// let route = LfsInitRoute::resolve("http://127.0.0.1:8080", &remote)?;
    ///
    /// assert_eq!(
    ///     route.lfs_url,
    ///     "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs"
    /// );
    /// # Ok::<(), lfscloud::CliError>(())
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

    /// Recovers a server base URL from a repository-specific LFS Cloud URL.
    ///
    /// The configured URL must exactly equal the route that LFS Cloud would
    /// construct for `remote`. This prevents another repository's endpoint or
    /// an arbitrary legacy LFS URL from becoming an authentication target.
    pub(crate) fn resolve_from_lfs_url(
        lfs_url: impl AsRef<str>,
        remote: &GitRemote,
    ) -> CliResult<Self> {
        Self::resolve_from_lfs_url_with_insecure_http(lfs_url, remote, false)
    }

    /// Recovers a server base URL with an explicit plaintext-HTTP policy.
    pub(crate) fn resolve_from_lfs_url_with_insecure_http(
        lfs_url: impl AsRef<str>,
        remote: &GitRemote,
        allow_insecure_http: bool,
    ) -> CliResult<Self> {
        let configured_url = validate_server_url(lfs_url.as_ref(), allow_insecure_http)?;
        let mut server_base = configured_url.clone();
        {
            let mut segments =
                server_base
                    .path_segments_mut()
                    .map_err(|()| CliError::InvalidArguments {
                        message: "configured lfs.url cannot be used as an LFS Cloud route"
                            .to_owned(),
                    })?;
            segments.pop_if_empty();
            for _ in 0..5 {
                segments.pop();
            }
        }

        let route = Self::resolve_with_insecure_http(
            normalized_server_url(&server_base),
            remote,
            allow_insecure_http,
        )?;
        let resolved_url = validate_server_url(&route.lfs_url, allow_insecure_http)?;
        if resolved_url != configured_url {
            return Err(CliError::InvalidArguments {
                message: format!(
                    "configured lfs.url does not match the current repository {}; pass --server URL to select its LFS Cloud server explicitly",
                    remote.repository_label()
                ),
            });
        }

        Ok(route)
    }
}

pub(crate) fn validate_server_url(value: &str, allow_insecure_http: bool) -> CliResult<Url> {
    validate_http_url(value, HttpUrlPolicy::route_base(allow_insecure_http)).map_err(|error| {
        let hint = match error {
            HttpUrlValidationError::InsecureTransport => {
                "; pass --allow-insecure-http only on a trusted development network"
            }
            _ => "",
        };
        CliError::InvalidArguments {
            message: format!("server URL {error}{hint}"),
        }
    })
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
    fn derives_server_base_from_repository_lfs_url() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
            .expect("remote should parse");
        let route = LfsInitRoute::resolve_from_lfs_url(
            "https://lfs.example.com/custom/base/github.com/owner/repo.git/info/lfs",
            &remote,
        )
        .expect("repository LFS URL should resolve");

        assert_eq!(route.server_url, "https://lfs.example.com/custom/base");
        assert_eq!(
            route.lfs_url,
            "https://lfs.example.com/custom/base/github.com/owner/repo.git/info/lfs"
        );
    }

    #[test]
    fn rejects_repository_lfs_url_for_another_route() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
            .expect("remote should parse");

        let error = LfsInitRoute::resolve_from_lfs_url(
            "https://lfs.example.com/github.com/attacker/repo.git/info/lfs",
            &remote,
        )
        .expect_err("mismatched repository route should be rejected");

        assert!(matches!(error, CliError::InvalidArguments { message }
            if message.contains("does not match the current repository")));
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
