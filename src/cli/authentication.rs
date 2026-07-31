//! Login, logout, credential, and local-session command handling.

use super::*;

const SESSION_REVOCATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LOGIN_TOKEN_INPUT_BYTES: usize = 1024;
pub(super) fn run_login_to_stdio(command: LoginCommand) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    if stdin.is_terminal() {
        run_login_with_token_reader(command, &mut stdout, read_login_token_from_terminal)
    } else {
        let mut input = stdin.lock();
        run_login(command, &mut input, &mut stdout)
    }
}

pub(super) fn run_logout_to_stdout(command: LogoutCommand) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_logout_from_dir(
        command,
        &current_dir,
        &mut stdout,
        |lfs_url| {
            GitCredentialLookup::new_with_insecure_http(lfs_url, true)?
                .lookup_in_dir(&current_dir)
                .map(|credential| credential.token().clone())
        },
        request_lfs_session_revocation,
        |rejection| rejection.reject_in_dir(&current_dir),
    )
    .map_err(anyhow::Error::from)
}

fn run_login<R, W>(command: LoginCommand, input: &mut R, output: &mut W) -> anyhow::Result<()>
where
    R: BufRead,
    W: Write,
{
    run_login_with_token_reader(command, output, || read_bounded_login_token(input))
}

fn run_login_with_token_reader<W, T>(
    command: LoginCommand,
    output: &mut W,
    read_token: T,
) -> anyhow::Result<()>
where
    W: Write,
    T: FnMut() -> CliResult<String>,
{
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;

    run_login_from_dir_with_token_reader(
        command,
        &current_dir,
        output,
        read_token,
        request_personal_access_token_lfs_session,
        |approval| approval.approve_in_dir(&current_dir),
    )
    .map_err(anyhow::Error::from)
}

#[cfg(test)]
fn run_login_from_dir<R, W, E, A>(
    command: LoginCommand,
    start_dir: impl AsRef<Path>,
    input: &mut R,
    output: &mut W,
    exchange_personal_access_token: E,
    approve_credential: A,
) -> CliResult<()>
where
    R: BufRead,
    W: Write,
    E: FnMut(&str, &str) -> CliResult<LfsSessionToken>,
    A: FnMut(GitCredentialApproval) -> CliResult<()>,
{
    run_login_from_dir_with_token_reader(
        command,
        start_dir,
        output,
        || read_bounded_login_token(input),
        exchange_personal_access_token,
        approve_credential,
    )
}

fn run_login_from_dir_with_token_reader<W, T, E, A>(
    command: LoginCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut read_token: T,
    mut exchange_personal_access_token: E,
    mut approve_credential: A,
) -> CliResult<()>
where
    W: Write,
    T: FnMut() -> CliResult<String>,
    E: FnMut(&str, &str) -> CliResult<LfsSessionToken>,
    A: FnMut(GitCredentialApproval) -> CliResult<()>,
{
    let repository = GitRepository::discover(start_dir.as_ref()).map_err(login_discovery_error)?;
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    write!(output, "GitHub personal access token: ").map_err(output_error)?;
    output.flush().map_err(output_error)?;
    let personal_access_token = read_token()?;
    writeln!(output).map_err(output_error)?;
    let token = exchange_personal_access_token(&route.server_url, &personal_access_token)?;
    let approval = GitCredentialApproval::new_with_insecure_http(
        &route.lfs_url,
        token,
        command.allow_insecure_http,
    )?;
    let approval_username = approval.username().to_owned();
    approve_credential(approval)?;

    writeln!(output, "stored local LFS credential").map_err(output_error)?;
    writeln!(
        output,
        "  lfs.url: {}",
        redacted_url_for_display(&route.lfs_url)
    )
    .map_err(output_error)?;
    writeln!(output, "  username: {approval_username}").map_err(output_error)?;

    Ok(())
}

#[derive(serde::Deserialize)]
struct PersonalAccessTokenLoginResponse {
    lfs_token: String,
}

