//! Git credential-helper integration for local LFS Cloud tokens.
//!
//! The GitHub OAuth token stays inside provider-facing code. This module stores
//! only the short-lived local LFS Cloud token that Git LFS should use when it
//! contacts the configured LFS URL.

use std::{
    fmt,
    io::{Read, Write},
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc::{self, Receiver},
    thread,
    time::{Duration, Instant},
};

use url::Url;

use crate::{
    CliError, CliResult, LfsSessionToken, SanitizedMessage,
    http_transport::uses_protected_http_transport,
};

/// Username stored alongside local LFS Cloud bearer tokens in Git credentials.
pub const DEFAULT_GIT_CREDENTIAL_USERNAME: &str = "lfscloud";

const MAX_CREDENTIAL_FIELD_LEN: usize = 2048;
const MAX_CREDENTIAL_OUTPUT_LEN: usize = 8192;
const MAX_COMMAND_STDERR_LEN: usize = 4096;
const MAX_RETAINED_COMMAND_STDERR_LEN: usize = MAX_COMMAND_STDERR_LEN + MAX_CREDENTIAL_FIELD_LEN;
const CREDENTIAL_LOOKUP_STDERR_SUPPRESSED: &str = "credential helper stderr suppressed";
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_DRAIN_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(1);
const STDERR_DRAIN_AFTER_KILL_TIMEOUT: Duration = Duration::from_millis(100);
const PIPE_EVENTS_PER_DRAIN: usize = 64;

/// Credential-helper payload for approving one configured LFS URL.
#[derive(Clone, Eq, PartialEq)]
pub struct GitCredentialApproval {
    lfs_url: Url,
    username: String,
    token: LfsSessionToken,
}

/// Credential-helper payload for erasing one local LFS Cloud token.
///
/// The token is included in the helper protocol so helpers can reject the
/// exact repository-scoped credential without receiving it as a process
/// argument.
#[derive(Clone, Eq, PartialEq)]
pub struct GitCredentialRejection {
    lfs_url: Url,
    username: String,
    token: LfsSessionToken,
}

/// Credential retrieved from Git's credential helper for one LFS URL.
///
/// The token is restored as an [`LfsSessionToken`] so callers do not need to
/// handle raw helper output or accept arbitrary stored passwords as LFS Cloud
/// credentials.
#[derive(Clone, Eq, PartialEq)]
pub struct GitCredential {
    lfs_url: Url,
    username: String,
    token: LfsSessionToken,
}

impl GitCredential {
    /// Returns the configured LFS URL this credential was looked up for.
    #[must_use]
    pub fn lfs_url(&self) -> &Url {
        &self.lfs_url
    }

    /// Returns the non-secret credential username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Returns the validated local LFS Cloud session token.
    #[must_use]
    pub fn token(&self) -> &LfsSessionToken {
        &self.token
    }
}

impl fmt::Debug for GitCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitCredential")
            .field("lfs_url", &self.lfs_url.as_str())
            .field("username", &self.username)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Credential-helper lookup request for a configured LFS URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitCredentialLookup {
    lfs_url: Url,
    username: String,
}

impl GitCredentialLookup {
    /// Creates a credential lookup request with the default LFS Cloud username.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `lfs_url` is not an absolute HTTP(S) URL.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::GitCredentialLookup;
    ///
    /// let lookup = GitCredentialLookup::new(
    ///     "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
    /// )?;
    ///
    /// assert_eq!(lookup.lfs_url().host_str(), Some("lfs.example.com"));
    /// # Ok::<(), lfscloud::CliError>(())
    /// ```
    pub fn new(lfs_url: impl AsRef<str>) -> CliResult<Self> {
        Self::with_username(lfs_url, DEFAULT_GIT_CREDENTIAL_USERNAME)
    }

    /// Creates a lookup after an explicit CLI plaintext-HTTP opt-in.
    pub(crate) fn new_with_insecure_http(
        lfs_url: impl AsRef<str>,
        allow_insecure_http: bool,
    ) -> CliResult<Self> {
        Ok(Self {
            lfs_url: validate_lfs_credential_url(lfs_url.as_ref(), allow_insecure_http)?,
            username: DEFAULT_GIT_CREDENTIAL_USERNAME.to_owned(),
        })
    }

    /// Creates a credential lookup request with an explicit username.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `lfs_url` is invalid, or `username` is blank,
    /// padded, too long, or contains control characters.
    pub fn with_username(lfs_url: impl AsRef<str>, username: impl Into<String>) -> CliResult<Self> {
        Ok(Self {
            lfs_url: validate_lfs_credential_url(lfs_url.as_ref(), false)?,
            username: validate_credential_field("git credential username", username.into())?,
        })
    }

    /// Returns the LFS URL the credential will be looked up for.
    #[must_use]
    pub fn lfs_url(&self) -> &Url {
        &self.lfs_url
    }

    /// Returns the expected non-secret credential username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Retrieves and validates a local LFS Cloud credential through
    /// `git credential fill`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `git` cannot be started, stdin cannot be
    /// written, the helper exits unsuccessfully, or the returned credential is
    /// not scoped to the configured LFS URL and username.
    pub fn lookup(&self) -> CliResult<GitCredential> {
        self.lookup_in_dir(Path::new("."))
    }

    /// Retrieves a credential in an explicit repository context.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git cannot be started, the helper fails, or
    /// the returned credential does not match the configured URL and username.
    pub fn lookup_in_dir(&self, repository_dir: impl AsRef<Path>) -> CliResult<GitCredential> {
        self.lookup_with_git_program_in_dir(Path::new("git"), repository_dir)
    }

    /// Retrieves and validates a credential with a caller-selected Git
    /// executable.
    ///
    /// This is primarily for tests that inject a fake `git` executable while
    /// preserving the exact stdin protocol used by the real helper.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the process cannot be started, stdin cannot be
    /// written, the helper exits unsuccessfully, or the returned credential is
    /// not a valid local LFS Cloud credential for this URL.
    pub fn lookup_with_git_program(
        &self,
        git_program: impl AsRef<Path>,
    ) -> CliResult<GitCredential> {
        self.lookup_with_git_program_in_dir(git_program, Path::new("."))
    }

    /// Retrieves a credential in an explicit repository context with a
    /// caller-selected Git executable.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the process cannot be started, the helper
    /// fails, or the returned credential is invalid for this request.
    pub fn lookup_with_git_program_in_dir(
        &self,
        git_program: impl AsRef<Path>,
        repository_dir: impl AsRef<Path>,
    ) -> CliResult<GitCredential> {
        let command_name = "git credential fill";
        let mut command = git_command(git_program.as_ref());
        // A lookup is a read-only cache probe. Disable every standard Git and
        // Git Credential Manager prompt path so a miss fails instead of
        // blocking an unattended CLI command or opening credential UI.
        command
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_ASKPASS")
            .env_remove("SSH_ASKPASS")
            .env("GCM_INTERACTIVE", "0")
            .env("GCM_GUI_PROMPT", "0")
            .args(["-c", "core.askPass=", "-c", "credential.interactive=false"]);
        let mut child = command
            .args(["credential", "fill"])
            .current_dir(repository_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // A failing helper can echo the stored password before stdout is
            // parsed, so there is no secret value available for redaction.
            .stderr(Stdio::null())
            .spawn()
            .map_err(|source| CliError::Io {
                context: format!("failed to start {command_name}"),
                source,
            })?;

        {
            let mut stdin = child.stdin.take().ok_or_else(|| CliError::Io {
                context: format!("failed to open stdin for {command_name}"),
                source: std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "child stdin unavailable",
                ),
            })?;
            stdin
                .write_all(self.credential_query_input().as_bytes())
                .map_err(|source| CliError::Io {
                    context: format!("failed to write credential query to {command_name}"),
                    source,
                })?;
        }

        let (status, stdout, _) =
            wait_for_git_command_output(&mut child, command_name, "", GIT_COMMAND_TIMEOUT)
                .map_err(suppress_credential_lookup_stderr)?;

        if !status.success() {
            return Err(credential_lookup_command_error(
                command_name.to_owned(),
                command_status_text(status),
            ));
        }

        parse_git_credential_fill_output(&self.lfs_url, &self.username, &stdout)
    }

    fn credential_query_input(&self) -> String {
        let mut input = format!(
            "protocol={}\nhost={}\n",
            self.lfs_url.scheme(),
            credential_host_field(&self.lfs_url)
        );
        if let Some(path) = expected_credential_path(&self.lfs_url) {
            input.push_str("path=");
            input.push_str(&path);
            input.push('\n');
        }
        input.push_str("username=");
        input.push_str(&self.username);
        input.push_str("\n\n");
        input
    }
}

impl GitCredentialRejection {
    /// Creates a rejection for the default LFS Cloud credential username.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::{GitCredentialRejection, LfsSessionToken};
    ///
    /// let rejection = GitCredentialRejection::new(
    ///     "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
    ///     LfsSessionToken::from_secret("local-lfs-token")?,
    /// )?;
    ///
    /// assert_eq!(rejection.username(), "lfscloud");
    /// # Ok::<(), lfscloud::LfsCloudError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `lfs_url` is not a protected absolute HTTP(S)
    /// URL.
    pub fn new(lfs_url: impl AsRef<str>, token: LfsSessionToken) -> CliResult<Self> {
        Self::with_username(lfs_url, DEFAULT_GIT_CREDENTIAL_USERNAME, token)
    }

