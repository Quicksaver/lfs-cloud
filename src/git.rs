//! Git repository discovery helpers for CLI commands.
//!
//! The `init` and migration commands need to understand the current checkout
//! before they can compute the configured LFS route. This module keeps that Git
//! process boundary narrow and parses repository remotes into provider-neutral
//! host/owner/name pieces without accepting credentials from remote URLs.

use std::{
    fmt,
    path::{Path, PathBuf},
    process::{Command, ExitStatus},
};

use url::Url;

use crate::{CliError, CliResult, SanitizedMessage};

const DEFAULT_REMOTE_NAME: &str = "origin";
const MAX_GIT_OUTPUT_BYTES: usize = 64 * 1024;

/// A detected Git worktree and selected repository remote.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRepository {
    /// Absolute path to the Git worktree root reported by `git rev-parse`.
    pub worktree_root: PathBuf,
    /// Parsed remote selected for route derivation.
    pub remote: GitRemote,
}

impl GitRepository {
    /// Detects the current Git worktree and parses the `origin` remote.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `start_dir` is not inside a Git worktree, Git
    /// cannot be started, the selected remote is missing, or the remote URL is
    /// not a supported GitHub-style repository URL.
    pub fn discover(start_dir: impl AsRef<Path>) -> CliResult<Self> {
        Self::discover_with_remote(start_dir, DEFAULT_REMOTE_NAME)
    }

    /// Detects the current Git worktree and parses a named remote.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git worktree detection fails, the selected
    /// remote is unavailable, or its URL cannot be safely mapped to host,
    /// owner, and repository name components.
    pub fn discover_with_remote(
        start_dir: impl AsRef<Path>,
        remote_name: impl AsRef<str>,
    ) -> CliResult<Self> {
        let start_dir = start_dir.as_ref();
        let remote_name = validate_remote_name(remote_name.as_ref())?;
        let worktree_root = detect_worktree_root(start_dir)?;
        let remote_url = git_stdout(
            start_dir,
            ["remote", "get-url", remote_name.as_str()],
            &format!("git remote get-url {}", remote_name),
        )?;

        Ok(Self {
            worktree_root,
            remote: GitRemote::parse(remote_name, remote_url.trim_end())?,
        })
    }
}

/// A parsed Git remote suitable for constructing an LFS Cloud route.
#[derive(Clone, Eq, PartialEq)]
pub struct GitRemote {
    /// Git remote name, usually `origin`.
    pub remote_name: String,
    /// Original remote URL as returned by Git.
    pub url: String,
    /// Repository host, such as `github.com`.
    pub host: String,
    /// Repository owner or organization.
    pub owner: String,
    /// Repository name without a trailing `.git` suffix.
    pub name: String,
}

impl GitRemote {
    /// Parses an HTTPS or SSH Git remote URL into host, owner, and repo name.
    ///
    /// Supported forms include `https://github.com/owner/repo.git`,
    /// `ssh://git@github.com/owner/repo.git`, and
    /// `git@github.com:owner/repo.git`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the remote URL carries credentials, is not an
    /// HTTP(S)/SSH/scp-like Git URL, or does not contain exactly `owner/repo`.
    pub fn parse(remote_name: impl Into<String>, url: impl Into<String>) -> CliResult<Self> {
        let remote_name = remote_name.into();
        let remote_name = validate_remote_name(&remote_name)?;
        let url = url.into();
        let trimmed = url.trim();

        if trimmed.is_empty() || trimmed.len() != url.len() {
            return invalid_remote("Git remote URL must not be blank or padded");
        }

        let (host, path) = if trimmed.contains("://") {
            parse_url_remote(trimmed)?
        } else {
            parse_scp_like_remote(trimmed)?
        };
        let (owner, name) = parse_repository_path(&path)?;

        Ok(Self {
            remote_name,
            url,
            host,
            owner,
            name,
        })
    }
}

impl fmt::Debug for GitRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitRemote")
            .field("remote_name", &self.remote_name)
            .field("url", &redacted_remote_url(&self.url))
            .field("host", &self.host)
            .field("owner", &self.owner)
            .field("name", &self.name)
            .finish()
    }
}

