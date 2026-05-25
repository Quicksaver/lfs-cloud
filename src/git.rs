//! Git repository discovery helpers for CLI commands.
//!
//! The `init` and migration commands need to understand the current checkout
//! before they can compute the configured LFS route. This module keeps that Git
//! process boundary narrow and parses repository remotes into provider-neutral
//! host/owner/name pieces without accepting credentials from remote URLs.

use std::{
    ffi::OsStr,
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
    /// Git worktree root path reported by `git rev-parse`.
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

    /// Returns the worktree `.lfsconfig` path used by committed Git LFS config.
    #[must_use]
    pub fn lfsconfig_path(&self) -> PathBuf {
        self.worktree_root.join(".lfsconfig")
    }

    /// Returns the repository-local Git config path resolved by Git.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git cannot resolve its local config path.
    pub fn local_git_config_path(&self) -> CliResult<PathBuf> {
        let output = git_stdout(
            &self.worktree_root,
            [
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "config",
            ],
            "git rev-parse --path-format=absolute --git-path config",
        )?;
        let path = PathBuf::from(output.trim_end());

        Ok(if path.is_absolute() {
            path
        } else {
            self.worktree_root.join(path)
        })
    }

    /// Writes the Git LFS URL either to `.lfsconfig` or to local Git config.
    ///
    /// The write is delegated to `git config` so Git's own config parser owns
    /// escaping and section formatting for both the committed and local-only
    /// targets.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the current value cannot be read or `git
    /// config` cannot persist the new value.
    pub fn write_lfs_url(
        &self,
        target: GitLfsConfigTarget,
        lfs_url: impl AsRef<str>,
    ) -> CliResult<GitLfsConfigChange> {
        let lfs_url = lfs_url.as_ref();
        let path = target.path(self)?;
        let previous_url = self.read_lfs_url(target)?;

        match target {
            GitLfsConfigTarget::WorktreeFile => {
                let lfsconfig_path = self.lfsconfig_path();
                run_git_config(
                    &self.worktree_root,
                    [
                        OsStr::new("config"),
                        OsStr::new("--file"),
                        lfsconfig_path.as_os_str(),
                        OsStr::new("lfs.url"),
                        OsStr::new(lfs_url),
                    ],
                    "git config --file .lfsconfig lfs.url",
                )?;
            }
            GitLfsConfigTarget::LocalRepository => {
                run_git_config(
                    &self.worktree_root,
                    [
                        OsStr::new("config"),
                        OsStr::new("--local"),
                        OsStr::new("lfs.url"),
                        OsStr::new(lfs_url),
                    ],
                    "git config --local lfs.url",
                )?;
            }
        }

        Ok(GitLfsConfigChange {
            target,
            path,
            previous_url,
            new_url: lfs_url.to_owned(),
        })
    }

    fn read_lfs_url(&self, target: GitLfsConfigTarget) -> CliResult<Option<String>> {
        match target {
            GitLfsConfigTarget::WorktreeFile => {
                let lfsconfig_path = self.lfsconfig_path();
                git_config_get(
                    &self.worktree_root,
                    [
                        OsStr::new("config"),
                        OsStr::new("--file"),
                        lfsconfig_path.as_os_str(),
                        OsStr::new("--get"),
                        OsStr::new("lfs.url"),
                    ],
                    "git config --file .lfsconfig --get lfs.url",
                )
            }
            GitLfsConfigTarget::LocalRepository => git_config_get(
                &self.worktree_root,
                [
                    OsStr::new("config"),
                    OsStr::new("--local"),
                    OsStr::new("--get"),
                    OsStr::new("lfs.url"),
                ],
                "git config --local --get lfs.url",
            ),
        }
    }
}

/// Target config location for `lfs-cloud init`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitLfsConfigTarget {
    /// Write the LFS URL to the worktree `.lfsconfig` file.
    WorktreeFile,
    /// Write the LFS URL to the repository-local `.git/config` file.
    LocalRepository,
}