    /// Creates a rejection after an explicit plaintext-HTTP CLI opt-in.
    pub(crate) fn new_with_insecure_http(
        lfs_url: impl AsRef<str>,
        token: LfsSessionToken,
        allow_insecure_http: bool,
    ) -> CliResult<Self> {
        Ok(Self {
            lfs_url: validate_lfs_credential_url(lfs_url.as_ref(), allow_insecure_http)?,
            username: DEFAULT_GIT_CREDENTIAL_USERNAME.to_owned(),
            token,
        })
    }

    /// Creates a rejection with an explicit credential username.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the URL or username is invalid.
    pub fn with_username(
        lfs_url: impl AsRef<str>,
        username: impl Into<String>,
        token: LfsSessionToken,
    ) -> CliResult<Self> {
        Ok(Self {
            lfs_url: validate_lfs_credential_url(lfs_url.as_ref(), false)?,
            username: validate_credential_field("git credential username", username.into())?,
            token,
        })
    }

    /// Returns the LFS URL whose credential will be erased.
    #[must_use]
    pub fn lfs_url(&self) -> &Url {
        &self.lfs_url
    }

    /// Returns the non-secret credential username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Returns the local token used to select the exact credential.
    #[must_use]
    pub fn token(&self) -> &LfsSessionToken {
        &self.token
    }

    /// Erases the credential through `git credential reject`.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git cannot be started, stdin cannot be
    /// written, or the credential helper exits unsuccessfully.
    pub fn reject(&self) -> CliResult<()> {
        self.reject_in_dir(Path::new("."))
    }

    /// Erases the credential in an explicit repository context.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git cannot be started, stdin cannot be
    /// written, or the credential helper exits unsuccessfully.
    pub fn reject_in_dir(&self, repository_dir: impl AsRef<Path>) -> CliResult<()> {
        self.reject_with_git_program_in_dir(Path::new("git"), repository_dir)
    }

    /// Erases the credential with a caller-selected Git executable.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the process cannot complete successfully.
    pub fn reject_with_git_program(&self, git_program: impl AsRef<Path>) -> CliResult<()> {
        self.reject_with_git_program_in_dir(git_program, Path::new("."))
    }

    /// Erases the credential in an explicit repository context with a
    /// caller-selected Git executable.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the process cannot be started, stdin cannot be
    /// written, or the credential helper exits unsuccessfully.
    pub fn reject_with_git_program_in_dir(
        &self,
        git_program: impl AsRef<Path>,
        repository_dir: impl AsRef<Path>,
    ) -> CliResult<()> {
        let command_name = "git credential reject";
        let mut command = git_command(git_program.as_ref());
        let mut child = command
            .args(["credential", "reject"])
            .current_dir(repository_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| CliError::Io {
                context: format!("failed to start {command_name}"),
                source,
            })?;

        {
            let mut stdin = child.stdin.take().ok_or_else(|| CliError::Io {
                context: format!("failed to open stdin for {command_name}"),
                source: std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "child stdin unavailable",
                ),
            })?;
            stdin
                .write_all(self.credential_input().as_bytes())
                .map_err(|source| CliError::Io {
                    context: format!("failed to write credential to {command_name}"),
                    source,
                })?;
        }

        let (status, stderr) = wait_for_git_command(&mut child, command_name, self.token.as_str())?;
        if status.success() {
            return Ok(());
        }

        Err(CliError::ExternalCommand {
            command: command_name.to_owned(),
            status: command_status_text(status),
            stderr: sanitize_command_stderr(&stderr, self.token.as_str()),
        })
    }

    fn credential_input(&self) -> String {
        format!(
            "url={}\nusername={}\npassword={}\n\n",
            self.lfs_url.as_str(),
            self.username,
            self.token.as_str()
        )
    }
}

impl fmt::Debug for GitCredentialRejection {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitCredentialRejection")
            .field("lfs_url", &self.lfs_url.as_str())
            .field("username", &self.username)
            .field("token", &"<redacted>")
            .finish()
    }
}

impl GitCredentialApproval {
    /// Creates a credential approval payload with the default LFS Cloud username.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `lfs_url` is not an absolute HTTP(S) URL.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::{GitCredentialApproval, LfsSessionToken};
    ///
    /// let approval = GitCredentialApproval::new(
    ///     "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
    ///     LfsSessionToken::from_secret("local-lfs-token")?,
    /// )?;
    ///
    /// assert_eq!(approval.lfs_url().host_str(), Some("lfs.example.com"));
    /// # Ok::<(), lfscloud::LfsCloudError>(())
    /// ```
    pub fn new(lfs_url: impl AsRef<str>, token: LfsSessionToken) -> CliResult<Self> {
        Self::with_username(lfs_url, DEFAULT_GIT_CREDENTIAL_USERNAME, token)
    }

    /// Creates an approval after an explicit CLI plaintext-HTTP opt-in.
    pub(crate) fn new_with_insecure_http(
        lfs_url: impl AsRef<str>,
        token: LfsSessionToken,
        allow_insecure_http: bool,
    ) -> CliResult<Self> {
        Ok(Self {
            lfs_url: validate_lfs_credential_url(lfs_url.as_ref(), allow_insecure_http)?,
            username: DEFAULT_GIT_CREDENTIAL_USERNAME.to_owned(),
            token,
        })
    }

    /// Creates a credential approval payload with an explicit username.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `lfs_url` is not an absolute HTTP(S) URL, or
    /// `username` is blank, padded, too long, or contains control characters.
    pub fn with_username(
        lfs_url: impl AsRef<str>,
        username: impl Into<String>,
        token: LfsSessionToken,
    ) -> CliResult<Self> {
        Ok(Self {
            lfs_url: validate_lfs_credential_url(lfs_url.as_ref(), false)?,
            username: validate_credential_field("git credential username", username.into())?,
            token,
        })
    }

    /// Returns the LFS URL the credential will be approved for.
    #[must_use]
    pub fn lfs_url(&self) -> &Url {
        &self.lfs_url
    }

    /// Returns the non-secret credential username.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// Returns the local LFS Cloud session token to approve.
    ///
    /// This is a local LFS Cloud token, not an upstream GitHub OAuth token.
    #[must_use]
    pub fn token(&self) -> &LfsSessionToken {
        &self.token
    }

    /// Approves the credential through `git credential approve`.
    ///
    /// The token is written on standard input, not passed as a process
    /// argument, so process listings do not expose the secret. Before storing
    /// the credential, this persists path-aware lookup for the LFS Cloud host so
    /// future Git LFS credential fills also keep repository paths separate.
    /// This writes `credential.<lfs-host>.useHttpPath=true` to the repository's
    /// local Git config because Git LFS resolves credentials in later processes.
    /// Repository scope prevents a pre-existing local `false` value from
    /// overriding path isolation when the helper stores the token.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git has no configured credential helper, `git`
    /// is not running in a repository, cannot be started, stdin cannot be
    /// written, or the credential helper exits unsuccessfully.
    pub fn approve(&self) -> CliResult<()> {
        self.approve_in_dir(Path::new("."))
    }

    /// Approves the credential in an explicit repository context.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git has no configured credential helper, the
    /// directory is not in a repository, Git cannot be started, stdin cannot be
    /// written, or the helper exits unsuccessfully.
    pub fn approve_in_dir(&self, repository_dir: impl AsRef<Path>) -> CliResult<()> {
        self.approve_with_git_program_in_dir(Path::new("git"), repository_dir)
    }

    /// Approves the credential with a caller-selected Git executable.
    ///
    /// This is primarily for tests that inject a fake `git` executable while
    /// preserving the exact stdin protocol used by the real helper.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git has no configured credential helper, the
    /// process is not running in a repository, cannot be started, stdin cannot
    /// be written, or the helper exits unsuccessfully.
    pub fn approve_with_git_program(&self, git_program: impl AsRef<Path>) -> CliResult<()> {
        self.approve_with_git_program_in_dir(git_program, Path::new("."))
    }

    /// Approves the credential in an explicit repository context with a
    /// caller-selected Git executable.
    ///
    /// This is primarily for callers and tests that already resolved the
    /// repository independently of the process working directory.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when Git has no configured credential helper, the
    /// directory is not in a repository, the process cannot be started, stdin
    /// cannot be written, or the helper exits unsuccessfully.
    pub fn approve_with_git_program_in_dir(
        &self,
        git_program: impl AsRef<Path>,
        repository_dir: impl AsRef<Path>,
    ) -> CliResult<()> {
        let git_program = git_program.as_ref();
        let repository_dir = repository_dir.as_ref();
        self.ensure_credential_helper_configured(git_program, repository_dir)?;
        self.persist_path_aware_lookup(git_program, repository_dir)?;
        self.approve_with_configured_git(git_program, repository_dir)
    }

    fn ensure_credential_helper_configured(
        &self,
        git_program: &Path,
        repository_dir: &Path,
    ) -> CliResult<()> {
        let command_name = format!(
            "git config --get-urlmatch credential.helper {}",
            self.lfs_url
        );
        let mut command = git_command(git_program);
        let mut child = command
            .args([
                "config",
                "--get-urlmatch",
                "credential.helper",
                self.lfs_url.as_str(),
            ])
            .current_dir(repository_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| CliError::Io {
                context: format!("failed to start {command_name}"),
                source,
            })?;

        let (status, stdout, stderr) = wait_for_git_command_output(
            &mut child,
            &command_name,
            self.token.as_str(),
            GIT_COMMAND_TIMEOUT,
        )?;

        if status.success() {
            if git_config_output_has_helper(&stdout, &command_name)? {
                return Ok(());
            }

            return Err(missing_credential_helper_error(
                &self.lfs_url,
                &self.username,
            ));
        }

        if status.code() == Some(1) && stdout.is_empty() {
            return Err(missing_credential_helper_error(
                &self.lfs_url,
                &self.username,
            ));
        }

        Err(CliError::ExternalCommand {
            command: command_name,
            status: command_status_text(status),
            stderr: sanitize_command_stderr(&stderr, self.token.as_str()),
        })
    }