fn detect_worktree_root(start_dir: &Path) -> CliResult<PathBuf> {
    let output = git_stdout(
        start_dir,
        ["rev-parse", "--show-toplevel"],
        "git rev-parse --show-toplevel",
    )?;

    Ok(PathBuf::from(output.trim_end()))
}

fn git_stdout<const N: usize>(
    current_dir: &Path,
    args: [&str; N],
    command_name: &str,
) -> CliResult<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(current_dir)
        .output()
        .map_err(|source| CliError::Io {
            context: format!("failed to start {command_name}"),
            source,
        })?;

    if !output.status.success() {
        return Err(git_command_error(
            command_name,
            output.status,
            output.stderr,
        ));
    }
    if output.stdout.len() > MAX_GIT_OUTPUT_BYTES {
        return Err(CliError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new("git returned too much output"),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| CliError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git returned non-UTF-8 output"),
    })
}

fn git_command_error(command: &str, status: ExitStatus, stderr: Vec<u8>) -> CliError {
    let stderr = String::from_utf8_lossy(&stderr);
    let message = if stderr.trim().is_empty() {
        "no error output".to_owned()
    } else {
        stderr.trim().to_owned()
    };

    CliError::ExternalCommand {
        command: command.to_owned(),
        status: command_status_text(status),
        stderr: SanitizedMessage::new(message),
    }
}

fn parse_url_remote(value: &str) -> CliResult<(String, String)> {
    let url = Url::parse(value).map_err(|source| CliError::InvalidArguments {
        message: format!("Git remote URL is not valid: {source}"),
    })?;

    if !matches!(url.scheme(), "http" | "https" | "ssh") {
        return invalid_remote("Git remote URL must use HTTPS or SSH");
    }
    if !url.username().is_empty() && url.scheme() != "ssh" {
        return invalid_remote("Git remote URL must not include credentials");
    }
    if url.password().is_some() {
        return invalid_remote("Git remote URL must not include credentials");
    }
    if url.query().is_some() || url.fragment().is_some() {
        return invalid_remote("Git remote URL must not include a query string or fragment");
    }

    let host = url
        .host_str()
        .filter(|host| !host.trim().is_empty())
        .ok_or_else(|| CliError::InvalidArguments {
            message: "Git remote URL must include a host".to_owned(),
        })?
        .to_ascii_lowercase();

    Ok((host, url.path().trim_start_matches('/').to_owned()))
}

fn parse_scp_like_remote(value: &str) -> CliResult<(String, String)> {
    let Some((host_part, path)) = value.split_once(':') else {
        return invalid_remote("Git remote URL must be HTTPS, SSH, or scp-like");
    };
    if host_part.contains('/') || path.starts_with('/') {
        return invalid_remote("Git remote URL must be a repository remote, not a local path");
    }
    if path.contains('?') || path.contains('#') {
        return invalid_remote("Git remote URL must not include a query string or fragment");
    }

    let host = host_part
        .rsplit_once('@')
        .map_or(host_part, |(_, host)| host)
        .trim();
    if host.is_empty() {
        return invalid_remote("Git remote URL must include a host");
    }

    Ok((host.to_ascii_lowercase(), path.to_owned()))
}

fn parse_repository_path(path: &str) -> CliResult<(String, String)> {
    let normalized = path.trim_matches('/');
    let mut components = normalized.split('/');
    let owner = components.next().unwrap_or_default();
    let repo = components.next().unwrap_or_default();

    if owner.is_empty() || repo.is_empty() || components.next().is_some() {
        return invalid_remote("Git remote URL path must have owner/repo form");
    }

    let name = repo.strip_suffix(".git").unwrap_or(repo);
    let owner = validate_remote_component("Git remote owner", owner)?;
    let name = validate_remote_component("Git remote repository name", name)?;

    Ok((owner, name))
}

fn validate_remote_name(value: &str) -> CliResult<String> {
    validate_remote_component("Git remote name", value)
}

fn validate_remote_component(label: &str, value: &str) -> CliResult<String> {
    if value.trim().is_empty() || value.trim().len() != value.len() {
        return Err(CliError::InvalidArguments {
            message: format!("{label} must not be blank or padded"),
        });
    }
    if value.chars().any(char::is_control) || value.chars().any(char::is_whitespace) {
        return Err(CliError::InvalidArguments {
            message: format!("{label} must not contain whitespace or control characters"),
        });
    }

    Ok(value.to_owned())
}