impl GitLfsConfigTarget {
    /// Returns a user-facing label for this config target.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::WorktreeFile => ".lfsconfig",
            Self::LocalRepository => "local Git config",
        }
    }

    fn path(self, repository: &GitRepository) -> CliResult<PathBuf> {
        match self {
            Self::WorktreeFile => Ok(repository.lfsconfig_path()),
            Self::LocalRepository => repository.local_git_config_path(),
        }
    }
}

/// Result of writing a Git LFS URL into repository configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitLfsConfigChange {
    /// Config location that was updated.
    pub target: GitLfsConfigTarget,
    /// Filesystem path backing the updated config.
    pub path: PathBuf,
    /// Previous `lfs.url` value, when one was configured.
    pub previous_url: Option<String>,
    /// Newly configured `lfs.url` value.
    pub new_url: String,
}

/// A parsed Git remote suitable for constructing an LFS Cloud route.
#[derive(Clone, Eq, PartialEq)]
pub struct GitRemote {
    /// Git remote name, usually `origin`.
    pub remote_name: String,
    url: String,
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
        let host = validate_remote_host(&host)?;
        let (owner, name) = parse_repository_path(&path)?;

        Ok(Self {
            remote_name,
            url,
            host,
            owner,
            name,
        })
    }

    /// Returns the original remote URL as returned by Git.
    ///
    /// Values returned by this getter have been parsed and validated to reject
    /// credential-bearing HTTP(S) URLs and unsupported credential-like scp
    /// forms.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl fmt::Debug for GitRemote {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitRemote")
            .field("remote_name", &self.remote_name)
            .field("url", &redacted_url_for_display(&self.url))
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

fn git_config_get<const N: usize>(
    current_dir: &Path,
    args: [&OsStr; N],
    command_name: &str,
) -> CliResult<Option<String>> {
    let output = Command::new("git")
        .args(args)
        .current_dir(current_dir)
        .output()
        .map_err(|source| CliError::Io {
            context: format!("failed to start {command_name}"),
            source,
        })?;

    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }

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

    String::from_utf8(output.stdout)
        .map(|value| Some(value.trim_end().to_owned()))
        .map_err(|_| CliError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new("git returned non-UTF-8 output"),
        })
}

fn run_git_config<const N: usize>(
    current_dir: &Path,
    args: [&OsStr; N],
    command_name: &str,
) -> CliResult<()> {
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

    Ok(())
}

fn git_command_error(command: &str, status: ExitStatus, stderr: Vec<u8>) -> CliError {
    let stderr = truncated_lossy_message(&stderr);
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

    if !matches!(url.scheme(), "https" | "ssh") {
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

    let host = if let Some((user, host)) = host_part.rsplit_once('@') {
        validate_scp_like_user(user)?;
        host
    } else {
        host_part
    }
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
    let owner = validate_route_component("Git remote owner", owner, false)?;
    let name = validate_route_component("Git remote repository name", name, true)?;
    // A second `.git` suffix means the repository path was double-suffixed,
    // such as `owner/repo.git.git`.
    if name.ends_with(".git") {
        return invalid_remote("Git remote repository name must not contain a nested .git suffix");
    }

    Ok((owner, name))
}

fn validate_remote_name(value: &str) -> CliResult<String> {
    // Keep the semantic helper so call sites do not repeat the human-facing
    // error label for Git remote names.
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

fn validate_remote_host(value: &str) -> CliResult<String> {
    let host = validate_remote_component("Git remote host", value)?;
    if host.split('.').any(|label| {
        label.is_empty()
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return invalid_remote("Git remote host must be a route-safe host made of ASCII labels");
    }

    Ok(host)
}

fn validate_route_component(
    label: &str,
    value: &str,
    allow_leading_dot: bool,
) -> CliResult<String> {
    let component = validate_remote_component(label, value)?;
    if matches!(component.as_str(), "." | "..")
        || (!allow_leading_dot && component.starts_with('.'))
        || component.ends_with('.')
        || component.contains("..")
        || !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "{label} must be a route-safe repository component without separators, percent escapes, or traversal segments"
            ),
        });
    }

    Ok(component)
}