    fn persist_path_aware_lookup(
        &self,
        git_program: &Path,
        repository_dir: &Path,
    ) -> CliResult<()> {
        let credential_scope = credential_host_scope(&self.lfs_url);
        let config_key = format!("credential.{credential_scope}.useHttpPath");
        let command_name = format!("git config --local {config_key} true");
        let mut command = git_command(git_program);
        let mut child = command
            .args(["config", "--local", &config_key, "true"])
            .current_dir(repository_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| CliError::Io {
                context: format!("failed to start {command_name}"),
                source,
            })?;
        let (status, stderr) =
            wait_for_git_command(&mut child, &command_name, self.token.as_str())?;

        if status.success() {
            return Ok(());
        }

        Err(CliError::ExternalCommand {
            command: command_name,
            status: command_status_text(status),
            stderr: sanitize_command_stderr(&stderr, self.token.as_str()),
        })
    }

    fn approve_with_configured_git(
        &self,
        git_program: &Path,
        repository_dir: &Path,
    ) -> CliResult<()> {
        let command_name = "git credential approve";
        let mut command = git_command(git_program);
        let mut child = command
            .args(["credential", "approve"])
            .current_dir(repository_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| CliError::Io {
                context: format!("failed to start {command_name}"),
                source,
            })?;

        {
            let mut stdin = child.stdin.take().ok_or_else(|| CliError::Io {
                context: format!("failed to open stdin for {command_name}"),
                source: std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "child stdin unavailable",
                ),
            })?;
            stdin
                .write_all(self.credential_input().as_bytes())
                .map_err(|source| CliError::Io {
                    context: format!("failed to write credential to {command_name}"),
                    source,
                })?;
        }

        let (status, stderr) = wait_for_git_command(&mut child, command_name, self.token.as_str())?;

        if status.success() {
            return Ok(());
        }

        Err(CliError::ExternalCommand {
            command: command_name.to_owned(),
            status: command_status_text(status),
            stderr: sanitize_command_stderr(&stderr, self.token.as_str()),
        })
    }

    fn credential_input(&self) -> String {
        format!(
            "url={}\nusername={}\npassword={}\n\n",
            self.lfs_url.as_str(),
            self.username,
            self.token.as_str()
        )
    }
}

/// Builds recovery instructions for systems without a Git credential helper.
///
/// The returned text is suitable for CLI error output because it contains only
/// the configured LFS URL and static setup commands. It never asks users to
/// paste GitHub OAuth tokens or personal access tokens into Git LFS.
///
/// # Errors
///
/// Returns [`CliError`] when `lfs_url` is not an absolute HTTP(S) URL accepted
/// by LFS Cloud credential storage.
///
/// # Examples
///
/// ```
/// use lfscloud::git_credential_helper_fallback_instructions;
///
/// let instructions = git_credential_helper_fallback_instructions(
///     "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
/// )?;
///
/// assert!(instructions.contains("credential.helper"));
/// # Ok::<(), lfscloud::CliError>(())
/// ```
pub fn git_credential_helper_fallback_instructions(lfs_url: impl AsRef<str>) -> CliResult<String> {
    let lfs_url = validate_lfs_credential_url(lfs_url.as_ref(), false)?;
    Ok(credential_helper_fallback_instructions(
        &lfs_url,
        DEFAULT_GIT_CREDENTIAL_USERNAME,
    ))
}

fn missing_credential_helper_error(lfs_url: &Url, username: &str) -> CliError {
    CliError::GitCredentialHelperNotConfigured {
        lfs_url: lfs_url.as_str().to_owned(),
        instructions: SanitizedMessage::new(credential_helper_fallback_instructions(
            lfs_url, username,
        )),
    }
}

fn credential_helper_fallback_instructions(lfs_url: &Url, username: &str) -> String {
    [
        "Configure a Git credential helper, then retry the lfscloud login or init command."
            .to_owned(),
        "Recommended persistent helpers:".to_owned(),
        "  macOS:   git config --global credential.helper osxkeychain".to_owned(),
        "  Windows: git config --global credential.helper manager".to_owned(),
        "           Git Credential Manager may also be installed as manager-core.".to_owned(),
        "  Linux:   git config --global credential.helper libsecret".to_owned(),
        "           Exact helper names vary by distribution and desktop environment.".to_owned(),
        "Short-lived in-memory fallback:".to_owned(),
        "  git config --global credential.helper 'cache --timeout=3600'".to_owned(),
        "  This writes a global Git helper setting; cached credentials expire after the timeout.".to_owned(),
        "Avoid plaintext storage unless you deliberately accept unencrypted credentials on disk."
            .to_owned(),
        format!(
            "After a helper is configured, LFS Cloud will store username '{username}' for {lfs_url}."
        ),
        "Do not store a GitHub OAuth token or personal access token here; Git LFS should receive only the local LFS Cloud session token.".to_owned(),
    ]
    .join("\n")
}

fn git_config_output_has_helper(stdout: &[u8], command_name: &str) -> CliResult<bool> {
    let output = std::str::from_utf8(stdout).map_err(|_| CliError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git config returned non-UTF-8 credential helper output"),
    })?;

    // An empty helper entry intentionally clears Git's helper chain; treat blank
    // output as missing so token approval never silently stores nowhere.
    Ok(output.lines().any(|line| !line.trim().is_empty()))
}

fn credential_host_scope(url: &Url) -> String {
    let mut scope = url.clone();
    // Git matches this URL-shaped credential section with a trailing slash; the
    // manual credential-helper check protects this exact global config key.
    scope.set_path("");
    scope.set_query(None);
    scope.set_fragment(None);
    scope.to_string()
}

impl fmt::Debug for GitCredentialApproval {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitCredentialApproval")
            .field("lfs_url", &self.lfs_url.as_str())
            .field("username", &self.username)
            .field("token", &"<redacted>")
            .finish()
    }
}

fn validate_lfs_credential_url(value: &str, allow_insecure_http: bool) -> CliResult<Url> {
    let url = Url::parse(value).map_err(|source| CliError::InvalidArguments {
        message: format!("configured LFS URL is not valid: {source}"),
    })?;

    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(CliError::InvalidArguments {
            message: "configured LFS URL must be an absolute http(s) URL".to_owned(),
        });
    }

    if url.fragment().is_some() {
        return Err(CliError::InvalidArguments {
            message: "configured LFS URL must not include a fragment".to_owned(),
        });
    }

    if !url.username().is_empty() || url.password().is_some() {
        return Err(CliError::InvalidArguments {
            message: "configured LFS URL must not include a username or password".to_owned(),
        });
    }

    if url.query().is_some() {
        return Err(CliError::InvalidArguments {
            message: "configured LFS URL must not include a query string".to_owned(),
        });
    }
    if !allow_insecure_http && !uses_protected_http_transport(&url) {
        return Err(CliError::InvalidArguments {
            message: "configured LFS URL must use HTTPS unless it targets an exact loopback IP; pass --allow-insecure-http only on a trusted development network".to_owned(),
        });
    }

    Ok(url)
}

fn validate_credential_field(label: &str, value: String) -> CliResult<String> {
    if value.len() > MAX_CREDENTIAL_FIELD_LEN {
        return Err(CliError::InvalidArguments {
            message: format!("{label} must not exceed {MAX_CREDENTIAL_FIELD_LEN} bytes"),
        });
    }

    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() != value.len() {
        return Err(CliError::InvalidArguments {
            message: format!("{label} must not be blank or padded"),
        });
    }

    if value.chars().any(char::is_control) {
        return Err(CliError::InvalidArguments {
            message: format!("{label} must not contain control characters"),
        });
    }

    Ok(value)
}

fn parse_git_credential_fill_output(
    lfs_url: &Url,
    expected_username: &str,
    stdout: &[u8],
) -> CliResult<GitCredential> {
    if stdout.len() > MAX_CREDENTIAL_OUTPUT_LEN {
        return Err(invalid_credential_output(
            "git credential fill returned too much output",
        ));
    }

    let output = std::str::from_utf8(stdout)
        .map_err(|_| invalid_credential_output("git credential fill returned non-UTF-8 output"))?;
    let fields = GitCredentialFields::parse(output)?;

    if fields.password.is_none() {
        return Err(invalid_credential_output(
            "git credential fill did not return a password",
        ));
    }

    if fields.protocol.as_deref() != Some(lfs_url.scheme())
        || fields.host.as_deref() != Some(&credential_host_field(lfs_url))
        || fields.username.as_deref() != Some(expected_username)
        || normalize_credential_path(fields.path.as_deref()).as_deref()
            != expected_credential_path(lfs_url).as_deref()
    {
        return Err(invalid_credential_output(
            "git credential fill returned a credential for a different LFS URL or username",
        ));
    }

    let token = LfsSessionToken::from_secret(fields.password.expect("password was checked"))
        .map_err(|source| {
            invalid_credential_output(format!(
                "git credential fill returned an invalid local LFS token: {source}"
            ))
        })?;

    Ok(GitCredential {
        lfs_url: lfs_url.clone(),
        username: expected_username.to_owned(),
        token,
    })
}