fn request_personal_access_token_lfs_session(
    server_url: &str,
    personal_access_token: &str,
) -> CliResult<LfsSessionToken> {
    crate::GitHubPersonalAccessToken::from_secret(personal_access_token.to_owned()).map_err(
        |_| CliError::InvalidArguments {
            message: "GitHub personal access token was invalid or blank".to_owned(),
        },
    )?;
    let login_url = github_personal_access_token_login_url_for_server(server_url)?;
    let client = redirect_free_http_client("failed to create GitHub PAT login client")?;
    let response = block_on_reqwest(
        client
            .post(login_url)
            .bearer_auth(personal_access_token)
            .timeout(SESSION_REVOCATION_TIMEOUT)
            .send(),
        "failed to exchange GitHub personal access token",
    )?;
    if !response.status().is_success() {
        return Err(CliError::ExternalCommandOutput {
            command: "GitHub personal access token login".to_owned(),
            message: SanitizedMessage::new(format!(
                "server returned HTTP status {}",
                response.status().as_u16()
            )),
        });
    }
    let response = block_on_reqwest(
        response.json::<PersonalAccessTokenLoginResponse>(),
        "failed to read GitHub PAT login response",
    )?;

    LfsSessionToken::from_secret(response.lfs_token).map_err(|_| CliError::ExternalCommandOutput {
        command: "GitHub personal access token login".to_owned(),
        message: SanitizedMessage::new("server returned an invalid local LFS token"),
    })
}

fn github_personal_access_token_login_url_for_server(server_url: &str) -> CliResult<String> {
    auth_url_for_server(server_url, GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH)
}

pub(super) trait LoginTerminal: BufRead {
    fn is_echo_enabled(&self) -> io::Result<bool>;

    fn set_echo_enabled(&mut self, enabled: bool) -> io::Result<()>;
}

impl LoginTerminal for terminal_prompt::Terminal {
    fn is_echo_enabled(&self) -> io::Result<bool> {
        terminal_prompt::Terminal::is_echo_enabled(self)
    }

    fn set_echo_enabled(&mut self, enabled: bool) -> io::Result<()> {
        if enabled {
            terminal_prompt::Terminal::enable_echo(self)
        } else {
            terminal_prompt::Terminal::disable_echo(self)
        }
    }
}

fn read_login_token_from_terminal() -> CliResult<String> {
    let mut terminal = terminal_prompt::Terminal::open().map_err(|source| CliError::Io {
        context: "failed to open terminal for hidden login input".to_owned(),
        source,
    })?;

    read_hidden_login_token(&mut terminal)
}

