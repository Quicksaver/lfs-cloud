//! Route planning for `lfs-cloud init`.
//!
//! The init command eventually owns Git LFS configuration writes. This module
//! currently keeps the first step narrow: derive the LFS Cloud endpoint for the
//! current repository from a trusted server base URL and the parsed Git remote.

use url::Url;

use crate::{CliError, CliResult, GitRemote};

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
        let server_url = validate_server_url(server_url.as_ref())?;
        let lfs_url = format!(
            "{server_url}/{}/{}/{}.git/info/lfs",
            remote.host, remote.owner, remote.name
        );

        Ok(Self {
            server_url,
            remote: remote.clone(),
            lfs_url,
        })
    }
}

fn validate_server_url(value: &str) -> CliResult<String> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return invalid_server_url("server URL must not be blank or padded");
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

    Ok(value.to_owned())
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
        ] {
            let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
                .expect("remote should parse");
            let error = LfsInitRoute::resolve(url, &remote).expect_err("URL should be rejected");

            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }
}