#[derive(Default)]
struct GitCredentialFields {
    protocol: Option<String>,
    host: Option<String>,
    path: Option<String>,
    username: Option<String>,
    password: Option<String>,
}

impl GitCredentialFields {
    fn parse(output: &str) -> CliResult<Self> {
        let mut fields = Self::default();
        for line in output.lines() {
            if line.is_empty() {
                break;
            }

            let Some((key, value)) = line.split_once('=') else {
                return Err(invalid_credential_output(
                    "git credential fill returned malformed output",
                ));
            };

            match key {
                "protocol" => fields.protocol = Some(value.to_owned()),
                "host" => fields.host = Some(value.to_owned()),
                "path" => fields.path = Some(value.to_owned()),
                "username" => fields.username = Some(value.to_owned()),
                "password" => fields.password = Some(value.to_owned()),
                _ => {}
            }
        }

        Ok(fields)
    }
}

fn credential_host_field(url: &Url) -> String {
    let host = url
        .host_str()
        .expect("credential URL validation requires a host");
    if let Some(port) = url.port() {
        format!("{host}:{port}")
    } else {
        host.to_owned()
    }
}

fn expected_credential_path(url: &Url) -> Option<String> {
    normalize_credential_path(Some(url.path()))
}

fn normalize_credential_path(path: Option<&str>) -> Option<String> {
    let path = path?.trim_start_matches('/');
    if path.is_empty() {
        None
    } else {
        Some(path.to_owned())
    }
}

fn invalid_credential_output(message: impl Into<String>) -> CliError {
    CliError::ExternalCommandOutput {
        command: "git credential fill".to_owned(),
        message: SanitizedMessage::new(message.into()),
    }
}

fn credential_lookup_command_error(command: String, status: String) -> CliError {
    CliError::ExternalCommand {
        command,
        status,
        stderr: SanitizedMessage::new(CREDENTIAL_LOOKUP_STDERR_SUPPRESSED),
    }
}

fn suppress_credential_lookup_stderr(error: CliError) -> CliError {
    match error {
        CliError::ExternalCommand {
            command, status, ..
        } => credential_lookup_command_error(command, status),
        other => other,
    }
}

fn sanitize_command_stderr(stderr: &[u8], token: &str) -> SanitizedMessage {
    let mut message = String::from_utf8_lossy(stderr).into_owned();
    if !token.is_empty() {
        // Redact even short restored tokens; avoiding secret disclosure is more
        // important than preserving every character of helper diagnostics.
        message = message.replace(token, "<redacted>");
    }
    message = message.replace(['\r', '\n'], " ");
    if message.len() > MAX_COMMAND_STDERR_LEN {
        let boundary = (0..=MAX_COMMAND_STDERR_LEN)
            .rev()
            .find(|&index| message.is_char_boundary(index))
            .expect("zero is always a valid string boundary");
        message.truncate(boundary);
        message.push_str("...");
    }
    let message = message.trim();

    if message.is_empty() {
        SanitizedMessage::new("<no stderr>")
    } else {
        SanitizedMessage::new(message.to_owned())
    }
}

fn wait_for_git_command(
    child: &mut Child,
    command_name: &str,
    token: &str,
) -> CliResult<(ExitStatus, Vec<u8>)> {
    wait_for_git_command_timeout(child, command_name, token, GIT_COMMAND_TIMEOUT)
}

fn wait_for_git_command_timeout(
    child: &mut Child,
    command_name: &str,
    token: &str,
    timeout: Duration,
) -> CliResult<(ExitStatus, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    let mut stderr = Vec::new();
    let mut stderr_reader = child.stderr.take().map(PipeReader::new);

    loop {
        drain_available_stderr(&mut stderr_reader, &mut stderr, command_name)?;

        if let Some(status) = child.try_wait().map_err(|source| CliError::Io {
            context: format!("failed to wait for {command_name}"),
            source,
        })? {
            finish_stderr_reader_after_child_exit(
                child,
                &mut stderr_reader,
                &mut stderr,
                command_name,
            )?;
            return Ok((status, stderr));
        }

        if Instant::now() >= deadline {
            stop_timed_out_child(child, command_name)?;
            let _ = child.wait();
            drain_stderr_until(
                &mut stderr_reader,
                &mut stderr,
                command_name,
                STDERR_DRAIN_AFTER_KILL_TIMEOUT,
            )?;
            join_pipe_reader(&mut stderr_reader, command_name)?;
            return Err(CliError::ExternalCommand {
                command: command_name.to_owned(),
                status: format!("timed out after {} seconds", timeout.as_secs()),
                stderr: sanitize_command_stderr(&stderr, token),
            });
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn wait_for_git_command_output(
    child: &mut Child,
    command_name: &str,
    stderr_secret: &str,
    timeout: Duration,
) -> CliResult<(ExitStatus, Vec<u8>, Vec<u8>)> {
    let deadline = Instant::now() + timeout;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut stdout_reader = child.stdout.take().map(PipeReader::new);
    let mut stderr_reader = child.stderr.take().map(PipeReader::new);

    loop {
        drain_available_pipe(
            &mut stdout_reader,
            &mut stdout,
            command_name,
            retain_stdout_data,
        )?;
        drain_available_pipe(
            &mut stderr_reader,
            &mut stderr,
            command_name,
            retain_stderr_data,
        )?;

        if let Some(status) = child.try_wait().map_err(|source| CliError::Io {
            context: format!("failed to wait for {command_name}"),
            source,
        })? {
            finish_output_readers_after_child_exit(
                child,
                &mut stdout_reader,
                &mut stdout,
                &mut stderr_reader,
                &mut stderr,
                command_name,
            )?;
            return Ok((status, stdout, stderr));
        }

        if Instant::now() >= deadline {
            stop_timed_out_child(child, command_name)?;
            let _ = child.wait();
            drain_pipe_until(
                &mut stdout_reader,
                &mut stdout,
                command_name,
                retain_stdout_data,
                STDERR_DRAIN_AFTER_KILL_TIMEOUT,
            )?;
            drain_pipe_until(
                &mut stderr_reader,
                &mut stderr,
                command_name,
                retain_stderr_data,
                STDERR_DRAIN_AFTER_KILL_TIMEOUT,
            )?;
            join_pipe_reader(&mut stdout_reader, command_name)?;
            join_pipe_reader(&mut stderr_reader, command_name)?;
            return Err(CliError::ExternalCommand {
                command: command_name.to_owned(),
                status: format!("timed out after {} seconds", timeout.as_secs()),
                stderr: sanitize_command_stderr(&stderr, stderr_secret),
            });
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn git_command(git_program: &Path) -> Command {
    let mut command = Command::new(git_program);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        // Git may invoke credential helpers that fork. A dedicated process
        // group gives the command boundary deterministic ownership of those
        // descendants even after the direct Git child has exited.
        command.process_group(0);
    }
    command
}

fn stop_timed_out_child(child: &mut Child, command_name: &str) -> CliResult<()> {
    stop_child_process_tree(child);

    if child
        .try_wait()
        .map_err(|source| CliError::Io {
            context: format!("failed to wait for timed-out {command_name}"),
            source,
        })?
        .is_none()
    {
        child.kill().map_err(|source| CliError::Io {
            context: format!("failed to stop timed-out {command_name}"),
            source,
        })?;
    }

    Ok(())
}

#[cfg(unix)]
fn stop_child_process_tree(child: &Child) {
    signal_process_group("TERM", child.id());
    thread::sleep(Duration::from_millis(50));
    signal_process_group("KILL", child.id());
}

#[cfg(unix)]
fn signal_process_group(signal: &str, process_group_id: u32) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{process_group_id}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn stop_child_process_tree(child: &Child) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn stop_child_process_tree(_child: &Child) {}

struct PipeReader {
    receiver: Receiver<PipeEvent>,
    reader_thread: Option<thread::JoinHandle<()>>,
    closed: bool,
}

impl PipeReader {
    fn new(mut pipe: impl Read + Send + 'static) -> Self {
        let (sender, receiver) = mpsc::sync_channel(PIPE_EVENTS_PER_DRAIN);
        let reader_thread = thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => {
                        let _ = sender.send(PipeEvent::Closed);
                        break;
                    }
                    Ok(count) => {
                        if sender
                            .send(PipeEvent::Data(buffer[..count].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(source) => {
                        let _ = sender.send(PipeEvent::Error(source));
                        break;
                    }
                }
            }
        });

        Self {
            receiver,
            reader_thread: Some(reader_thread),
            closed: false,
        }
    }

    fn join(&mut self, command_name: &str) -> CliResult<()> {
        let Some(reader_thread) = self.reader_thread.take() else {
            return Ok(());
        };
        reader_thread.join().map_err(|_| CliError::Io {
            context: format!("pipe reader thread panicked for {command_name}"),
            source: std::io::Error::other("pipe reader thread panicked"),
        })
    }
}

enum PipeEvent {
    Data(Vec<u8>),
    Error(std::io::Error),
    Closed,
}

fn finish_stderr_reader_after_child_exit(
    child: &Child,
    reader: &mut Option<PipeReader>,
    stderr: &mut Vec<u8>,
    command_name: &str,
) -> CliResult<()> {
    drain_stderr_until(
        reader,
        stderr,
        command_name,
        STDERR_DRAIN_AFTER_KILL_TIMEOUT,
    )?;
    if pipe_reader_is_open(reader) {
        // A pipe that remains open after the direct child exits is owned by a
        // descendant. Stop the command's remaining process tree before waiting
        // for EOF so no blocked reader thread is detached on return.
        stop_child_process_tree(child);
    }
    drain_stderr_until(
        reader,
        stderr,
        command_name,
        STDERR_DRAIN_AFTER_EXIT_TIMEOUT,
    )?;
    join_pipe_reader(reader, command_name)
}

fn finish_output_readers_after_child_exit(
    child: &Child,
    stdout_reader: &mut Option<PipeReader>,
    stdout: &mut Vec<u8>,
    stderr_reader: &mut Option<PipeReader>,
    stderr: &mut Vec<u8>,
    command_name: &str,
) -> CliResult<()> {
    drain_pipe_until(
        stdout_reader,
        stdout,
        command_name,
        retain_stdout_data,
        STDERR_DRAIN_AFTER_KILL_TIMEOUT,
    )?;
    drain_pipe_until(
        stderr_reader,
        stderr,
        command_name,
        retain_stderr_data,
        STDERR_DRAIN_AFTER_KILL_TIMEOUT,
    )?;
    if pipe_reader_is_open(stdout_reader) || pipe_reader_is_open(stderr_reader) {
        stop_child_process_tree(child);
    }
    drain_pipe_until(
        stdout_reader,
        stdout,
        command_name,
        retain_stdout_data,
        STDERR_DRAIN_AFTER_EXIT_TIMEOUT,
    )?;
    drain_pipe_until(
        stderr_reader,
        stderr,
        command_name,
        retain_stderr_data,
        STDERR_DRAIN_AFTER_EXIT_TIMEOUT,
    )?;
    join_pipe_reader(stdout_reader, command_name)?;
    join_pipe_reader(stderr_reader, command_name)
}

fn pipe_reader_is_open(reader: &Option<PipeReader>) -> bool {
    reader.as_ref().is_some_and(|reader| !reader.closed)
}

fn join_pipe_reader(reader: &mut Option<PipeReader>, command_name: &str) -> CliResult<()> {
    let Some(reader) = reader else {
        return Ok(());
    };
    if !reader.closed {
        return Err(CliError::Io {
            context: format!("timed out draining pipe output from {command_name}"),
            source: std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "pipe remained open after process-tree cleanup",
            ),
        });
    }
    reader.join(command_name)
}

fn drain_available_stderr(
    reader: &mut Option<PipeReader>,
    stderr: &mut Vec<u8>,
    command_name: &str,
) -> CliResult<()> {
    let Some(reader) = reader else {
        return Ok(());
    };

    drain_available_stderr_reader(reader, stderr, command_name)
}

fn drain_available_stderr_reader(
    reader: &mut PipeReader,
    stderr: &mut Vec<u8>,
    command_name: &str,
) -> CliResult<()> {
    drain_available_pipe_reader(reader, stderr, command_name, retain_stderr_data)
}

fn drain_available_pipe(
    reader: &mut Option<PipeReader>,
    output: &mut Vec<u8>,
    command_name: &str,
    retain: fn(&mut Vec<u8>, &[u8]),
) -> CliResult<()> {
    let Some(reader) = reader else {
        return Ok(());
    };

    drain_available_pipe_reader(reader, output, command_name, retain)
}

fn drain_available_pipe_reader(
    reader: &mut PipeReader,
    output: &mut Vec<u8>,
    command_name: &str,
    retain: fn(&mut Vec<u8>, &[u8]),
) -> CliResult<()> {
    for _ in 0..PIPE_EVENTS_PER_DRAIN {
        if reader.closed {
            break;
        }

        match reader.receiver.try_recv() {
            Ok(event) => handle_pipe_event(event, reader, output, command_name, retain)?,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                reader.closed = true;
            }
        }
    }

    Ok(())
}

fn drain_stderr_until(
    reader: &mut Option<PipeReader>,
    stderr: &mut Vec<u8>,
    command_name: &str,
    timeout: Duration,
) -> CliResult<()> {
    let Some(reader) = reader else {
        return Ok(());
    };

    let deadline = Instant::now() + timeout;
    while !reader.closed {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match reader.receiver.recv_timeout(remaining) {
            Ok(event) => {
                handle_pipe_event(event, reader, stderr, command_name, retain_stderr_data)?
            }
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                reader.closed = true;
            }
        }
    }

    drain_available_stderr_reader(reader, stderr, command_name)
}

fn drain_pipe_until(
    reader: &mut Option<PipeReader>,
    output: &mut Vec<u8>,
    command_name: &str,
    retain: fn(&mut Vec<u8>, &[u8]),
    timeout: Duration,
) -> CliResult<()> {
    let Some(reader) = reader else {
        return Ok(());
    };

    let deadline = Instant::now() + timeout;
    while !reader.closed {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match reader.receiver.recv_timeout(remaining) {
            Ok(event) => handle_pipe_event(event, reader, output, command_name, retain)?,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                reader.closed = true;
            }
        }
    }

    drain_available_pipe_reader(reader, output, command_name, retain)
}