fn validate_scp_like_user(value: &str) -> CliResult<()> {
    if value != "git" {
        return invalid_remote("Git scp-like remote user must be git when present");
    }

    Ok(())
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

pub(crate) fn redacted_url_for_display(value: &str) -> String {
    match Url::parse(value) {
        Ok(mut url) => {
            let mut redacted = false;
            if !url.username().is_empty() {
                let _ = url.set_username("REDACTED");
                redacted = true;
            }
            if url.password().is_some() {
                let _ = url.set_password(Some("REDACTED"));
                redacted = true;
            }
            if url.query().is_some() {
                url.set_query(Some("REDACTED"));
                redacted = true;
            }
            if url.fragment().is_some() {
                url.set_fragment(Some("REDACTED"));
                redacted = true;
            }

            if redacted {
                url.to_string()
            } else {
                redact_url_parse_fallback_for_display(value)
            }
        }
        _ => redact_url_parse_fallback_for_display(value),
    }
}

fn redact_url_parse_fallback_for_display(value: &str) -> String {
    redact_scp_like_remote_url(value).unwrap_or_else(|| redact_query_fragment_for_display(value))
}

fn redact_scp_like_remote_url(value: &str) -> Option<String> {
    let (userinfo, rest) = value.rsplit_once('@')?;
    if userinfo.is_empty() || rest.is_empty() || !rest.contains(':') {
        return None;
    }

    Some(format!("REDACTED@{rest}"))
}

fn redact_query_fragment_for_display(value: &str) -> String {
    let query_index = value.find('?');
    let fragment_index = value.find('#');
    let Some(first_sensitive_index) = query_index.into_iter().chain(fragment_index).min() else {
        return value.to_owned();
    };

    let mut redacted = value[..first_sensitive_index].to_owned();
    if query_index.is_some_and(|index| fragment_index.is_none_or(|fragment| index < fragment)) {
        redacted.push_str("?REDACTED");
    }
    if fragment_index.is_some() {
        redacted.push_str("#REDACTED");
    }
    redacted
}

fn truncated_lossy_message(bytes: &[u8]) -> String {
    if bytes.len() <= MAX_GIT_OUTPUT_BYTES {
        return String::from_utf8_lossy(bytes).into_owned();
    }

    let mut message = String::from_utf8_lossy(&bytes[..MAX_GIT_OUTPUT_BYTES]).into_owned();
    message.push_str("\n[truncated]");
    message
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::{fs, path::Path, process::Command};

    use tempfile::TempDir;

    use super::{GitLfsConfigTarget, GitRemote, GitRepository, redacted_url_for_display};
    use crate::CliError;

    #[test]
    fn parses_https_github_remote() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/repo.git")
            .expect("remote should parse");

        assert_eq!(remote.remote_name, "origin");
        assert_eq!(remote.host, "github.com");
        assert_eq!(remote.owner, "owner");
        assert_eq!(remote.name, "repo");
        assert_eq!(remote.url(), "https://github.com/owner/repo.git");
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
    fn parses_dot_prefixed_github_repository_name() {
        let remote = GitRemote::parse("origin", "https://github.com/owner/.github.git")
            .expect("dot-prefixed repository should parse");

        assert_eq!(remote.owner, "owner");
        assert_eq!(remote.name, ".github");
    }

    #[test]
    fn rejects_credentials_and_ambiguous_paths() {
        for url in [
            "http://github.com/owner/repo.git",
            "https://token@github.com/owner/repo.git",
            "https://user:token@github.com/owner/repo.git",
            "https://github.com/owner/repo.git?token=secret",
            "https://github.com/owner/group/repo.git",
            "file:///tmp/repo.git",
            "../repo.git",
            "token@github.com:owner/repo.git",
        ] {
            let error = GitRemote::parse("origin", url).expect_err("remote should be rejected");
            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }

    #[test]
    fn rejects_route_unsafe_remote_components() {
        for url in [
            "git@github.com:../repo.git",
            "git@github.com:.hidden/repo.git",
            "git@github.com:owner/trailing..git",
            "git@github.com:owner/repo..git",
            "https://github.com/owner/foo.git.git",
            "https://github.com/owner/re%20po.git",
            "git@github..com:owner/repo.git",
        ] {
            let error = GitRemote::parse("origin", url).expect_err("remote should be rejected");
            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }

    #[test]
    fn debug_redacts_credentialed_url_defensively() {
        for url in [
            "https://user:secret@github.com/owner/repo.git",
            "token@github.com:owner/repo.git",
            "user:secret@github.com:owner/repo.git",
        ] {
            let remote = GitRemote {
                remote_name: "origin".to_owned(),
                url: url.to_owned(),
                host: "github.com".to_owned(),
                owner: "owner".to_owned(),
                name: "repo".to_owned(),
            };
            let rendered = format!("{remote:?}");

            assert!(rendered.contains("REDACTED"));
            assert!(!rendered.contains("secret"));
            assert!(!rendered.contains("token"));
        }
    }

    #[test]
    fn display_redaction_strips_unparseable_query_and_fragment() {
        let rendered = redacted_url_for_display("not a url?token=query-secret#fragment-secret");

        assert_eq!(rendered, "not a url?REDACTED#REDACTED");
        assert!(!rendered.contains("query-secret"));
        assert!(!rendered.contains("fragment-secret"));
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
    fn local_git_config_path_resolves_linked_worktree_gitdir() {
        let repo = TempGitRepo::new();
        repo.git([
            "-c",
            "user.name=LFS Cloud Test",
            "-c",
            "user.email=lfs-cloud@example.invalid",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let linked_worktree = repo.path().join("linked-worktree");
        repo.git([
            "worktree",
            "add",
            "-b",
            "linked-test",
            linked_worktree.to_str().expect("path should be UTF-8"),
            "HEAD",
        ]);
        let repository = GitRepository {
            worktree_root: linked_worktree.clone(),
            remote: GitRemote::parse("origin", "git@github.com:owner/repo.git")
                .expect("remote should parse"),
        };

        let path = repository
            .local_git_config_path()
            .expect("linked worktree config path should resolve");

        assert!(path.is_absolute());
        assert_eq!(
            path,
            repo.path()
                .join(".git/config")
                .canonicalize()
                .expect("main config should canonicalize")
        );
    }

    #[cfg(unix)]
    #[test]
    fn lfsconfig_permission_denial_is_not_treated_as_unset() {
        let repo = TempGitRepo::new();
        let lfsconfig = repo.path().join(".lfsconfig");
        fs::write(&lfsconfig, "[lfs]\n\turl = https://old.example/info/lfs\n")
            .expect("test .lfsconfig should be written");
        let mut permissions = fs::metadata(&lfsconfig)
            .expect("test .lfsconfig metadata should be readable")
            .permissions();
        permissions.set_mode(0o000);
        fs::set_permissions(&lfsconfig, permissions).expect("test .lfsconfig should be locked");
        let repository = GitRepository {
            worktree_root: repo.path().to_owned(),
            remote: GitRemote::parse("origin", "git@github.com:owner/repo.git")
                .expect("remote should parse"),
        };

        let error = repository
            .write_lfs_url(
                GitLfsConfigTarget::WorktreeFile,
                "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            )
            .expect_err("permission denial should be reported");

        let mut permissions = fs::metadata(&lfsconfig)
            .expect("test .lfsconfig metadata should still be readable")
            .permissions();
        permissions.set_mode(0o600);
        fs::set_permissions(&lfsconfig, permissions).expect("test .lfsconfig should be unlocked");
        assert!(matches!(error, CliError::ExternalCommand { .. }));
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