fn read_hidden_login_token<T>(terminal: &mut T) -> CliResult<String>
where
    T: LoginTerminal,
{
    let echo_was_enabled = terminal
        .is_echo_enabled()
        .map_err(|source| terminal_echo_error("inspect", source))?;
    if echo_was_enabled {
        terminal
            .set_echo_enabled(false)
            .map_err(|source| terminal_echo_error("disable", source))?;
    }

    let read_result = read_bounded_login_token(terminal);
    let restore_result = if echo_was_enabled {
        terminal
            .set_echo_enabled(true)
            .map_err(|source| terminal_echo_error("restore", source))
    } else {
        Ok(())
    };

    match (read_result, restore_result) {
        (Ok(token), Ok(())) => Ok(token),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn terminal_echo_error(action: &str, source: io::Error) -> CliError {
    CliError::Io {
        context: format!("failed to {action} terminal echo for lfs_token input"),
        source,
    }
}

fn read_bounded_login_token<R>(input: &mut R) -> CliResult<String>
where
    R: BufRead + ?Sized,
{
    let maximum_line_bytes = MAX_LOGIN_TOKEN_INPUT_BYTES + 2;
    let mut bytes = Vec::with_capacity(maximum_line_bytes + 1);
    input
        .take((maximum_line_bytes + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|source| CliError::Io {
            context: "failed to read lfs_token input".to_owned(),
            source,
        })?;

    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_LOGIN_TOKEN_INPUT_BYTES {
        return Err(CliError::InvalidArguments {
            message: format!("lfs_token input must not exceed {MAX_LOGIN_TOKEN_INPUT_BYTES} bytes"),
        });
    }

    String::from_utf8(bytes)
        .map(|token| token.trim_ascii().to_owned())
        .map_err(|_| CliError::InvalidArguments {
            message: "lfs_token input must be valid UTF-8".to_owned(),
        })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionRevocationStatus {
    Revoked,
    AlreadyInactive,
}

fn run_logout_from_dir<W, L, R, E>(
    command: LogoutCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut lookup_credential: L,
    mut revoke_session: R,
    mut erase_credential: E,
) -> CliResult<()>
where
    W: Write,
    L: FnMut(&str) -> CliResult<LfsSessionToken>,
    R: FnMut(&str, &LfsSessionToken) -> CliResult<SessionRevocationStatus>,
    E: FnMut(GitCredentialRejection) -> CliResult<()>,
{
    let repository = GitRepository::discover(start_dir.as_ref()).map_err(login_discovery_error)?;
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    let token = lookup_credential(&route.lfs_url)?;
    let revoke_url = session_revocation_url_for_server(&route.server_url)?;
    let revocation = revoke_session(&revoke_url, &token)?;
    let rejection = GitCredentialRejection::new_with_insecure_http(
        &route.lfs_url,
        token,
        command.allow_insecure_http,
    )?;
    erase_credential(rejection)?;

    match revocation {
        SessionRevocationStatus::Revoked => {
            writeln!(output, "revoked local LFS session").map_err(output_error)?;
        }
        SessionRevocationStatus::AlreadyInactive => {
            writeln!(output, "local LFS session was already inactive").map_err(output_error)?;
        }
    }
    writeln!(output, "erased local LFS credential").map_err(output_error)?;
    writeln!(
        output,
        "  lfs.url: {}",
        redacted_url_for_display(&route.lfs_url)
    )
    .map_err(output_error)?;

    Ok(())
}

fn session_revocation_url_for_server(server_url: &str) -> CliResult<String> {
    auth_url_for_server(server_url, LFS_SESSION_REVOKE_PATH)
}

fn request_lfs_session_revocation(
    revoke_url: &str,
    token: &LfsSessionToken,
) -> CliResult<SessionRevocationStatus> {
    let client = redirect_free_http_client("failed to create LFS session revocation client")?;
    let response = block_on_reqwest(
        client
            .delete(revoke_url)
            .bearer_auth(token.as_str())
            .timeout(SESSION_REVOCATION_TIMEOUT)
            .send(),
        "failed to request LFS session revocation",
    )?;

    match response.status() {
        HttpStatusCode::NO_CONTENT => Ok(SessionRevocationStatus::Revoked),
        HttpStatusCode::UNAUTHORIZED => Ok(SessionRevocationStatus::AlreadyInactive),
        status => Err(CliError::ExternalCommandOutput {
            command: "LFS session revocation request".to_owned(),
            message: SanitizedMessage::new(format!(
                "server returned unexpected HTTP status {}",
                status.as_u16()
            )),
        }),
    }
}

fn login_discovery_error(error: CliError) -> CliError {
    match error {
        CliError::ExternalCommand { command, .. } if command == "git remote get-url origin" => {
            CliError::InvalidArguments {
                message: "lfscloud login requires an origin remote; add the repository remote before logging in".to_owned(),
            }
        }
        error => error,
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[allow(unused_imports)]
    use std::ffi::OsString;
    #[cfg(unix)]
    #[allow(unused_imports)]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(unix)]
    #[allow(unused_imports)]
    use std::time::Instant;
    #[allow(unused_imports)]
    use std::{
        collections::BTreeMap,
        fs, io,
        path::{Path, PathBuf},
        process::Command as ProcessCommand,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[allow(unused_imports)]
    use axum::{
        Json, Router,
        http::{HeaderMap, StatusCode},
        routing::post,
    };
    #[allow(unused_imports)]
    use clap::{CommandFactory, Parser};
    #[allow(unused_imports)]
    use sha2::{Digest, Sha256};
    #[allow(unused_imports)]
    use tempfile::TempDir;

    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::cli::test_support::*;
    #[allow(unused_imports)]
    use crate::{
        CliError, DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, GitCredentialApproval,
        GitCredentialRejection, GitLfsConfigChange, GitLfsConfigTarget, GitRepository,
        GoogleDriveStorageConfig, LfsObject, LfsObjectSize, LfsOid, LfsPointer, LfsSessionToken,
        LocalCacheError, LocalCacheLayout, LocalCacheWorktreeRegistration, ProviderFuture,
        RepositoryMapping, SanitizedMessage, ServeOptions, ServerConfig, StorageDeleteOutcome,
        StorageError, StorageProvider, StorageProviderConfig, StorageResult, StoredObject,
    };

    #[test]
    fn personal_access_token_login_url_preserves_server_base_path() {
        assert_eq!(
            github_personal_access_token_login_url_for_server(
                "https://lfs.example.com/custom/base"
            )
            .expect("login URL should resolve"),
            "https://lfs.example.com/custom/base/auth/github/pat"
        );
    }

    #[test]
    fn personal_access_token_login_url_preserves_root_server_base() {
        assert_eq!(
            github_personal_access_token_login_url_for_server("https://lfs.example.com")
                .expect("login URL should resolve"),
            "https://lfs.example.com/auth/github/pat"
        );
    }

    #[test]
    fn session_revocation_url_preserves_server_base_paths() {
        for (server_url, expected) in [
            (
                "https://lfs.example.com",
                "https://lfs.example.com/auth/session",
            ),
            (
                "https://lfs.example.com/custom/base",
                "https://lfs.example.com/custom/base/auth/session",
            ),
        ] {
            assert_eq!(
                session_revocation_url_for_server(server_url)
                    .expect("session revocation URL should resolve"),
                expected
            );
        }
    }

    #[test]
    fn login_url_rejects_unsafe_server_url_components() {
        for server_url in [
            " https://lfs.example.com/custom/base",
            "https://lfs.example.com/custom/base/",
            "https://user:secret@lfs.example.com/custom/base",
            "https://lfs.example.com/custom/base?token=secret",
            "https://lfs.example.com/custom/base#fragment",
            "https://lfs.example.com/custom base",
            "https://lfs.example.com/custom\nbase",
            "https://lfs.example.com\\custom\\base",
            "https://lfs.example.com/custom/../base",
            "https://lfs.example.com/custom/./base",
            "https://lfs.example.com/custom/%2e%2e/base",
        ] {
            let error = github_personal_access_token_login_url_for_server(server_url)
                .expect_err("unsafe server URL should be rejected");
            assert!(
                matches!(error, CliError::InvalidArguments { .. }),
                "unexpected error for {server_url}: {error}"
            );
        }
    }

    #[test]
    fn login_exchanges_pat_and_stores_only_local_lfs_token_for_current_repo() {
        require_git();

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let exchange = Arc::new(Mutex::new(None));
        let approved = Arc::new(Mutex::new(None));
        let exchange_for_runner = Arc::clone(&exchange);
        let approved_for_runner = Arc::clone(&approved);
        let mut input = io::Cursor::new(b" \tgithub-pat \r\n".to_vec());
        let mut output = Vec::new();

        run_login_from_dir(
            LoginCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
            },
            repo.path(),
            &mut input,
            &mut output,
            move |server_url, personal_access_token| {
                *exchange_for_runner
                    .lock()
                    .expect("capture mutex should lock") =
                    Some((server_url.to_owned(), personal_access_token.to_owned()));
                LfsSessionToken::from_secret("local-lfs-token").map_err(|error| {
                    CliError::InvalidArguments {
                        message: error.to_string(),
                    }
                })
            },
            move |approval: GitCredentialApproval| {
                let credential = (
                    approval.lfs_url().to_string(),
                    approval.username().to_owned(),
                    approval.token().as_str().to_owned(),
                );
                *approved_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(credential);
                Ok(())
            },
        )
        .expect("login should complete");

        assert_eq!(
            *exchange.lock().expect("capture mutex should lock"),
            Some(("http://127.0.0.1:8080".to_owned(), "github-pat".to_owned(),))
        );
        assert_eq!(
            *approved.lock().expect("capture mutex should lock"),
            Some((
                "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs".to_owned(),
                "lfscloud".to_owned(),
                "local-lfs-token".to_owned(),
            ))
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("GitHub personal access token:"));
        assert!(rendered.contains("stored local LFS credential"));
        assert!(rendered.contains("username: lfscloud"));
        assert!(!rendered.contains("local-lfs-token"));
        assert!(!rendered.contains("github-pat"));
    }

    #[test]
    fn piped_login_token_input_is_bounded_and_trimmed() {
        let maximum_token = "x".repeat(MAX_LOGIN_TOKEN_INPUT_BYTES);
        let mut input = io::Cursor::new(format!("{maximum_token}\r\n"));

        assert_eq!(
            read_bounded_login_token(&mut input).expect("maximum token should be accepted"),
            maximum_token
        );

        let mut oversized = io::Cursor::new("x".repeat(MAX_LOGIN_TOKEN_INPUT_BYTES + 1));
        let error = read_bounded_login_token(&mut oversized)
            .expect_err("oversized piped input should be rejected");

        assert!(matches!(
            error,
            CliError::InvalidArguments { message }
                if message.contains("must not exceed")
        ));
        assert!(oversized.position() <= (MAX_LOGIN_TOKEN_INPUT_BYTES + 3) as u64);

        let mut padded = io::Cursor::new(b" local-lfs-token \n".to_vec());
        assert_eq!(
            read_bounded_login_token(&mut padded)
                .expect("line reader should trim pasted ASCII whitespace"),
            "local-lfs-token"
        );
    }

    struct TrackingLoginTerminal {
        input: io::Cursor<Vec<u8>>,
        echo_enabled: bool,
        read_while_hidden: bool,
    }

    impl io::Read for TrackingLoginTerminal {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            self.read_while_hidden |= !self.echo_enabled;
            self.input.read(buffer)
        }
    }

    impl io::BufRead for TrackingLoginTerminal {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            self.read_while_hidden |= !self.echo_enabled;
            self.input.fill_buf()
        }

        fn consume(&mut self, amount: usize) {
            self.input.consume(amount);
        }
    }

    impl LoginTerminal for TrackingLoginTerminal {
        fn is_echo_enabled(&self) -> io::Result<bool> {
            Ok(self.echo_enabled)
        }

        fn set_echo_enabled(&mut self, enabled: bool) -> io::Result<()> {
            self.echo_enabled = enabled;
            Ok(())
        }
    }

    #[test]
    fn terminal_login_token_input_is_hidden_and_restores_echo() {
        let mut terminal = TrackingLoginTerminal {
            input: io::Cursor::new(b"terminal-lfs-token\n".to_vec()),
            echo_enabled: true,
            read_while_hidden: false,
        };

        assert_eq!(
            read_hidden_login_token(&mut terminal).expect("hidden terminal token should be read"),
            "terminal-lfs-token"
        );
        assert!(terminal.read_while_hidden);
        assert!(terminal.echo_enabled);
    }

    #[test]
    fn logout_revokes_remote_session_before_erasing_local_credential() {
        require_git();

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let events = Arc::new(Mutex::new(Vec::new()));
        let lookup_events = Arc::clone(&events);
        let revoke_events = Arc::clone(&events);
        let erase_events = Arc::clone(&events);
        let mut output = Vec::new();

        run_logout_from_dir(
            LogoutCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
            },
            repo.path(),
            &mut output,
            move |lfs_url| {
                lookup_events
                    .lock()
                    .expect("events mutex should lock")
                    .push(format!("lookup:{lfs_url}"));
                crate::LfsSessionToken::from_secret("local-lfs-token").map_err(|error| {
                    CliError::InvalidArguments {
                        message: error.to_string(),
                    }
                })
            },
            move |logout_url, token| {
                revoke_events
                    .lock()
                    .expect("events mutex should lock")
                    .push(format!("revoke:{logout_url}:{}", token.as_str()));
                Ok(SessionRevocationStatus::Revoked)
            },
            move |rejection: GitCredentialRejection| {
                erase_events
                    .lock()
                    .expect("events mutex should lock")
                    .push(format!(
                        "erase:{}:{}",
                        rejection.lfs_url(),
                        rejection.token().as_str()
                    ));
                Ok(())
            },
        )
        .expect("logout should complete");

        assert_eq!(
            *events.lock().expect("events mutex should lock"),
            vec![
                "lookup:http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs",
                "revoke:http://127.0.0.1:8080/auth/session:local-lfs-token",
                "erase:http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs:local-lfs-token",
            ]
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("revoked local LFS session"));
        assert!(rendered.contains("erased local LFS credential"));
        assert!(!rendered.contains("local-lfs-token"));
    }

    #[test]
    fn logout_erases_stale_local_credential_when_session_is_already_inactive() {
        require_git();

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let erased = Arc::new(Mutex::new(false));
        let erased_for_runner = Arc::clone(&erased);
        let mut output = Vec::new();

        run_logout_from_dir(
            LogoutCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
            },
            repo.path(),
            &mut output,
            |_| {
                LfsSessionToken::from_secret("stale-lfs-token").map_err(|error| {
                    CliError::InvalidArguments {
                        message: error.to_string(),
                    }
                })
            },
            |_, _| Ok(SessionRevocationStatus::AlreadyInactive),
            move |_| {
                *erased_for_runner.lock().expect("erasure mutex should lock") = true;
                Ok(())
            },
        )
        .expect("already inactive logout should complete local cleanup");

        assert!(*erased.lock().expect("erasure mutex should lock"));
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("already inactive"));
        assert!(rendered.contains("erased local LFS credential"));
        assert!(!rendered.contains("stale-lfs-token"));
    }

    #[test]
    fn login_reports_missing_origin_remote_with_targeted_message() {
        require_git();

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        let mut input = io::Cursor::new(b"local-lfs-token\n".to_vec());
        let mut output = Vec::new();

        let error = run_login_from_dir(
            LoginCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
            },
            repo.path(),
            &mut input,
            &mut output,
            |_, _| panic!("PAT exchange should not run without a remote"),
            |_| panic!("credential approval should not run without a remote"),
        )
        .expect_err("missing origin remote should fail before login");

        assert!(matches!(
            error,
            CliError::InvalidArguments { message }
                if message.contains("requires an origin remote")
        ));
    }
}