fn handle_pipe_event(
    event: PipeEvent,
    reader: &mut PipeReader,
    output: &mut Vec<u8>,
    command_name: &str,
    retain: fn(&mut Vec<u8>, &[u8]),
) -> CliResult<()> {
    match event {
        PipeEvent::Data(data) => retain(output, &data),
        PipeEvent::Closed => {
            reader.closed = true;
        }
        PipeEvent::Error(source) => {
            reader.closed = true;
            return Err(CliError::Io {
                context: format!("failed to read pipe output from {command_name}"),
                source,
            });
        }
    }

    Ok(())
}

fn retain_stdout_data(stdout: &mut Vec<u8>, data: &[u8]) {
    let retained = stdout.len();
    if retained > MAX_CREDENTIAL_OUTPUT_LEN {
        return;
    }

    let remaining = (MAX_CREDENTIAL_OUTPUT_LEN + 1) - retained;
    stdout.extend_from_slice(&data[..remaining.min(data.len())]);
}

fn retain_stderr_data(stderr: &mut Vec<u8>, data: &[u8]) {
    let retained = stderr.len();
    if retained >= MAX_RETAINED_COMMAND_STDERR_LEN {
        return;
    }

    let remaining = MAX_RETAINED_COMMAND_STDERR_LEN - retained;
    stderr.extend_from_slice(&data[..remaining.min(data.len())]);
}