fn invalid_remote<T>(message: impl Into<String>) -> CliResult<T> {
    Err(CliError::InvalidArguments {
        message: message.into(),
    })
}

fn command_status_text(status: ExitStatus) -> String {
    status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| code.to_string(),
    )
}

fn redacted_remote_url(value: &str) -> String {
    match Url::parse(value) {
        Ok(mut url) if !url.username().is_empty() || url.password().is_some() => {
            let _ = url.set_username("REDACTED");
            let _ = url.set_password(Some("REDACTED"));
            url.to_string()
        }
        _ => value.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use tempfile::TempDir;

    use super::{GitRemote, GitRepository};
    use crate::CliError;

    #[test]
    fn parses_https_github_remote() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
            .expect("remote should parse");

        assert_eq!(remote.remote_name, "origin");
        assert_eq!(remote.host, "github.com");
        assert_eq!(remote.owner, "owner");
        assert_eq!(remote.name, "repo");
    }

    #[test]
    fn parses_ssh_and_scp_like_github_remotes() {
        let ssh = GitRemote::parse("upstream", "ssh://git@github.com/owner/repo.git")
            .expect("ssh remote should parse");
        let scp = GitRemote::parse("origin", "git@github.com:owner/repo.git")
            .expect("scp-like remote should parse");

        assert_eq!(ssh.host, "github.com");
        assert_eq!(ssh.owner, "owner");
        assert_eq!(ssh.name, "repo");
        assert_eq!(scp.host, "github.com");
        assert_eq!(scp.owner, "owner");
        assert_eq!(scp.name, "repo");
    }

    #[test]
    fn rejects_credentials_and_ambiguous_paths() {
        for url in [
            "https://token@github.com/owner/repo.git",
            "https://user:token@github.com/owner/repo.git",
            "https://github.com/owner/repo.git?token=secret",
            "https://github.com/owner/group/repo.git",
            "file:///tmp/repo.git",
            "../repo.git",
        ] {
            let error = GitRemote::parse("origin", url).expect_err("remote should be rejected");
            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }

    #[test]
    fn debug_redacts_credentialed_url_defensively() {
        let remote = GitRemote {
            remote_name: "origin".to_owned(),
            url: "https://user:secret@github.com/owner/repo.git".to_owned(),
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let rendered = format!("{remote:?}");

        assert!(rendered.contains("REDACTED"));
        assert!(!rendered.contains("secret"));
    }

    #[test]
    fn discovers_worktree_root_and_origin_remote_from_nested_directory() {
        let repo = TempGitRepo::new();
        repo.git(["remote", "add", "origin", "git@github.com:owner/repo.git"]);
        let nested = repo.path().join("nested/path");
        fs::create_dir_all(&nested).expect("nested test directory should be created");

        let detected = GitRepository::discover(&nested).expect("repository should be detected");

        assert_eq!(
            detected
                .worktree_root
                .canonicalize()
                .expect("detected root should canonicalize"),
            repo.path()
                .canonicalize()
                .expect("repo root should canonicalize")
        );
        assert_eq!(detected.remote.host, "github.com");
        assert_eq!(detected.remote.owner, "owner");
        assert_eq!(detected.remote.name, "repo");
    }

    #[test]
    fn reports_non_git_directory_as_git_command_failure() {
        let directory = TempDir::new().expect("temporary directory should be created");

        let error = GitRepository::discover(directory.path())
            .expect_err("non-Git directory should not be detected");

        assert!(matches!(error, CliError::ExternalCommand { .. }));
    }

    struct TempGitRepo {
        root: TempDir,
    }

    impl TempGitRepo {
        fn new() -> Self {
            let root = TempDir::new().expect("temporary repository should be created");
            let repo = Self { root };

            repo.git(["init"]);
            repo
        }

        fn path(&self) -> &Path {
            self.root.path()
        }

        fn git<const N: usize>(&self, args: [&str; N]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(self.path())
                .output()
                .expect("git command should start");

            assert!(
                output.status.success(),
                "git command failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
}
