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

use crate::{CliError, CliResult, LfsSessionToken, SanitizedMessage};

/// Username stored alongside local LFS Cloud bearer tokens in Git credentials.
pub const DEFAULT_GIT_CREDENTIAL_USERNAME: &str = "lfs-cloud";

const MAX_CREDENTIAL_FIELD_LEN: usize = 2048;
const MAX_COMMAND_STDERR_LEN: usize = 4096;
const MAX_RETAINED_COMMAND_STDERR_LEN: usize = MAX_COMMAND_STDERR_LEN + MAX_CREDENTIAL_FIELD_LEN;
const GIT_COMMAND_TIMEOUT: Duration = Duration::from_secs(30);
const STDERR_DRAIN_AFTER_EXIT_TIMEOUT: Duration = Duration::from_secs(1);
const STDERR_DRAIN_AFTER_KILL_TIMEOUT: Duration = Duration::from_millis(100);
const STDERR_EVENTS_PER_WAIT_POLL: usize = 64;

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
    /// argument, so process listings do not expose the secret. Before storing
    /// the credential, this persists path-aware lookup for the LFS Cloud host so
    /// future Git LFS credential fills also keep repository paths separate.
    /// This writes `credential.<lfs-host>.useHttpPath=true` to the user's global
    /// Git config because Git LFS resolves credentials in later processes.
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
        self.persist_path_aware_lookup(git_program.as_ref())?;
        self.approve_with_configured_git(git_program.as_ref())
    }

    fn persist_path_aware_lookup(&self, git_program: &Path) -> CliResult<()> {
        let credential_scope = credential_host_scope(&self.lfs_url);
        let config_key = format!("credential.{credential_scope}.useHttpPath");
        let command_name = format!("git config --global {config_key} true");
        let mut command = git_command(git_program);
        let mut child = command
            .args(["config", "--global", &config_key, "true"])
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

    fn approve_with_configured_git(&self, git_program: &Path) -> CliResult<()> {
        let command_name = "git credential approve";
        let mut command = git_command(git_program);
        let mut child = command
            .args(["credential", "approve"])
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
    let mut stderr_reader = child.stderr.take().map(StderrReader::new);

    loop {
        drain_available_stderr(&mut stderr_reader, &mut stderr, command_name)?;

        if let Some(status) = child.try_wait().map_err(|source| CliError::Io {
            context: format!("failed to wait for {command_name}"),
            source,
        })? {
            drain_stderr_until(
                &mut stderr_reader,
                &mut stderr,
                command_name,
                STDERR_DRAIN_AFTER_EXIT_TIMEOUT,
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
            return Err(CliError::ExternalCommand {
                command: command_name.to_owned(),
                status: format!("timed out after {} seconds", timeout.as_secs()),
                stderr: sanitize_command_stderr(&stderr, token),
            });
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn git_command(git_program: &Path) -> Command {
    Command::new(git_program)
}

fn stop_timed_out_child(child: &mut Child, command_name: &str) -> CliResult<()> {
    stop_timed_out_child_process_tree(child);

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
fn stop_timed_out_child_process_tree(child: &Child) {
    let descendants = collect_descendant_pids(child.id());
    for pid in descendants.iter().rev() {
        signal_process("TERM", *pid);
    }
    thread::sleep(Duration::from_millis(50));
    for pid in descendants.iter().rev() {
        signal_process("KILL", *pid);
    }
}

#[cfg(unix)]
fn collect_descendant_pids(root_pid: u32) -> Vec<u32> {
    let mut descendants = Vec::new();
    let mut pending = child_pids(root_pid);

    while let Some(pid) = pending.pop() {
        descendants.push(pid);
        pending.extend(child_pids(pid));
    }

    descendants
}

#[cfg(target_os = "linux")]
fn child_pids(parent_pid: u32) -> Vec<u32> {
    let children_path = format!("/proc/{parent_pid}/task/{parent_pid}/children");
    let Ok(children) = std::fs::read_to_string(children_path) else {
        return Vec::new();
    };

    children
        .split_whitespace()
        .filter_map(|pid| pid.parse().ok())
        .collect()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn child_pids(parent_pid: u32) -> Vec<u32> {
    let Ok(output) = Command::new("pgrep")
        .args(["-P", &parent_pid.to_string()])
        .stdin(Stdio::null())
        .output()
    else {
        return Vec::new();
    };

    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

#[cfg(unix)]
fn signal_process(signal: &str, pid: u32) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn stop_timed_out_child_process_tree(child: &Child) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn stop_timed_out_child_process_tree(_child: &Child) {}

struct StderrReader {
    receiver: Receiver<StderrEvent>,
    _reader_thread: thread::JoinHandle<()>,
    closed: bool,
}

impl StderrReader {
    fn new(mut pipe: std::process::ChildStderr) -> Self {
        let (sender, receiver) = mpsc::sync_channel(STDERR_EVENTS_PER_WAIT_POLL);
        let reader_thread = thread::spawn(move || {
            let mut buffer = [0; 8192];
            loop {
                match pipe.read(&mut buffer) {
                    Ok(0) => {
                        let _ = sender.send(StderrEvent::Closed);
                        break;
                    }
                    Ok(count) => {
                        if sender
                            .send(StderrEvent::Data(buffer[..count].to_vec()))
                            .is_err()
                        {
                            break;
                        }
                    }
                    Err(source) => {
                        let _ = sender.send(StderrEvent::Error(source));
                        break;
                    }
                }
            }
        });

        Self {
            receiver,
            _reader_thread: reader_thread,
            closed: false,
        }
    }
}

enum StderrEvent {
    Data(Vec<u8>),
    Error(std::io::Error),
    Closed,
}

fn drain_available_stderr(
    reader: &mut Option<StderrReader>,
    stderr: &mut Vec<u8>,
    command_name: &str,
) -> CliResult<()> {
    let Some(reader) = reader else {
        return Ok(());
    };

    drain_available_stderr_reader(reader, stderr, command_name)
}

fn drain_available_stderr_reader(
    reader: &mut StderrReader,
    stderr: &mut Vec<u8>,
    command_name: &str,
) -> CliResult<()> {
    for _ in 0..STDERR_EVENTS_PER_WAIT_POLL {
        if reader.closed {
            break;
        }

        match reader.receiver.try_recv() {
            Ok(event) => handle_stderr_event(event, reader, stderr, command_name)?,
            Err(mpsc::TryRecvError::Empty) => break,
            Err(mpsc::TryRecvError::Disconnected) => {
                reader.closed = true;
            }
        }
    }

    Ok(())
}

fn drain_stderr_until(
    reader: &mut Option<StderrReader>,
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
            Ok(event) => handle_stderr_event(event, reader, stderr, command_name)?,
            Err(mpsc::RecvTimeoutError::Timeout) => break,
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                reader.closed = true;
            }
        }
    }

    drain_available_stderr_reader(reader, stderr, command_name)
}

fn handle_stderr_event(
    event: StderrEvent,
    reader: &mut StderrReader,
    stderr: &mut Vec<u8>,
    command_name: &str,
) -> CliResult<()> {
    match event {
        StderrEvent::Data(data) => retain_stderr_data(stderr, &data),
        StderrEvent::Closed => {
            reader.closed = true;
        }
        StderrEvent::Error(source) => {
            reader.closed = true;
            return Err(CliError::Io {
                context: format!("failed to read stderr from {command_name}"),
                source,
            });
        }
    }

    Ok(())
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
        DEFAULT_GIT_CREDENTIAL_USERNAME, GitCredentialApproval, MAX_COMMAND_STDERR_LEN,
        MAX_RETAINED_COMMAND_STDERR_LEN, git_command, retain_stderr_data, sanitize_command_stderr,
        wait_for_git_command_timeout,
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
            "https://user:pass@lfs.example.com/repo.git/info/lfs",
            "https://lfs.example.com/repo.git/info/lfs?token=secret",
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
        let config_path = temp.path().join("config.txt");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
if [ "$1" = "config" ]; then
  if [ "$2" != "--global" ] ||
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
            "config --global credential.https://lfs.example.com/.useHttpPath true\n"
        );
    }

    #[test]
    #[cfg(unix)]
    fn approve_failure_redacts_token_from_command_error() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let fake_git = write_fake_git(
            temp.path(),
            r#"#!/bin/sh
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
        let mut child = Command::new(&fake_git)
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
        let mut child = Command::new(&fake_git)
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
    fn command_timeout_stops_descendant_helpers() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let marker_path = temp.path().join("descendant-survived");
        let fake_git = write_fake_git(
            temp.path(),
            &format!(
                r#"#!/bin/sh
(
  sleep 0.5
  touch '{}'
) &
echo "waiting with local-lfs-token" >&2
wait
"#,
                marker_path.display()
            ),
        );
        let mut command = git_command(&fake_git);
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
        let mut child = Command::new(&fake_git)
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
            "git config --global credential.https://lfs.example.com/.useHttpPath true failed"
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