fn command_status_text(status: ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| status.to_string(), |code| code.to_string())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::Path,
        process::{Command, Stdio},
        thread,
        time::{Duration, Instant},
    };

    use super::{
        DEFAULT_GIT_CREDENTIAL_USERNAME, GitCredentialApproval, GitCredentialLookup,
        GitCredentialRejection, MAX_COMMAND_STDERR_LEN, MAX_CREDENTIAL_FIELD_LEN,
        MAX_CREDENTIAL_OUTPUT_LEN, MAX_RETAINED_COMMAND_STDERR_LEN, git_command,
        git_config_output_has_helper, git_credential_helper_fallback_instructions,
        parse_git_credential_fill_output, retain_stderr_data, retain_stdout_data,
        sanitize_command_stderr, wait_for_git_command_output, wait_for_git_command_timeout,
    };
    use crate::{CliError, LfsSessionToken};

    const PROCESS_TREE_HELPER_TEST: &str = "credentials::tests::credential_process_tree_helper";
    const PROCESS_TREE_DESCENDANT_TEST: &str =
        "credentials::tests::credential_process_tree_descendant";
    const PROCESS_TREE_MARKER_ENV: &str = "LFS_CLOUD_CREDENTIAL_TEST_MARKER";

    fn token() -> LfsSessionToken {
        LfsSessionToken::from_secret("local-lfs-token").expect("test token should be valid")
    }

    #[test]
    fn approval_payload_uses_git_credential_protocol() {
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        assert_eq!(approval.lfs_url().host_str(), Some("lfs.example.com"));
        assert_eq!(approval.username(), DEFAULT_GIT_CREDENTIAL_USERNAME);
        assert_eq!(
            approval.credential_input(),
            "url=https://lfs.example.com/github.com/owner/repo.git/info/lfs\nusername=lfscloud\npassword=local-lfs-token\n\n"
        );
        assert!(!format!("{approval:?}").contains("local-lfs-token"));
    }

    #[test]
    fn rejection_payload_uses_git_credential_protocol_without_debug_leaks() {
        let rejection = GitCredentialRejection::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("rejection should parse");

        assert_eq!(rejection.lfs_url().host_str(), Some("lfs.example.com"));
        assert_eq!(rejection.username(), DEFAULT_GIT_CREDENTIAL_USERNAME);
        assert_eq!(
            rejection.credential_input(),
            "url=https://lfs.example.com/github.com/owner/repo.git/info/lfs\nusername=lfscloud\npassword=local-lfs-token\n\n"
        );
        assert!(!format!("{rejection:?}").contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn rejection_invokes_git_credential_reject_in_repository_context() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let repository = temp.path().join("repo");
        let stdin_path = temp.path().join("stdin.txt");
        let cwd_path = temp.path().join("cwd.txt");
        fs::create_dir(&repository).expect("repository directory should be created");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" != "credential" ] || [ "$2" != "reject" ]; then
  echo "unexpected args: $*" >&2
  exit 64
fi
pwd > '{}'
cat > '{}'
"#,
                cwd_path.display(),
                stdin_path.display()
            ),
        );
        let rejection = GitCredentialRejection::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("rejection should parse");

        rejection
            .reject_with_git_program_in_dir(&fake_git, &repository)
            .expect("fake git rejection should succeed");

        assert_eq!(
            fs::read_to_string(stdin_path).expect("stdin capture should be readable"),
            "url=https://lfs.example.com/github.com/owner/repo.git/info/lfs\nusername=lfscloud\npassword=local-lfs-token\n\n"
        );
        let recorded_cwd = fs::read_to_string(cwd_path).expect("cwd capture should be readable");
        assert_eq!(
            dunce::canonicalize(recorded_cwd.trim()).expect("captured cwd should canonicalize"),
            dunce::canonicalize(repository).expect("repository should canonicalize")
        );
    }

    #[test]
    fn approval_rejects_urls_git_cannot_scope_safely() {
        for invalid in [
            "not a url",
            "file:///tmp/repo.git/info/lfs",
            "https://",
            "https://lfs.example.com/repo.git/info/lfs#fragment",
            "https://user:pass@lfs.example.com/repo.git/info/lfs",
            "https://lfs.example.com/repo.git/info/lfs?token=secret",
        ] {
            let error = GitCredentialApproval::new(invalid, token()).unwrap_err();
            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }

    #[test]
    fn credentials_require_protected_transport_without_explicit_opt_in() {
        for rejected in [
            "http://localhost:8080/repo.git/info/lfs",
            "http://192.168.1.25:8080/repo.git/info/lfs",
        ] {
            let error = GitCredentialApproval::new(rejected, token())
                .expect_err("non-loopback HTTP should be rejected");
            assert!(error.to_string().contains("must use HTTPS"), "{rejected}");
            let error = GitCredentialLookup::new(rejected)
                .expect_err("non-loopback HTTP lookup should be rejected");
            assert!(error.to_string().contains("must use HTTPS"), "{rejected}");
        }

        let approval = GitCredentialApproval::new_with_insecure_http(
            "http://192.168.1.25:8080/repo.git/info/lfs",
            token(),
            true,
        )
        .expect("explicit CLI opt-in should allow the trusted LAN URL");
        assert_eq!(approval.lfs_url().host_str(), Some("192.168.1.25"));
    }

    #[test]
    fn approval_rejects_invalid_usernames() {
        for invalid in ["", "  ", " user", "user ", "user\nname"] {
            let error = GitCredentialApproval::with_username(
                "https://lfs.example.com/repo.git/info/lfs",
                invalid,
                token(),
            )
            .unwrap_err();
            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }

    #[test]
    fn credential_helper_fallback_instructions_avoid_repository_host_tokens() {
        let instructions = git_credential_helper_fallback_instructions(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
        )
        .expect("instructions should be generated");

        assert!(instructions.contains("git config --global credential.helper osxkeychain"));
        assert!(instructions.contains("git config --global credential.helper manager"));
        assert!(instructions.contains("manager-core"));
        assert!(instructions.contains("git config --global credential.helper libsecret"));
        assert!(instructions.contains("vary by distribution"));
        assert!(instructions.contains("global Git helper setting"));
        assert!(instructions.contains("Avoid plaintext storage"));
        assert!(!instructions.contains("git config --global credential.helper store"));
        assert!(instructions.contains("GitHub OAuth token"));
        assert!(instructions.contains("personal access token"));
        assert!(instructions.contains("local LFS Cloud session token"));
    }

    #[test]
    fn credential_helper_config_output_accepts_non_empty_helpers_only() {
        assert!(
            git_config_output_has_helper(
                b"osxkeychain\n",
                "git config --get-urlmatch credential.helper https://lfs.example.com/repo.git/info/lfs",
            )
            .expect("helper output should parse")
        );
        assert!(!git_config_output_has_helper(
            b"\n  \n",
            "git config --get-urlmatch credential.helper https://lfs.example.com/repo.git/info/lfs",
        )
        .expect("blank output should parse"));
        let error = git_config_output_has_helper(
            b"helper\xff",
            "git config --get-urlmatch credential.helper https://lfs.example.com/repo.git/info/lfs",
        )
        .unwrap_err();
        assert!(matches!(error, CliError::ExternalCommandOutput { .. }));
        assert!(
            error
                .to_string()
                .contains("https://lfs.example.com/repo.git/info/lfs")
        );
    }

    #[test]
    #[cfg(unix)]
    fn lookup_invokes_git_credential_fill_and_validates_local_token() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let stdin_path = temp.path().join("stdin.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" != "-c" ] || [ "$2" != "core.askPass=" ] ||
   [ "$3" != "-c" ] || [ "$4" != "credential.interactive=false" ] ||
   [ "$5" != "credential" ] || [ "$6" != "fill" ]; then
  echo "unexpected args: $*" >&2
  exit 64
fi
cat > '{}'
printf '%s\n' \
  'protocol=https' \
  'host=lfs.example.com' \
  'path=github.com/owner/repo.git/info/lfs' \
  'username=lfscloud' \
  'password=local-lfs-token' \
  ''
"#,
                stdin_path.display()
            ),
        );
        let lookup =
            GitCredentialLookup::new("https://lfs.example.com/github.com/owner/repo.git/info/lfs")
                .expect("lookup should parse");

        let credential = lookup
            .lookup_with_git_program(&fake_git)
            .expect("fake git lookup should succeed");

        assert_eq!(
            fs::read_to_string(stdin_path).expect("stdin capture should be readable"),
            "protocol=https\nhost=lfs.example.com\npath=github.com/owner/repo.git/info/lfs\nusername=lfscloud\n\n"
        );
        assert_eq!(credential.lfs_url(), lookup.lfs_url());
        assert_eq!(credential.username(), DEFAULT_GIT_CREDENTIAL_USERNAME);
        assert_eq!(credential.token().as_str(), "local-lfs-token");
        assert!(!format!("{credential:?}").contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn lookup_supports_custom_username_and_explicit_port_scope() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let stdin_path = temp.path().join("stdin.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
cat > '{}'
printf '%s\n' \
  'protocol=https' \
  'host=lfs.example.com:8443' \
  'path=/github.com/owner/repo.git/info/lfs' \
  'username=custom-lfs-user' \
  'password=local-lfs-token' \
  ''
"#,
                stdin_path.display()
            ),
        );
        let lookup = GitCredentialLookup::with_username(
            "https://lfs.example.com:8443/github.com/owner/repo.git/info/lfs",
            "custom-lfs-user",
        )
        .expect("lookup should parse");

        let credential = lookup
            .lookup_with_git_program(&fake_git)
            .expect("fake git lookup should succeed");

        assert_eq!(
            fs::read_to_string(stdin_path).expect("stdin capture should be readable"),
            "protocol=https\nhost=lfs.example.com:8443\npath=github.com/owner/repo.git/info/lfs\nusername=custom-lfs-user\n\n"
        );
        assert_eq!(credential.username(), "custom-lfs-user");
        assert_eq!(credential.token().as_str(), "local-lfs-token");
    }

    #[test]
    #[cfg(unix)]
    fn lookup_rejects_credentials_for_a_different_lfs_path() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' \
  'protocol=https' \
  'host=lfs.example.com' \
  'path=github.com/owner/other.git/info/lfs' \
  'username=lfscloud' \
  'password=local-lfs-token' \
  ''
