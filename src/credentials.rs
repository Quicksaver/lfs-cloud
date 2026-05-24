//! Git credential-helper integration for local LFS Cloud tokens.
//!
//! The GitHub OAuth token stays inside provider-facing code. This module stores
//! only the short-lived local LFS Cloud token that Git LFS should use when it
//! contacts the configured LFS URL.

use std::{
    fmt,
    io::Write,
    path::Path,
    process::{Command, Stdio},
};

use url::Url;

use crate::{CliError, CliResult, LfsSessionToken, SanitizedMessage};

/// Username stored alongside local LFS Cloud bearer tokens in Git credentials.
pub const DEFAULT_GIT_CREDENTIAL_USERNAME: &str = "lfs-cloud";

const MAX_CREDENTIAL_FIELD_LEN: usize = 2048;
const MAX_COMMAND_STDERR_LEN: usize = 4096;

/// Credential-helper payload for approving one configured LFS URL.
#[derive(Clone, Eq, PartialEq)]
pub struct GitCredentialApproval {
    lfs_url: Url,
    username: String,
    token: LfsSessionToken,
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
    /// use lfs_cloud::{GitCredentialApproval, LfsSessionToken};
    ///
    /// let approval = GitCredentialApproval::new(
    ///     "https://lfs.example.com/github.com/owner/repo.git/info/lfs",
    ///     LfsSessionToken::from_secret("local-lfs-token")?,
    /// )?;
    ///
    /// assert_eq!(approval.lfs_url().host_str(), Some("lfs.example.com"));
    /// # Ok::<(), lfs_cloud::LfsCloudError>(())
    /// ```
    pub fn new(lfs_url: impl AsRef<str>, token: LfsSessionToken) -> CliResult<Self> {
        Self::with_username(lfs_url, DEFAULT_GIT_CREDENTIAL_USERNAME, token)
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
            lfs_url: validate_lfs_credential_url(lfs_url.as_ref())?,
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

    /// Approves the credential through `git credential approve`.
    ///
    /// The token is written on standard input, not passed as a process
    /// argument, so process listings do not expose the secret. The command
    /// forces path-aware credential storage so repositories sharing one
    /// `lfs-cloud` host keep separate LFS URL credentials.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when `git` cannot be started, stdin cannot be
    /// written, or the credential helper exits unsuccessfully.
    pub fn approve(&self) -> CliResult<()> {
        self.approve_with_git_program(Path::new("git"))
    }

    /// Approves the credential with a caller-selected Git executable.
    ///
    /// This is primarily for tests that inject a fake `git` executable while
    /// preserving the exact stdin protocol used by the real helper.
    ///
    /// # Errors
    ///
    /// Returns [`CliError`] when the process cannot be started, stdin cannot be
    /// written, or the helper exits unsuccessfully.
    pub fn approve_with_git_program(&self, git_program: impl AsRef<Path>) -> CliResult<()> {
        let command_name = "git -c credential.useHttpPath=true credential approve";
        let mut child = Command::new(git_program.as_ref())
            .args(["-c", "credential.useHttpPath=true", "credential", "approve"])
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

        let output = child.wait_with_output().map_err(|source| CliError::Io {
            context: format!("failed to wait for {command_name}"),
            source,
        })?;

        if output.status.success() {
            return Ok(());
        }

        Err(CliError::ExternalCommand {
            command: command_name.to_owned(),
            status: output
                .status
                .code()
                .map_or_else(|| output.status.to_string(), |code| code.to_string()),
            stderr: sanitize_command_stderr(&output.stderr, self.token.as_str()),
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

fn validate_lfs_credential_url(value: &str) -> CliResult<Url> {
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

    if value
        .chars()
        .any(|character| character == '\n' || character == '\r' || character.is_control())
    {
        return Err(CliError::InvalidArguments {
            message: format!("{label} must not contain control characters"),
        });
    }

    Ok(value)
}

fn sanitize_command_stderr(stderr: &[u8], token: &str) -> SanitizedMessage {
    let mut message = String::from_utf8_lossy(stderr).into_owned();
    if !token.is_empty() {
        message = message.replace(token, "<redacted>");
    }
    if message.len() > MAX_COMMAND_STDERR_LEN {
        message.truncate(MAX_COMMAND_STDERR_LEN);
        message.push_str("...");
    }
    let message = message.trim();

    if message.is_empty() {
        SanitizedMessage::new("<no stderr>")
    } else {
        SanitizedMessage::new(message.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use super::{
        DEFAULT_GIT_CREDENTIAL_USERNAME, GitCredentialApproval, MAX_COMMAND_STDERR_LEN,
        sanitize_command_stderr,
    };
    use crate::{CliError, LfsSessionToken};

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
            "url=https://lfs.example.com/github.com/owner/repo.git/info/lfs\nusername=lfs-cloud\npassword=local-lfs-token\n\n"
        );
        assert!(!format!("{approval:?}").contains("local-lfs-token"));
    }

    #[test]
    fn approval_rejects_urls_git_cannot_scope_safely() {
        for invalid in [
            "not a url",
            "file:///tmp/repo.git/info/lfs",
            "https://",
            "https://lfs.example.com/repo.git/info/lfs#fragment",
        ] {
            let error = GitCredentialApproval::new(invalid, token()).unwrap_err();
            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
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
    #[cfg(unix)]
    fn approve_invokes_git_credential_approve_with_stdin_payload() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let stdin_path = temp.path().join("stdin.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" != "credential" ] || [ "$2" != "approve" ]; then
  shift 2
fi
if [ "$1" != "credential" ] || [ "$2" != "approve" ]; then
  echo "unexpected args: $*" >&2
  exit 64
fi
cat > '{}'
"#,
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
    }

    #[test]
    #[cfg(unix)]
    fn approve_failure_redacts_token_from_command_error() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
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
        assert!(display.contains("git -c credential.useHttpPath=true credential approve failed"));
        assert!(display.contains("<redacted>"));
        assert!(!display.contains("local-lfs-token"));
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