"#,
        );
        let lookup =
            GitCredentialLookup::new("https://lfs.example.com/github.com/owner/repo.git/info/lfs")
                .expect("lookup should parse");

        let error = lookup
            .lookup_with_git_program(&fake_git)
            .expect_err("path mismatch should be rejected");
        let display = error.to_string();

        assert!(matches!(error, CliError::ExternalCommandOutput { .. }));
        assert!(display.contains("different LFS URL or username"));
        assert!(!display.contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn lookup_rejects_invalid_local_lfs_tokens_without_leaking_them() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
cat >/dev/null
printf '%s\n' \
  'protocol=https' \
  'host=lfs.example.com' \
  'path=github.com/owner/repo.git/info/lfs' \
  'username=lfscloud' \
  'password=local token' \
  ''
"#,
        );
        let lookup =
            GitCredentialLookup::new("https://lfs.example.com/github.com/owner/repo.git/info/lfs")
                .expect("lookup should parse");

        let error = lookup
            .lookup_with_git_program(&fake_git)
            .expect_err("invalid token should be rejected");
        let display = error.to_string();

        assert!(matches!(error, CliError::ExternalCommandOutput { .. }));
        assert!(display.contains("invalid local LFS token"));
        assert!(!display.contains("local token"));
    }

    #[test]
    fn lookup_rejects_oversized_stdout_as_invalid_helper_output() {
        let lfs_url = url::Url::parse("https://lfs.example.com/repo.git/info/lfs")
            .expect("test URL should parse");
        let oversized = vec![b'x'; MAX_CREDENTIAL_OUTPUT_LEN + 1];

        let error =
            parse_git_credential_fill_output(&lfs_url, DEFAULT_GIT_CREDENTIAL_USERNAME, &oversized)
                .expect_err("oversized output should be rejected");

        assert!(matches!(error, CliError::ExternalCommandOutput { .. }));
        assert!(error.to_string().contains("too much output"));
    }

    #[test]
    fn lookup_rejects_non_utf8_stdout_without_leaking_output() {
        let lfs_url = url::Url::parse("https://lfs.example.com/repo.git/info/lfs")
            .expect("test URL should parse");
        let output = b"protocol=https\npassword=local-lfs-token\xff\n";

        let error =
            parse_git_credential_fill_output(&lfs_url, DEFAULT_GIT_CREDENTIAL_USERNAME, output)
                .expect_err("non-UTF-8 output should be rejected");
        let display = error.to_string();

        assert!(matches!(error, CliError::ExternalCommandOutput { .. }));
        assert!(display.contains("non-UTF-8 output"));
        assert!(!display.contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn lookup_failure_suppresses_helper_stderr() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
cat >/dev/null
echo "credential helper rejected stored-lfs-token" >&2
exit 1
"#,
        );
        let lookup =
            GitCredentialLookup::new("https://lfs.example.com/github.com/owner/repo.git/info/lfs")
                .expect("lookup should parse");

        let error = lookup
            .lookup_with_git_program(&fake_git)
            .expect_err("helper failure should be surfaced");
        let display = error.to_string();

        assert!(matches!(error, CliError::ExternalCommand { .. }));
        assert!(display.contains("git credential fill failed"));
        assert!(display.contains("credential helper stderr suppressed"));
        assert!(!display.contains("credential helper rejected"));
        assert!(!display.contains("stored-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn lookup_cache_miss_disables_interactive_prompts() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let environment_path = temp.path().join("environment.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
printf '%s\n' \
  "$GIT_TERMINAL_PROMPT" \
  "${{GIT_ASKPASS-unset}}" \
  "${{SSH_ASKPASS-unset}}" \
  "$GCM_INTERACTIVE" \
  "$GCM_GUI_PROMPT" \
  "$*" > '{}'
cat >/dev/null
exit 1
"#,
                environment_path.display()
            ),
        );
        let lookup =
            GitCredentialLookup::new("https://lfs.example.com/github.com/owner/repo.git/info/lfs")
                .expect("lookup should parse");

        let error = lookup
            .lookup_with_git_program(&fake_git)
            .expect_err("cache miss should return without prompting");

        assert!(matches!(error, CliError::ExternalCommand { .. }));
        assert_eq!(
            fs::read_to_string(environment_path)
                .expect("fake Git environment capture should be readable"),
            "0\nunset\nunset\n0\n0\n-c core.askPass= -c credential.interactive=false credential fill\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn approve_invokes_git_credential_approve_with_stdin_payload() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let stdin_path = temp.path().join("stdin.txt");
        let config_path = temp.path().join("config.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "--get-urlmatch" ]; then
  if [ "$3" != "credential.helper" ] ||
     [ "$4" != "https://lfs.example.com/github.com/owner/repo.git/info/lfs" ]; then
    echo "unexpected helper check args: $*" >&2
    exit 64
  fi
  printf '%s\n' 'store'
  exit 0
fi
if [ "$1" = "config" ]; then
  if [ "$2" != "--local" ] ||
     [ "$3" != "credential.https://lfs.example.com/.useHttpPath" ] ||
     [ "$4" != "true" ]; then
    echo "unexpected config args: $*" >&2
    exit 64
  fi
  printf '%s\n' "$*" > '{}'
  exit 0
fi
if [ "$1" != "credential" ] || [ "$2" != "approve" ]; then
  echo "unexpected args: $*" >&2
  exit 64
fi
cat > '{}'
	"#,
                config_path.display(),
                stdin_path.display()
            ),
        );
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        approval
            .approve_with_git_program(&fake_git)
            .expect("fake git approve should succeed");

        assert_eq!(
            fs::read_to_string(stdin_path).expect("stdin capture should be readable"),
            approval.credential_input()
        );
        assert_eq!(
            fs::read_to_string(config_path).expect("config capture should be readable"),
            "config --local credential.https://lfs.example.com/.useHttpPath true\n"
        );
    }

    #[test]
    fn approve_overrides_repository_local_host_scoping_before_storing_token() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }

        let temp = tempfile::tempdir().expect("tempdir should be created");
        let repo = temp.path().join("repo");
        let credential_store = temp.path().join("credentials");
        fs::create_dir(&repo).expect("test repository directory should be created");

        let init = Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(&repo)
            .status()
            .expect("git init should start");
        assert!(init.success(), "git init should succeed");

        let reset_helper = Command::new("git")
            .args(["config", "--local", "--add", "credential.helper", ""])
            .current_dir(&repo)
            .status()
            .expect("credential helper reset should start");
        assert!(
            reset_helper.success(),
            "credential helper reset should succeed"
        );
        let helper = format!("store --file={}", credential_store.display());
        let configure_helper = Command::new("git")
            .args(["config", "--local", "--add", "credential.helper", &helper])
            .current_dir(&repo)
            .status()
            .expect("credential helper configuration should start");
        assert!(
            configure_helper.success(),
            "credential helper configuration should succeed"
        );
        let configure_host_scope = Command::new("git")
            .args([
                "config",
                "--local",
                "credential.https://lfs.example.com/.useHttpPath",
                "false",
            ])
            .current_dir(&repo)
            .status()
            .expect("host-scope configuration should start");
        assert!(
            configure_host_scope.success(),
            "host-scope configuration should succeed"
        );

        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        approval
            .approve_with_git_program_in_dir("git", &repo)
            .expect("repository-local override should be repaired before approval");

        let effective = Command::new("git")
            .args([
                "config",
                "--bool",
                "--get-urlmatch",
                "credential.useHttpPath",
                approval.lfs_url().as_str(),
            ])
            .current_dir(&repo)
            .output()
            .expect("effective configuration lookup should start");
        assert!(effective.status.success());
        assert_eq!(effective.stdout, b"true\n");

        let stored = fs::read_to_string(credential_store)
            .expect("credential helper should persist the approved token");
        assert!(stored.contains("/github.com/owner/repo.git/info/lfs"));
        assert!(!stored.ends_with("@lfs.example.com\n"));
    }

    #[test]
    #[cfg(unix)]
    fn approve_requires_configured_credential_helper_before_storing_token() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let approve_path = temp.path().join("approve.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "--get-urlmatch" ]; then
  exit 1
fi
if [ "$1" = "credential" ] && [ "$2" = "approve" ]; then
  touch '{}'
fi
exit 0
"#,
                approve_path.display()
            ),
        );
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        let error = approval
            .approve_with_git_program(&fake_git)
            .expect_err("missing helper should be reported before approve");
        let display = error.to_string();

        assert!(matches!(
            error,
            CliError::GitCredentialHelperNotConfigured { .. }
        ));
        assert!(display.contains("credential.helper osxkeychain"));
        assert!(display.contains("credential.helper 'cache --timeout=3600'"));
        assert!(display.contains("writes a global Git helper setting"));
        assert!(display.contains("Do not store a GitHub OAuth token"));
        assert!(!display.contains("local-lfs-token"));
        assert!(!approve_path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn approve_treats_credential_helper_check_miss_with_warning_as_missing_helper() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let approve_path = temp.path().join("approve.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "--get-urlmatch" ]; then
  echo "warning: helper lookup miss" >&2
  exit 1
fi
if [ "$1" = "credential" ] && [ "$2" = "approve" ]; then
  touch '{}'
fi
exit 0
"#,
                approve_path.display()
            ),
        );
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        let error = approval
            .approve_with_git_program(&fake_git)
            .expect_err("helper lookup miss should still produce recovery guidance");

        assert!(matches!(
            error,
            CliError::GitCredentialHelperNotConfigured { .. }
        ));
        assert!(!approve_path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn approve_accepts_url_matched_credential_helper_before_storing_token() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let approve_path = temp.path().join("approve.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "--get-urlmatch" ]; then
  if [ "$3" != "credential.helper" ] ||
     [ "$4" != "https://lfs.example.com/github.com/owner/repo.git/info/lfs" ]; then
    echo "unexpected helper check args: $*" >&2
    exit 64
  fi
  printf '%s\n' 'store'
  exit 0
fi
if [ "$1" = "config" ]; then
  exit 0
fi
if [ "$1" = "credential" ] && [ "$2" = "approve" ]; then
  touch '{}'
fi
"#,
                approve_path.display()
            ),
        );
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        approval
            .approve_with_git_program(&fake_git)
            .expect("URL-matched helper should allow approval");

        assert!(approve_path.exists());
    }

    #[test]
    #[cfg(unix)]
    fn approve_failure_redacts_token_from_command_error() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "--get-urlmatch" ]; then
  printf '%s\n' 'store'
  exit 0
fi
if [ "$1" = "config" ]; then
  exit 0
fi
echo "helper rejected local-lfs-token" >&2
exit 42
"#,
        );
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        let error = approval
            .approve_with_git_program(&fake_git)
            .expect_err("fake git should fail");
        let display = error.to_string();

        assert!(matches!(error, CliError::ExternalCommand { .. }));
        assert!(display.contains("git credential approve failed"));
        assert!(display.contains("<redacted>"));
        assert!(!display.contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn approve_failure_normalizes_multiline_command_stderr() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "--get-urlmatch" ]; then
  printf '%s\n' 'store'
  exit 0
fi
if [ "$1" = "config" ]; then
  exit 0
fi
printf 'first line\nsecond local-lfs-token line\n' >&2
exit 42
"#,
        );
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        let error = approval
            .approve_with_git_program(&fake_git)
            .expect_err("fake git should fail");
        let display = error.to_string();

        assert!(display.contains("first line second <redacted> line"));
        assert!(!display.contains('\n'));
        assert!(!display.contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn approve_failure_times_out_hung_git_helpers() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
echo "waiting with local-lfs-token" >&2
sleep 5
"#,
        );
        let mut command = git_command(&fake_git);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fake git should start");

        let error = wait_for_git_command_timeout(
            &mut child,
            "fake git",
            "local-lfs-token",
            Duration::from_millis(100),
        )
        .expect_err("fake git should time out");
        let display = error.to_string();

        assert!(display.contains("fake git failed with status timed out"));
        assert!(!display.contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn command_wait_drains_large_stderr_while_process_runs() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
i=0
while [ "$i" -lt 20000 ]; do
  printf 'large stderr line %s\n' "$i" >&2
  i=$((i + 1))
done
exit 42
"#,
        );
        let mut command = git_command(&fake_git);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fake git should start");

        let (status, stderr) =
            wait_for_git_command_timeout(&mut child, "fake git", "", Duration::from_secs(5))
                .expect("large stderr should not deadlock or time out");

        assert_eq!(status.code(), Some(42));
        assert_eq!(stderr.len(), MAX_RETAINED_COMMAND_STDERR_LEN);
    }

    #[test]
    #[cfg(unix)]
    fn command_timeout_does_not_block_on_descendant_stderr() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
(sleep 2) &
echo "waiting with local-lfs-token" >&2
wait
"#,
        );
        let mut command = git_command(&fake_git);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fake git should start");

        let started = Instant::now();
        let error = wait_for_git_command_timeout(
            &mut child,
            "fake git",
            "local-lfs-token",
            Duration::from_millis(100),
        )
        .expect_err("fake git should time out");
        let display = error.to_string();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(display.contains("fake git failed with status timed out"));
        assert!(!display.contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn successful_command_stops_pipe_holding_descendants() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let descendant_pid_path = temp.path().join("descendant.pid");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
sleep 10 &
printf '%s\n' "$!" > "$1"
printf 'configured-helper\n'
exit 0
"#,
        );
        let mut command = git_command(&fake_git);
        let mut child = command
            .arg(&descendant_pid_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fake git should start");

        let (status, stdout, stderr) =
            wait_for_git_command_output(&mut child, "fake git", "", Duration::from_secs(5))
                .expect("successful direct child should complete");
        let descendant_pid = fs::read_to_string(&descendant_pid_path)
            .expect("fake git should record its descendant")
            .trim()
            .to_owned();
        let descendant_alive = Command::new("kill")
            .args(["-0", &descendant_pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("kill probe should run")
            .success();
        if descendant_alive {
            let _ = Command::new("kill")
                .args(["-KILL", &descendant_pid])
                .status();
        }

        assert!(status.success());
        assert_eq!(stdout, b"configured-helper\n");
        assert!(stderr.is_empty());
        assert!(!descendant_alive, "pipe-holding descendant was left alive");
    }

    #[test]
    fn command_timeout_stops_descendant_helpers() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let marker_path = temp.path().join("descendant-survived");
        let test_executable = std::env::current_exe().expect("test executable should resolve");
        let mut command = git_command(&test_executable);
        command
            .args(["--ignored", "--exact", PROCESS_TREE_HELPER_TEST])
            .env(PROCESS_TREE_MARKER_ENV, &marker_path);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fake git should start");

        wait_for_git_command_timeout(
            &mut child,
            "fake git",
            "local-lfs-token",
            Duration::from_millis(100),
        )
        .expect_err("fake git should time out");
        thread::sleep(Duration::from_millis(700));

        assert!(!marker_path.exists());
    }

    #[test]
    #[ignore = "invoked as a platform-native process-tree test helper"]
    fn credential_process_tree_helper() {
        let Some(marker_path) = std::env::var_os(PROCESS_TREE_MARKER_ENV) else {
            return;
        };
        let test_executable = std::env::current_exe().expect("test executable should resolve");
        let mut descendant = Command::new(test_executable)
            .args(["--ignored", "--exact", PROCESS_TREE_DESCENDANT_TEST])
            .env(PROCESS_TREE_MARKER_ENV, marker_path)
            .spawn()
            .expect("descendant helper should start");

        descendant
            .wait()
            .expect("descendant helper should remain waitable");
    }

    #[test]
    #[ignore = "invoked as a platform-native process-tree test descendant"]
    fn credential_process_tree_descendant() {
        let Some(marker_path) = std::env::var_os(PROCESS_TREE_MARKER_ENV) else {
            return;
        };

        thread::sleep(Duration::from_millis(500));
        fs::write(marker_path, b"descendant survived timeout cleanup")
            .expect("descendant marker should be writable");
    }

    #[test]
    #[cfg(unix)]
    fn command_timeout_is_checked_while_stderr_stays_noisy() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
while :; do
  printf 'still noisy with local-lfs-token\n' >&2
done
"#,
        );
        let mut command = git_command(&fake_git);
        let mut child = command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .expect("fake git should start");

        let started = Instant::now();
        let error = wait_for_git_command_timeout(
            &mut child,
            "fake git",
            "local-lfs-token",
            Duration::from_millis(100),
        )
        .expect_err("fake git should time out");
        let display = error.to_string();

        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(display.contains("fake git failed with status timed out"));
        assert!(!display.contains("local-lfs-token"));
    }

    #[test]
    #[cfg(unix)]
    fn approve_failure_reports_config_errors_without_running_approve() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let approve_path = temp.path().join("approve.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "config" ] && [ "$2" = "--get-urlmatch" ]; then
  printf '%s\n' 'store'
  exit 0
fi
if [ "$1" = "config" ]; then
  echo "config rejected local-lfs-token" >&2
  exit 43
fi
touch '{}'
exit 0
"#,
                approve_path.display()
            ),
        );
        let approval = GitCredentialApproval::new(
            "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
            token(),
        )
        .expect("approval should parse");

        let error = approval
            .approve_with_git_program(&fake_git)
            .expect_err("fake git config should fail");
        let display = error.to_string();

        assert!(matches!(error, CliError::ExternalCommand { .. }));
        assert!(display.contains(
            "git config --local credential.https://lfs.example.com/.useHttpPath true failed"
        ));
        assert!(display.contains("<redacted>"));
        assert!(!display.contains("local-lfs-token"));
        assert!(!approve_path.exists());
    }

    #[test]
    fn command_stderr_redacts_token_before_truncating() {
        let token = "split-token-secret";
        let prefix = "x".repeat(MAX_COMMAND_STDERR_LEN - 5);
        let stderr = format!("{prefix}{token} suffix");

        let sanitized = sanitize_command_stderr(stderr.as_bytes(), token);

        assert!(sanitized.as_str().len() <= MAX_COMMAND_STDERR_LEN + "...".len());
        assert!(!sanitized.as_str().contains("split"));
        assert!(!sanitized.as_str().contains("split-token-secret"));
    }

    #[test]
    fn retained_command_stderr_is_capped_before_sanitizing() {
        let mut stderr = Vec::new();
        retain_stderr_data(
            &mut stderr,
            &vec![b'x'; MAX_RETAINED_COMMAND_STDERR_LEN * 2],
        );

        assert_eq!(stderr.len(), MAX_RETAINED_COMMAND_STDERR_LEN);
    }

    #[test]
    fn retained_credential_stdout_is_capped_before_parsing() {
        let mut stdout = Vec::new();
        retain_stdout_data(
            &mut stdout,
            &vec![b'x'; MAX_CREDENTIAL_OUTPUT_LEN + MAX_CREDENTIAL_FIELD_LEN],
        );
        retain_stdout_data(&mut stdout, b"extra");

        assert_eq!(stdout.len(), MAX_CREDENTIAL_OUTPUT_LEN + 1);

        let lfs_url = url::Url::parse("https://lfs.example.com/repo.git/info/lfs")
            .expect("test URL should parse");
        let error =
            parse_git_credential_fill_output(&lfs_url, DEFAULT_GIT_CREDENTIAL_USERNAME, &stdout)
                .expect_err("retained oversized output should be rejected");

        assert!(matches!(error, CliError::ExternalCommandOutput { .. }));
        assert!(error.to_string().contains("too much output"));
    }

    #[test]
    fn retained_command_stderr_keeps_token_boundary_before_sanitizing() {
        let token = "split-token-secret";
        let mut stderr = Vec::new();
        let prefix = "x".repeat(MAX_COMMAND_STDERR_LEN - 5);
        let noisy_tail = "y".repeat(MAX_RETAINED_COMMAND_STDERR_LEN * 2);
        let diagnostic = format!("{prefix}{token}{noisy_tail}");

        retain_stderr_data(&mut stderr, diagnostic.as_bytes());
        let sanitized = sanitize_command_stderr(&stderr, token);

        assert!(sanitized.as_str().len() <= MAX_COMMAND_STDERR_LEN + "...".len());
        assert!(!sanitized.as_str().contains("split"));
        assert!(!sanitized.as_str().contains(token));
    }

    #[test]
    fn command_stderr_truncates_at_utf8_boundary() {
        let stderr = format!("{}é", "x".repeat(MAX_COMMAND_STDERR_LEN - 1));

        let sanitized = sanitize_command_stderr(stderr.as_bytes(), "unused-token");

        assert_eq!(
            sanitized.as_str(),
            &format!("{}...", "x".repeat(MAX_COMMAND_STDERR_LEN - 1))
        );
    }

    #[cfg(unix)]
    fn write_fake_git(dir: &Path, script: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        let path = dir.join("git");
        fs::write(&path, script).expect("fake git should be written");
        let mut permissions = fs::metadata(&path)
            .expect("fake git metadata should be readable")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&path, permissions).expect("fake git should be executable");
        path
    }
}
