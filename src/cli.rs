//! Command-line parsing and dispatch for LFS Cloud.
//!
//! This module keeps the binary target small while making CLI behavior
//! testable without binding sockets. The process entry point owns global
//! tracing initialization, while parser and dispatch helpers stay side-effect
//! free for focused tests.

use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs,
    future::Future,
    io::{self, BufRead, IsTerminal, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::{Path, PathBuf},
    process::{Command as ProcessCommand, Stdio},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use reqwest::{Client, StatusCode as HttpStatusCode, redirect::Policy};
use url::Url;

use crate::child_process::{
    ChildProcessError, ChildProcessOptions, ChildProcessOutput, PipeCapture,
    configure_process_tree, wait_for_child,
};
use crate::git_output::{GitPathOutputError, parse_lfs_filter_attribute_paths};
use crate::google_drive::{GoogleDriveAccessTokenCache, GoogleDriveAccessTokenSource};
use crate::{CliError, CliResult, SanitizedMessage, git::redacted_url_for_display};
use crate::{
    GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH, GitCredentialApproval, GitCredentialLookup,
    GitCredentialRejection, GitLfsConfigChange, GitLfsConfigTarget, GitLfsHistoryPointers,
    GitLfsMigrationDiscovery, GitLfsSourceEndpointSource, GitRemote, GitRepository,
    GoogleDriveGcloudTokenProvider, GoogleDriveObjectStore, GoogleDriveRootValidator,
    GoogleDriveStorageConfig, LFS_POINTER_SIZE_CUTOFF, LFS_SESSION_REVOKE_PATH, LfsInitRoute,
    LfsObject, LfsPointer, LfsSessionToken, LocalCacheDehydration, LocalCacheDehydrationStatus,
    LocalCacheGarbageCollection, LocalCacheGarbageCollectionObject, LocalCacheIngest,
    LocalCacheIngestStatus, LocalCacheLayout, LocalCacheMaterialization,
    LocalCacheMaterializationStatus, LocalCacheWorktreeRegistration,
    LocalMigrationObjectAvailability, MetadataDatabase, MigrationError, MigrationFetchMode,
    MigrationSourceFetch, MigrationStorageUpload, ProviderFuture, RepositoryMapping, ServeOptions,
    ServerConfig, StorageDeleteOutcome, StorageError, StorageProvider, StorageProviderConfig,
    StorageResult, StoredObject, TracingConfig, check_local_migration_objects,
    discover_git_lfs_migration_from_remote, enumerate_current_checkout_lfs_pointers,
    enumerate_fetched_ref_lfs_pointers_for_remote, enumerate_selected_ref_lfs_pointers,
    fetch_migration_git_refs, fetch_missing_migration_objects_from_remote, init_tracing,
    upload_migration_objects_to_storage,
};

const STATUS_SERVER_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_REVOCATION_TIMEOUT: Duration = Duration::from_secs(30);
const MIGRATION_TARGET_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_FETCH_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_PULL_FETCH_OUTPUT_BYTES: usize = 256 * 1024;
const MIGRATION_OBJECT_REPORT_LIMIT: usize = 100;
const SOURCE_ENDPOINT_UNSET_LABEL: &str = "<unset>";
const SOURCE_PROVIDER_UNKNOWN_LABEL: &str = "unknown";
const MAX_LOGIN_TOKEN_INPUT_BYTES: usize = 1024;

#[derive(Debug, Parser)]
#[command(name = "lfscloud", version, about, propagate_version = true)]
#[command(arg_required_else_help = true)]
struct Cli {
    /// Server config path to load.
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Tracing filter or log level to use instead of RUST_LOG.
    #[arg(long, global = true, value_name = "FILTER")]
    log_level: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run the local Git LFS-compatible HTTP server.
    Serve(ServeCommand),
    /// Authenticate with GitHub and store the local LFS token for this repo.
    Login(LoginCommand),
    /// Revoke the local LFS session and erase its Git credential.
    Logout(LogoutCommand),
    /// Resolve the Git LFS URL for the current repository.
    Init(InitCommand),
    /// Check repository, server, auth, storage, and local cache readiness.
    Status(StatusCommand),
    /// Fetch current Git LFS objects and hydrate pointer files from cache.
    Pull(PullCommand),
    /// Hydrate Git LFS pointer files from the shared local cache.
    Hydrate(HydrateCommand),
    /// Dehydrate clean worktree files back to Git LFS pointers.
    Dehydrate(DehydrateCommand),
    /// Remove shared local cache objects with no registered worktree references.
    ///
    /// When run inside a Git worktree, the current worktree registration is
    /// refreshed before collection so its Git LFS pointers are considered
    /// reachable.
    Gc(GcCommand),
    /// Migrate objects from an existing Git LFS provider.
    Migrate(MigrateCommand),
}

#[derive(Debug, Args, Eq, PartialEq)]
struct ServeCommand {
    /// Host or interface address to bind.
    #[arg(long)]
    host: Option<String>,

    /// TCP port to bind.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct LoginCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct LogoutCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct InitCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,

    /// Write lfs.url to local Git config instead of committed .lfsconfig.
    #[arg(long)]
    local: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct StatusCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: Option<String>,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,

    /// Local cache root to inspect instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct PullCommand {
    /// Local cache root to use instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct HydrateCommand {
    /// Local cache root to use instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Git LFS pointer files to replace with cached object bytes.
    #[arg(value_name = "PATH", required = true)]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct DehydrateCommand {
    /// Local cache root to use instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Clean hydrated files to replace with Git LFS pointers.
    #[arg(value_name = "PATH", required = true)]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct GcCommand {
    /// Local cache root to clean instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Report objects and worktree registrations that would be removed.
    #[arg(long)]
    dry_run: bool,

    /// Permanently forget unavailable worktrees before removing objects.
    #[arg(long)]
    prune_unavailable_worktrees: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
struct MigrateCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,

    /// Local cache root to inspect instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Git remote that owns the source repository and source LFS objects.
    #[arg(long, value_name = "REMOTE", default_value = "origin")]
    source_remote: String,

    /// Confirm migration between different source and target repositories.
    #[arg(long)]
    allow_cross_remote: bool,

    /// Scan one selected branch, tag, or ref from a non-shallow repository.
    /// Can be repeated.
    #[arg(long = "ref", value_name = "REF", conflicts_with = "all_refs")]
    refs: Vec<String>,

    /// Scan local branches, tags, and source refs from a non-shallow repository.
    #[arg(long, conflicts_with = "refs")]
    all_refs: bool,

    /// Report the migration plan without fetching, uploading, or writing config.
    #[arg(long)]
    dry_run: bool,

    /// Include source-LFS purge guidance in the migration report.
    ///
    /// GitHub does not expose a normal self-service API for arbitrary LFS
    /// object deletion, so this flag never mutates the source. Dry runs report
    /// planning guidance; completed executions point at the durable verified
    /// receipt for the provider's supported cleanup process.
    #[arg(long)]
    purge_source_lfs: bool,
}

/// Parses process arguments, initializes tracing, and runs the requested command.
///
/// # Errors
///
/// Returns an error when tracing initialization fails or the selected command
/// cannot complete.
pub async fn run_from_env() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(&tracing_config(&cli)).context("failed to initialize tracing")?;
    dispatch(
        cli,
        crate::serve,
        run_init_to_stdout,
        run_login_to_stdio,
        run_logout_to_stdout,
        run_status_to_stdout,
        run_pull_to_stdout,
        run_hydrate_to_stdout,
        run_dehydrate_to_stdout,
        run_gc_to_stdout,
        run_migrate_to_stdout,
    )
    .await
}

#[expect(
    clippy::too_many_arguments,
    reason = "command dispatch keeps per-subcommand side effects injectable for focused tests"
)]
async fn dispatch<F, Fut, I, L, O, S, P, H, D, G, M>(
    cli: Cli,
    serve: F,
    init: I,
    login: L,
    logout: O,
    status: S,
    pull: P,
    hydrate: H,
    dehydrate: D,
    gc: G,
    migrate: M,
) -> anyhow::Result<()>
where
    F: FnOnce(ServeOptions) -> Fut,
    Fut: Future<Output = crate::ServerResult<()>>,
    I: FnOnce(InitCommand) -> anyhow::Result<()>,
    L: FnOnce(LoginCommand) -> anyhow::Result<()>,
    O: FnOnce(LogoutCommand) -> anyhow::Result<()>,
    S: FnOnce(StatusCommand, Option<PathBuf>) -> anyhow::Result<()>,
    P: FnOnce(PullCommand) -> anyhow::Result<()>,
    H: FnOnce(HydrateCommand) -> anyhow::Result<()>,
    D: FnOnce(DehydrateCommand) -> anyhow::Result<()>,
    G: FnOnce(GcCommand) -> anyhow::Result<()>,
    M: FnOnce(MigrateCommand, Option<PathBuf>) -> anyhow::Result<()>,
{
    // Keep command execution injectable only at the command boundary; each new
    // subcommand should add its own runner here rather than hiding side effects
    // in parser code.
    match cli.command {
        Command::Serve(command) => serve(command.serve_options(cli.config))
            .await
            .context("failed to run lfscloud server"),
        Command::Login(command) => login(command).context("failed to complete lfscloud login"),
        Command::Logout(command) => logout(command).context("failed to complete lfscloud logout"),
        Command::Init(command) => init(command).context("failed to resolve lfscloud init route"),
        Command::Status(command) => {
            status(command, cli.config).context("failed to check lfscloud status")
        }
        Command::Pull(command) => pull(command).context("failed to pull LFS objects"),
        Command::Hydrate(command) => hydrate(command).context("failed to hydrate paths"),
        Command::Dehydrate(command) => dehydrate(command).context("failed to dehydrate paths"),
        Command::Gc(command) => gc(command).context("failed to garbage collect local cache"),
        Command::Migrate(command) => {
            migrate(command, cli.config).context("failed to complete lfscloud migration")
        }
    }
}

fn tracing_config(cli: &Cli) -> TracingConfig {
    cli.log_level
        .as_deref()
        .map(|filter| TracingConfig::new(filter).without_env_filter())
        .unwrap_or_default()
}

impl ServeCommand {
    fn serve_options(self, config_path: Option<PathBuf>) -> ServeOptions {
        ServeOptions::new(config_path, self.host, self.port)
    }
}

fn run_init<W>(command: InitCommand, output: &mut W) -> anyhow::Result<()>
where
    W: Write,
{
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;

    run_init_from_dir(command, &current_dir, output)
}

fn run_init_to_stdout(command: InitCommand) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();

    run_init(command, &mut stdout)
}

fn run_login_to_stdio(command: LoginCommand) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

    if stdin.is_terminal() {
        run_login_with_token_reader(command, &mut stdout, read_login_token_from_terminal)
    } else {
        let mut input = stdin.lock();
        run_login(command, &mut input, &mut stdout)
    }
}

fn run_logout_to_stdout(command: LogoutCommand) -> anyhow::Result<()> {
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

fn run_status_to_stdout(
    command: StatusCommand,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    tokio::task::block_in_place(|| run_status_to_stdout_blocking(command, config_path))
}

fn run_status_to_stdout_blocking(
    command: StatusCommand,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_status_from_dir(
        command,
        config_path,
        &current_dir,
        &mut stdout,
        probe_server_reachable,
        |lfs_url| {
            GitCredentialLookup::new_with_insecure_http(lfs_url, true)
                .and_then(|lookup| lookup.lookup().map(|_| ()))
        },
        validate_status_storage,
    )
    .map_err(anyhow::Error::from)
}

fn run_pull_to_stdout(command: PullCommand) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_pull_from_dir(command, &current_dir, &mut stdout, fetch_git_lfs_objects)
        .map_err(anyhow::Error::from)
}

fn run_hydrate_to_stdout(command: HydrateCommand) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_hydrate_from_dir(command, &current_dir, &mut stdout).map_err(anyhow::Error::from)
}

fn run_dehydrate_to_stdout(command: DehydrateCommand) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_dehydrate_from_dir(command, &current_dir, &mut stdout).map_err(anyhow::Error::from)
}

fn run_gc_to_stdout(command: GcCommand) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    let current_dir = std::env::current_dir().context("failed to read current directory")?;

    run_gc_from_dir(command, &current_dir, &mut stdout).map_err(anyhow::Error::from)
}

fn run_migrate_to_stdout(
    command: MigrateCommand,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    if command.dry_run {
        return run_migrate_from_dir(
            command,
            config_path,
            &current_dir,
            &mut stdout,
            probe_server_reachable,
            |lfs_url| {
                GitCredentialLookup::new_with_insecure_http(lfs_url, true)
                    .and_then(|lookup| lookup.lookup().map(|_| ()))
            },
            validate_status_storage,
        )
        .map_err(anyhow::Error::from);
    }

    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(run_migrate_execution_from_dir(
            command,
            config_path,
            &current_dir,
            &mut stdout,
            probe_server_reachable,
            |lfs_url| {
                GitCredentialLookup::new_with_insecure_http(lfs_url, true)
                    .and_then(|lookup| lookup.lookup().map(|credential| credential.token().clone()))
            },
        ))
    })
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

trait LoginTerminal: BufRead {
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

fn run_init_from_dir<W>(
    command: InitCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
) -> anyhow::Result<()>
where
    W: Write,
{
    let repository = GitRepository::discover(start_dir.as_ref())
        .context("failed to inspect current Git repository")?;
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )
    .context("failed to build Git LFS URL")?;
    let change = repository
        .write_lfs_url(command.target(), &route.lfs_url)
        .context("failed to write Git LFS config")?;

    write_init_change(output, &change).context("failed to write init summary")
}

fn run_status_from_dir<W, P, A, S>(
    command: StatusCommand,
    config_path: Option<PathBuf>,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut probe_server: P,
    mut lookup_credential: A,
    mut validate_storage: S,
) -> CliResult<()>
where
    W: Write,
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
{
    let mut report = StatusReport::new();
    let config_path = config_path.unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let config = match ServerConfig::load_from_path(&config_path) {
        Ok(config) => {
            report.ok("config", format!("loaded {}", config_path.display()));
            Some(config)
        }
        Err(error) => {
            report.error("config", format!("{error}"));
            None
        }
    };
    let repository = match GitRepository::discover(start_dir.as_ref()) {
        Ok(repository) => {
            report.ok(
                "repository",
                format!(
                    "{} ({})",
                    repository.worktree_root.display(),
                    repository.remote.repository_label()
                ),
            );
            Some(repository)
        }
        Err(error) => {
            report.error("repository", format!("{error}"));
            None
        }
    };
    let server_url = command.server.clone().or_else(|| {
        config
            .as_ref()
            .map(|config| config.server.public_url.clone())
    });
    let allow_insecure_http = command.allow_insecure_http
        || (command.server.is_none()
            && config
                .as_ref()
                .is_some_and(|config| config.server.allow_insecure_http));

    if let Some(server_url) = server_url.as_deref() {
        let server_url_display = redacted_url_for_display(server_url);
        match probe_server(server_url) {
            Ok(()) => report.ok("server", format!("{server_url_display} is reachable")),
            Err(error) => report.error(
                "server",
                format!("{server_url_display} is unreachable: {error}"),
            ),
        }
    } else {
        report.error(
            "server",
            "missing --server and no server.public_url could be loaded from config",
        );
    }

    let route = match (server_url.as_deref(), repository.as_ref()) {
        (Some(server_url), Some(repository)) => {
            match LfsInitRoute::resolve_with_insecure_http(
                server_url,
                &repository.remote,
                allow_insecure_http,
            ) {
                Ok(route) => {
                    report.ok("route", redacted_url_for_display(&route.lfs_url));
                    Some(route)
                }
                Err(error) => {
                    report.error("route", format!("{error}"));
                    None
                }
            }
        }
        _ => None,
    };

    let mapping = match (config.as_ref(), repository.as_ref()) {
        (Some(config), Some(repository)) => {
            match config.repository_mapping_for_identity(
                &repository.remote.host,
                &repository.remote.owner,
                &repository.remote.name,
            ) {
                Some(mapping) => {
                    report.ok(
                        "mapping",
                        format!("{} -> {}", mapping.id, mapping.storage_provider),
                    );
                    Some(mapping)
                }
                None => {
                    report.error(
                        "mapping",
                        format!(
                            "no server config entry for {}",
                            repository.remote.repository_label()
                        ),
                    );
                    None
                }
            }
        }
        _ => None,
    };

    if let Some(route) = route.as_ref() {
        match lookup_credential(&route.lfs_url) {
            Ok(()) => report.ok("auth", "local LFS credential found"),
            Err(error) => report.error("auth", format!("{error}")),
        }
    }

    if let (Some(config), Some(mapping)) = (config.as_ref(), mapping) {
        if let Some(storage) = config.storage_providers.get(&mapping.storage_provider) {
            match validate_storage(storage) {
                Ok(()) => report.ok(
                    "storage",
                    format!(
                        "{} {} credential is configured",
                        storage.provider_type(),
                        storage.id()
                    ),
                ),
                Err(error) => report.error("storage", format!("{error}")),
            }
        } else {
            report.error(
                "storage",
                format!(
                    "mapping {} references unknown storage provider {}",
                    mapping.id, mapping.storage_provider
                ),
            );
        }
    }

    report_cache_status(&mut report, command.cache_root);
    report.write(output).map_err(output_error)?;

    if report.has_errors() {
        return Err(CliError::StatusFailed {
            message: "one or more status checks failed".to_owned(),
        });
    }

    Ok(())
}

fn run_pull_from_dir<W, F>(
    command: PullCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut fetch_lfs_objects: F,
) -> CliResult<()>
where
    W: Write,
    F: FnMut(&Path) -> CliResult<()>,
{
    let layout = local_cache_layout(command.cache_root)?;
    let repository = GitRepository::discover(start_dir.as_ref())?;
    let git_lfs_objects_dir = git_lfs_objects_dir(&repository)?;

    fetch_lfs_objects(&repository.worktree_root)?;
    register_current_worktree(&layout, &repository.worktree_root)?;

    let pointer_scan = current_checkout_lfs_pointer_scan(&repository.worktree_root)?;
    writeln!(output, "lfscloud pull").map_err(output_error)?;
    writeln!(output, "  fetched Git LFS objects").map_err(output_error)?;
    writeln!(
        output,
        "  tracked paths: {}",
        pointer_scan.tracked_path_count
    )
    .map_err(output_error)?;
    writeln!(output, "  pointers: {}", pointer_scan.pointer_files.len()).map_err(output_error)?;

    let mut first_failure = None;
    let mut failure_count = 0;
    for pointer_file in pointer_scan.pointer_files {
        let result = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &pointer_file.object)
            .map_err(local_cache_cli_error)
            .and_then(|ingest| {
                layout
                    .hydrate_pointer_file(&pointer_file.path)
                    .map_err(local_cache_cli_error)
                    .map(|materialization| (ingest, materialization))
            });

        match result {
            Ok((ingest, materialization)) => {
                write_pull_result(output, &ingest, &materialization).map_err(output_error)?;
            }
            Err(error) => {
                failure_count += 1;
                writeln!(output, "failed {}: {}", pointer_file.path.display(), error)
                    .map_err(output_error)?;
                first_failure.get_or_insert_with(|| {
                    (pointer_file.path, SanitizedMessage::new(error.to_string()))
                });
            }
        }
    }

    if let Some((path, message)) = first_failure {
        return Err(CliError::PullFailed {
            failures: failure_count,
            path,
            message,
        });
    }

    Ok(())
}

fn run_hydrate_from_dir<W>(
    command: HydrateCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    let layout = local_cache_layout(command.cache_root)?;
    let start_dir = start_dir.as_ref();
    let repository = GitRepository::discover(start_dir)?;
    register_worktree(&layout, &repository)?;

    for path in command.paths {
        let path = resolve_cli_path(start_dir, &path);
        let path = contained_worktree_file_path(&repository.worktree_root, &path, "hydration")?;
        let materialization = layout
            .hydrate_pointer_file(&path)
            .map_err(local_cache_cli_error)?;
        write_hydrate_result(output, &materialization).map_err(output_error)?;
    }

    Ok(())
}

fn run_dehydrate_from_dir<W>(
    command: DehydrateCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    let layout = local_cache_layout(command.cache_root)?;
    let start_dir = start_dir.as_ref();
    let repository = GitRepository::discover(start_dir)?;
    register_worktree(&layout, &repository)?;
    let git_lfs_objects_dir = git_lfs_objects_dir(&repository)?;

    for path in command.paths {
        let path = resolve_cli_path(start_dir, &path);
        let path = contained_worktree_file_path(&repository.worktree_root, &path, "dehydration")?;
        let object = indexed_lfs_object_for_dehydration(&repository.worktree_root, &path)?;
        let dehydration = layout
            .dehydrate_file(&object, &path)
            .map_err(local_cache_cli_error)?;
        publish_dehydrated_object_to_git_lfs(&layout, &git_lfs_objects_dir, &dehydration)?;
        write_dehydrate_result(output, &dehydration).map_err(output_error)?;
    }

    Ok(())
}

fn run_gc_from_dir<W>(
    command: GcCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    let layout = local_cache_layout(command.cache_root)?;
    register_current_worktree_for_gc(&layout, start_dir.as_ref())?;
    let report = layout
        .garbage_collect(command.dry_run, command.prune_unavailable_worktrees)
        .map_err(local_cache_cli_error)?;

    write_gc_result(output, layout.root(), &report).map_err(output_error)
}

fn run_migrate_from_dir<W, P, A, S>(
    command: MigrateCommand,
    config_path: Option<PathBuf>,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut probe_server: P,
    mut lookup_credential: A,
    mut validate_storage: S,
) -> CliResult<()>
where
    W: Write,
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
{
    if !command.dry_run {
        return Err(CliError::InvalidArguments {
            message: "the migration planning runner requires --dry-run".to_owned(),
        });
    }

    let start_dir = start_dir.as_ref();
    let repository = GitRepository::discover(start_dir)?;
    let source_repository = GitRepository::discover_with_remote(start_dir, &command.source_remote)?;
    if !same_repository_identity(&source_repository.remote, &repository.remote)
        && !command.allow_cross_remote
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "source remote {} identifies {}, but target remote {} identifies {}; rerun with --allow-cross-remote only after confirming this cross-repository migration",
                source_repository.remote.remote_name,
                source_repository.remote.repository_label(),
                repository.remote.remote_name,
                repository.remote.repository_label(),
            ),
        });
    }
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    let discovery = discover_git_lfs_migration_from_remote(start_dir, &command.source_remote)?;
    let scan = migration_pointer_scan(start_dir, &command, &command.source_remote)?;
    let cache_layout = Some(local_cache_layout(command.cache_root.clone())?);
    let availability =
        check_local_migration_objects(start_dir, scan.objects.iter(), cache_layout.as_ref())?;
    let config_path = config_path.unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let readiness_checks = migration_readiness_checks(
        &config_path,
        &repository,
        MigrationTargetReadiness {
            server_url: &command.server,
            lfs_url: &route.lfs_url,
        },
        &discovery,
        &mut probe_server,
        &mut lookup_credential,
        &mut validate_storage,
    );
    let source_purge = migration_source_purge_report(&discovery, command.purge_source_lfs);
    let report = MigrationDryRunReport {
        discovery,
        source_remote: source_repository.remote,
        target_remote: repository.remote.clone(),
        scan,
        availability,
        route,
        config_path,
        readiness_checks,
        would_touch_files: migration_dry_run_touched_files(&repository)?,
        source_purge,
    };

    write_migration_dry_run_report(output, &report).map_err(output_error)
}

#[derive(Debug)]
struct MigrationExecutionPreparation {
    repository: GitRepository,
    source_remote: GitRemote,
    route: LfsInitRoute,
    discovery: GitLfsMigrationDiscovery,
    cache_layout: LocalCacheLayout,
    purge_source_lfs: bool,
    allow_insecure_http: bool,
}

impl MigrationExecutionPreparation {
    fn scan_fetched_refs(self) -> CliResult<MigrationExecutionContext> {
        let scan = history_pointer_scan(
            MigrationScanMode::AllFetchedRefs,
            enumerate_fetched_ref_lfs_pointers_for_remote(
                &self.repository.worktree_root,
                &self.source_remote.remote_name,
            )?,
        );
        Ok(MigrationExecutionContext {
            repository: self.repository,
            source_remote: self.source_remote,
            route: self.route,
            discovery: self.discovery,
            scan,
            cache_layout: self.cache_layout,
            purge_source_lfs: self.purge_source_lfs,
        })
    }
}

#[derive(Debug)]
struct MigrationExecutionContext {
    repository: GitRepository,
    source_remote: GitRemote,
    route: LfsInitRoute,
    discovery: GitLfsMigrationDiscovery,
    scan: MigrationPointerScan,
    cache_layout: LocalCacheLayout,
    purge_source_lfs: bool,
}

#[derive(Debug)]
struct MigrationExecutionResult {
    source_fetch: MigrationSourceFetch,
    storage_upload: MigrationStorageUpload,
    config_changes: Vec<GitLfsConfigChange>,
}

struct MigrationGoogleDriveStorage {
    storage: GoogleDriveStorageConfig,
    repository_namespace: String,
    token_source: Arc<dyn GoogleDriveAccessTokenSource>,
    token_cache: GoogleDriveAccessTokenCache,
    metadata: Arc<MetadataDatabase>,
    #[cfg(test)]
    api_base_url: Option<String>,
}

impl MigrationGoogleDriveStorage {
    async fn object_store(&self) -> StorageResult<GoogleDriveObjectStore> {
        let token = self
            .token_cache
            .get_or_refresh(&self.storage, self.token_source.as_ref())
            .await?;
        #[cfg(test)]
        if let Some(api_base_url) = &self.api_base_url {
            return GoogleDriveObjectStore::with_api_base_url(
                self.storage.clone(),
                &self.repository_namespace,
                token,
                api_base_url,
            );
        }
        GoogleDriveObjectStore::new(self.storage.clone(), &self.repository_namespace, token)
    }

    fn validate_repository_namespace(&self, repository_namespace: &str) -> StorageResult<()> {
        if repository_namespace == self.repository_namespace {
            Ok(())
        } else {
            Err(StorageError::RepositoryNamespaceMismatch {
                provider: self.storage.id.clone(),
            })
        }
    }
}

impl StorageProvider for MigrationGoogleDriveStorage {
    fn provider_id(&self) -> &str {
        &self.storage.id
    }

    fn object_exists<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<bool>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            Ok(self
                .object_store()
                .await?
                .lookup_object(object)
                .await?
                .is_some())
        })
    }

    fn upload_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            let verified_file = GoogleDriveObjectStore::open_verified_staged_upload_file(
                &self.storage,
                object,
                source,
            )
            .await?;
            let _upload_lock = self
                .metadata
                .acquire_object_upload_lock(
                    repository_namespace.to_owned(),
                    self.storage.id.clone(),
                    object.clone(),
                )
                .await
                .map_err(|error| StorageError::Retryable {
                    provider: self.storage.id.clone(),
                    message: format!("migration upload lock failed: {error}"),
                })?;

            // Perform the lookup and possible upload while holding the
            // cross-process lock so a live server cannot win the race and
            // create a duplicate Drive file. File verification stays outside
            // the lock so cache-hit retries do not serialize large-file reads.
            let store = self.object_store().await?;
            store
                .upload_verified_object_idempotent(object, source, verified_file)
                .await
        })
    }

    fn download_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
        destination: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            StorageProvider::download_object(
                &self.object_store().await?,
                repository_namespace,
                object,
                destination,
            )
            .await
        })
    }

    fn delete_or_mark_object<'a>(
        &'a self,
        repository_namespace: &'a str,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
        Box::pin(async move {
            self.validate_repository_namespace(repository_namespace)?;
            StorageProvider::delete_or_mark_object(
                &self.object_store().await?,
                repository_namespace,
                object,
            )
            .await
        })
    }
}

async fn run_migrate_execution_from_dir<W, P, A>(
    command: MigrateCommand,
    config_path: Option<PathBuf>,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut probe_server: P,
    mut lookup_credential: A,
) -> CliResult<()>
where
    W: Write,
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<LfsSessionToken>,
{
    let preparation = prepare_migration_execution(command, start_dir.as_ref())?;

    // Prove the endpoint is usable before fetching source bytes or creating
    // target storage state. The credential was issued separately by `login`,
    // so checking it does not require changing the source LFS configuration.
    probe_server(&preparation.route.server_url)?;
    let token = lookup_credential(&preparation.route.lfs_url)?;
    probe_authenticated_migration_target(
        &preparation.route.lfs_url,
        preparation.allow_insecure_http,
        &token,
    )
    .await?;

    let config_path = config_path.unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let (mapping, storage) =
        migration_google_drive_storage(&config_path, &preparation.repository).await?;
    fetch_migration_git_refs(
        &preparation.repository.worktree_root,
        &preparation.source_remote.remote_name,
    )?;
    let context = preparation.scan_fetched_refs()?;
    let result = execute_migration_with_storage(&context, &mapping, &storage).await?;
    write_migration_execution_report(output, &context, &mapping, &result).map_err(output_error)
}

fn prepare_migration_execution(
    command: MigrateCommand,
    start_dir: &Path,
) -> CliResult<MigrationExecutionPreparation> {
    if command.dry_run {
        return Err(CliError::InvalidArguments {
            message: "migration execution cannot be prepared from a --dry-run request".to_owned(),
        });
    }
    if !command.all_refs {
        return Err(CliError::InvalidArguments {
            message: "migration execution requires --all-refs so reconfiguration cannot strand historical LFS objects; use --dry-run for narrower planning"
                .to_owned(),
        });
    }

    let repository = GitRepository::discover(start_dir)?;
    let source_repository = GitRepository::discover_with_remote(start_dir, &command.source_remote)?;
    if !same_repository_identity(&source_repository.remote, &repository.remote)
        && !command.allow_cross_remote
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "source remote {} identifies {}, but target remote {} identifies {}; rerun with --allow-cross-remote only after confirming this cross-repository migration",
                source_repository.remote.remote_name,
                source_repository.remote.repository_label(),
                repository.remote.remote_name,
                repository.remote.repository_label(),
            ),
        });
    }
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    let discovery = discover_git_lfs_migration_from_remote(start_dir, &command.source_remote)?;
    if !discovery.installation.installed {
        return Err(CliError::InvalidArguments {
            message: "migration execution requires Git LFS; install it and run `git lfs install` before retrying"
                .to_owned(),
        });
    }
    if discovery
        .source_endpoint
        .as_ref()
        .is_some_and(|source| source.url == route.lfs_url)
    {
        return Err(CliError::InvalidArguments {
            message: "source Git LFS endpoint already points at the requested LFS Cloud target"
                .to_owned(),
        });
    }
    Ok(MigrationExecutionPreparation {
        repository,
        source_remote: source_repository.remote,
        route,
        discovery,
        cache_layout: local_cache_layout(command.cache_root)?,
        purge_source_lfs: command.purge_source_lfs,
        allow_insecure_http: command.allow_insecure_http,
    })
}

async fn migration_google_drive_storage(
    config_path: &Path,
    repository: &GitRepository,
) -> CliResult<(RepositoryMapping, MigrationGoogleDriveStorage)> {
    let config = ServerConfig::load_from_path(config_path)?;
    let mapping = config
        .repository_mapping_for_identity(
            &repository.remote.host,
            &repository.remote.owner,
            &repository.remote.name,
        )
        .cloned()
        .ok_or_else(|| CliError::InvalidArguments {
            message: format!(
                "server config has no repository mapping for {}",
                repository.remote.repository_label()
            ),
        })?;
    let storage = config
        .storage_providers
        .get(&mapping.storage_provider)
        .cloned()
        .ok_or_else(|| CliError::InvalidArguments {
            message: format!(
                "repository mapping {} references unknown storage provider {}",
                mapping.id, mapping.storage_provider
            ),
        })?;
    let StorageProviderConfig::GoogleDrive(storage) = storage;
    let token_source: Arc<dyn GoogleDriveAccessTokenSource> =
        Arc::new(GoogleDriveGcloudTokenProvider::new());
    let token_cache = GoogleDriveAccessTokenCache::default();
    let token = token_cache
        .get_or_refresh(&storage, token_source.as_ref())
        .await
        .map_err(MigrationError::from)?;
    GoogleDriveRootValidator::new()
        .map_err(MigrationError::from)?
        .validate_root_folder(&storage, &token)
        .await
        .map_err(MigrationError::from)?;

    let metadata = Arc::new(MetadataDatabase::open(&config.server.metadata_path)?);
    metadata.sync_config(&config)?;
    Ok((
        mapping.clone(),
        MigrationGoogleDriveStorage {
            storage,
            repository_namespace: mapping.id,
            token_source,
            token_cache,
            metadata,
            #[cfg(test)]
            api_base_url: None,
        },
    ))
}

async fn execute_migration_with_storage(
    context: &MigrationExecutionContext,
    mapping: &RepositoryMapping,
    storage: &dyn StorageProvider,
) -> CliResult<MigrationExecutionResult> {
    let repository_namespace = storage_namespace_for_context(context, mapping)?;
    if context.scan.objects.is_empty() {
        return Err(CliError::InvalidArguments {
            message: "migration found no non-empty Git LFS objects across the selected history"
                .to_owned(),
        });
    }
    let source_fetch = fetch_missing_migration_objects_from_remote(
        &context.repository.worktree_root,
        context.scan.objects.iter(),
        Some(&context.cache_layout),
        &context.source_remote.remote_name,
        MigrationFetchMode::AllFetchedRefs,
    )?;
    if let Some(object) = source_fetch.unavailable_objects.first() {
        return Err(MigrationError::SourceObjectMissing {
            oid: object.oid.as_hex().to_owned(),
            size: object.size.bytes(),
        }
        .into());
    }

    let storage_upload =
        upload_migration_objects_to_storage(&source_fetch.after, storage, repository_namespace)
            .await?;
    if let Some(first) = storage_upload.failed_objects.first() {
        return Err(CliError::MigrationUploadFailed {
            failures: storage_upload.failed_objects.len(),
            oid: first.object.oid.as_hex().to_owned(),
            message: first.message.clone(),
        });
    }

    // Persist both forms after every object has a synchronized successful
    // checkpoint record. The local override keeps historical commits working
    // even when they predate the newly committed `.lfsconfig` file.
    let config_changes = [
        GitLfsConfigTarget::WorktreeFile,
        GitLfsConfigTarget::LocalRepository,
    ]
    .into_iter()
    .map(|target| {
        context
            .repository
            .write_lfs_url(target, &context.route.lfs_url)
    })
    .collect::<CliResult<Vec<_>>>()?;

    Ok(MigrationExecutionResult {
        source_fetch,
        storage_upload,
        config_changes,
    })
}

fn storage_namespace_for_context<'a>(
    context: &MigrationExecutionContext,
    mapping: &'a RepositoryMapping,
) -> CliResult<&'a str> {
    if mapping
        .host
        .eq_ignore_ascii_case(&context.repository.remote.host)
        && mapping
            .owner
            .eq_ignore_ascii_case(&context.repository.remote.owner)
        && mapping
            .name
            .eq_ignore_ascii_case(&context.repository.remote.name)
    {
        Ok(&mapping.id)
    } else {
        Err(CliError::InvalidArguments {
            message: format!(
                "repository mapping {} does not match migration target {}",
                mapping.id,
                context.repository.remote.repository_label()
            ),
        })
    }
}

fn write_migration_execution_report<W>(
    output: &mut W,
    context: &MigrationExecutionContext,
    mapping: &RepositoryMapping,
    result: &MigrationExecutionResult,
) -> io::Result<()>
where
    W: Write,
{
    let already_local = result.source_fetch.before.available_objects().len();
    writeln!(output, "lfscloud migrate complete")?;
    writeln!(output, "  mode: {}", context.scan.mode.label())?;
    writeln!(
        output,
        "  source remote: {} ({})",
        context.source_remote.remote_name,
        context.source_remote.repository_label()
    )?;
    writeln!(
        output,
        "  source: {}",
        source_endpoint_display(&context.discovery)
    )?;
    writeln!(
        output,
        "  target: {}",
        redacted_url_for_display(&context.route.lfs_url)
    )?;
    writeln!(output, "  repository namespace: {}", mapping.id)?;
    writeln!(
        output,
        "  refs scanned: {}",
        context.scan.refs_scanned.len()
    )?;
    writeln!(
        output,
        "  objects discovered: {} ({} bytes total)",
        context.scan.objects.len(),
        migration_objects_total_bytes(context.scan.objects.iter())
    )?;
    writeln!(
        output,
        "  source objects: {} already local, {} fetched",
        already_local,
        result.source_fetch.fetched_objects.len()
    )?;
    writeln!(
        output,
        "  target objects: {} uploaded, {} already present",
        result.storage_upload.uploaded_objects.len(),
        result.storage_upload.already_present_objects.len()
    )?;
    writeln!(
        output,
        "  durable receipt: {}",
        result.storage_upload.checkpoint_path.display()
    )?;
    writeln!(output, "  repository configuration:")?;
    for change in &result.config_changes {
        writeln!(
            output,
            "    {}: {}",
            change.target.label(),
            change.path.display()
        )?;
    }
    writeln!(
        output,
        "  next step: commit .lfsconfig so new clones use LFS Cloud"
    )?;
    if context.purge_source_lfs {
        writeln!(output, "  source purge:")?;
        writeln!(output, "    automatic purge: unsupported")?;
        writeln!(
            output,
            "    verified candidates: {}",
            context.scan.objects.len()
        )?;
        writeln!(
            output,
            "    use the durable receipt above with the source provider's supported cleanup process"
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct MigrationPointerScan {
    mode: MigrationScanMode,
    refs_scanned: Vec<String>,
    pointer_file_count: usize,
    objects: Vec<LfsObject>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationScanMode {
    CurrentCheckout,
    SelectedRefs,
    AllFetchedRefs,
}

impl MigrationScanMode {
    fn label(self) -> &'static str {
        match self {
            Self::CurrentCheckout => "current-checkout",
            Self::SelectedRefs => "selected-refs",
            Self::AllFetchedRefs => "all-refs",
        }
    }

    fn scope_label(self) -> &'static str {
        match self {
            Self::CurrentCheckout => "current checkout index only",
            Self::SelectedRefs => "selected refs only",
            Self::AllFetchedRefs => {
                "all local branches, tags, and fetched refs for the source remote"
            }
        }
    }

    fn scope_warning(self) -> Option<&'static str> {
        match self {
            Self::CurrentCheckout => Some(
                "other refs were not scanned and may reference additional LFS objects; use --all-refs for a full provider move",
            ),
            Self::SelectedRefs | Self::AllFetchedRefs => None,
        }
    }
}

#[derive(Debug)]
struct MigrationDryRunReport {
    discovery: GitLfsMigrationDiscovery,
    source_remote: GitRemote,
    target_remote: GitRemote,
    scan: MigrationPointerScan,
    availability: LocalMigrationObjectAvailability,
    route: LfsInitRoute,
    config_path: PathBuf,
    readiness_checks: Vec<MigrationReadinessCheck>,
    would_touch_files: Vec<PathBuf>,
    source_purge: Option<MigrationSourcePurgeReport>,
}

#[derive(Debug)]
struct MigrationReadinessCheck {
    name: &'static str,
    level: StatusLevel,
    message: String,
}

#[derive(Clone, Copy, Debug)]
struct MigrationTargetReadiness<'a> {
    server_url: &'a str,
    lfs_url: &'a str,
}

#[derive(Debug)]
enum MigrationSourcePurgeReport {
    GitHub,
    NotConfigured,
    Unsupported { host: String },
}

fn migration_pointer_scan(
    start_dir: &Path,
    command: &MigrateCommand,
    source_remote: &str,
) -> CliResult<MigrationPointerScan> {
    if command.all_refs {
        let history = enumerate_fetched_ref_lfs_pointers_for_remote(start_dir, source_remote)?;
        return Ok(history_pointer_scan(
            MigrationScanMode::AllFetchedRefs,
            history,
        ));
    }

    if !command.refs.is_empty() {
        let history = enumerate_selected_ref_lfs_pointers(start_dir, command.refs.iter())?;
        return Ok(history_pointer_scan(
            MigrationScanMode::SelectedRefs,
            history,
        ));
    }

    let checkout = enumerate_current_checkout_lfs_pointers(start_dir)?;
    let objects = dedupe_lfs_objects(checkout.pointers.iter().map(|pointer| &pointer.object));

    Ok(MigrationPointerScan {
        mode: MigrationScanMode::CurrentCheckout,
        refs_scanned: vec!["current checkout".to_owned()],
        pointer_file_count: checkout.pointers.len(),
        objects,
    })
}

fn same_repository_identity(left: &GitRemote, right: &GitRemote) -> bool {
    left.host.eq_ignore_ascii_case(&right.host)
        && left.owner.eq_ignore_ascii_case(&right.owner)
        && left.name.eq_ignore_ascii_case(&right.name)
}

fn history_pointer_scan(
    mode: MigrationScanMode,
    history: GitLfsHistoryPointers,
) -> MigrationPointerScan {
    let objects = dedupe_lfs_objects(history.pointers.iter().map(|pointer| &pointer.object));

    MigrationPointerScan {
        mode,
        refs_scanned: history
            .refs
            .into_iter()
            .map(|scanned| scanned.name)
            .collect(),
        pointer_file_count: history.pointers.len(),
        objects,
    }
}

fn dedupe_lfs_objects<'a>(objects: impl IntoIterator<Item = &'a LfsObject>) -> Vec<LfsObject> {
    objects
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect()
}

fn migration_readiness_checks<P, A, S>(
    config_path: &Path,
    repository: &GitRepository,
    target: MigrationTargetReadiness<'_>,
    discovery: &GitLfsMigrationDiscovery,
    probe_server: &mut P,
    lookup_credential: &mut A,
    validate_storage: &mut S,
) -> Vec<MigrationReadinessCheck>
where
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
{
    let mut checks = Vec::new();
    checks.push(migration_git_lfs_readiness_check(discovery));
    checks.push(migration_filter_readiness_check(discovery));
    checks.push(migration_source_readiness_check(discovery));
    checks.push(migration_target_readiness_check(
        target.server_url,
        probe_server,
    ));

    checks.push(match lookup_credential(target.lfs_url) {
        Ok(()) => MigrationReadinessCheck {
            name: "lfs-credential",
            level: StatusLevel::Ok,
            message: "local LFS credential found; server acceptance not probed".to_owned(),
        },
        Err(error) => MigrationReadinessCheck {
            name: "lfs-credential",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        },
    });

    match ServerConfig::load_from_path(config_path) {
        Ok(config) => {
            checks.push(MigrationReadinessCheck {
                name: "config",
                level: StatusLevel::Ok,
                message: format!("loaded {}", config_path.display()),
            });
            migration_config_readiness_checks(&mut checks, &config, repository, validate_storage);
        }
        Err(error) => checks.push(MigrationReadinessCheck {
            name: "config",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        }),
    }

    checks
}

fn migration_git_lfs_readiness_check(
    discovery: &GitLfsMigrationDiscovery,
) -> MigrationReadinessCheck {
    if discovery.installation.installed {
        return MigrationReadinessCheck {
            name: "git-lfs",
            level: StatusLevel::Ok,
            message: discovery
                .installation
                .version
                .clone()
                .unwrap_or_else(|| "git lfs is available locally".to_owned()),
        };
    }

    MigrationReadinessCheck {
        name: "git-lfs",
        level: StatusLevel::Warning,
        message: discovery.installation.diagnostic.as_ref().map_or_else(
            || "git lfs is not available locally".to_owned(),
            ToString::to_string,
        ),
    }
}

fn migration_filter_readiness_check(
    discovery: &GitLfsMigrationDiscovery,
) -> MigrationReadinessCheck {
    let filters = &discovery.filters;
    let missing = [
        ("filter.lfs.clean", filters.clean.is_none()),
        ("filter.lfs.smudge", filters.smudge.is_none()),
        ("filter.lfs.process", filters.process.is_none()),
        ("filter.lfs.required", filters.required.is_none()),
    ]
    .into_iter()
    .filter_map(|(name, is_missing)| is_missing.then_some(name))
    .collect::<Vec<_>>();

    if missing.is_empty() {
        MigrationReadinessCheck {
            name: "lfs-filters",
            level: StatusLevel::Ok,
            message: "clean, smudge, process, and required filters are configured locally"
                .to_owned(),
        }
    } else {
        MigrationReadinessCheck {
            name: "lfs-filters",
            level: StatusLevel::Warning,
            message: format!(
                "missing local Git LFS filter settings: {}",
                missing.join(", ")
            ),
        }
    }
}

fn migration_target_readiness_check<P>(
    server_url: &str,
    probe_server: &mut P,
) -> MigrationReadinessCheck
where
    P: FnMut(&str) -> CliResult<()>,
{
    let display = redacted_url_for_display(server_url);
    match probe_server(server_url) {
        Ok(()) => MigrationReadinessCheck {
            name: "server-tcp",
            level: StatusLevel::Ok,
            message: format!(
                "{display} TCP endpoint is reachable; server authentication and repository access not probed"
            ),
        },
        Err(error) => MigrationReadinessCheck {
            name: "server-tcp",
            level: StatusLevel::Warning,
            message: format!(
                "{display} TCP endpoint is unreachable: {error}; server authentication and repository access not probed"
            ),
        },
    }
}

fn migration_source_readiness_check(
    discovery: &GitLfsMigrationDiscovery,
) -> MigrationReadinessCheck {
    match &discovery.source_endpoint {
        Some(endpoint) => MigrationReadinessCheck {
            name: "source-config",
            level: StatusLevel::Ok,
            message: format!(
                "{} ({}); source repository access not probed",
                redacted_url_for_display(&endpoint.url),
                source_endpoint_source_label(endpoint.source)
            ),
        },
        None => MigrationReadinessCheck {
            name: "source-config",
            level: StatusLevel::Warning,
            message:
                "source Git LFS endpoint is not configured; source repository access not probed"
                    .to_owned(),
        },
    }
}

fn migration_config_readiness_checks<S>(
    checks: &mut Vec<MigrationReadinessCheck>,
    config: &ServerConfig,
    repository: &GitRepository,
    validate_storage: &mut S,
) where
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
{
    let Some(mapping) = config.repository_mapping_for_identity(
        &repository.remote.host,
        &repository.remote.owner,
        &repository.remote.name,
    ) else {
        checks.push(MigrationReadinessCheck {
            name: "mapping",
            level: StatusLevel::Warning,
            message: format!(
                "no server config entry for {}",
                repository.remote.repository_label()
            ),
        });
        return;
    };

    checks.push(MigrationReadinessCheck {
        name: "mapping",
        level: StatusLevel::Ok,
        message: format!("{} -> {}", mapping.id, mapping.storage_provider),
    });

    let Some(storage) = config.storage_providers.get(&mapping.storage_provider) else {
        checks.push(MigrationReadinessCheck {
            name: "storage-credential",
            level: StatusLevel::Warning,
            message: format!(
                "mapping {} references unknown storage provider {}",
                mapping.id, mapping.storage_provider
            ),
        });
        return;
    };

    checks.push(match validate_storage(storage) {
        Ok(()) => MigrationReadinessCheck {
            name: "storage-credential",
            level: StatusLevel::Ok,
            message: format!(
                "{} {} credential loads locally; Drive root access not probed",
                storage.provider_type(),
                storage.id()
            ),
        },
        Err(error) => MigrationReadinessCheck {
            name: "storage-credential",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        },
    });
}

fn migration_dry_run_touched_files(repository: &GitRepository) -> CliResult<Vec<PathBuf>> {
    Ok(vec![
        repository.worktree_root.join(".lfsconfig"),
        repository.local_git_config_path()?,
    ])
}

fn migration_source_purge_report(
    discovery: &GitLfsMigrationDiscovery,
    requested: bool,
) -> Option<MigrationSourcePurgeReport> {
    if !requested {
        return None;
    }

    match discovery
        .source_endpoint
        .as_ref()
        .map(|endpoint| source_endpoint_provider_label(&endpoint.url))
    {
        Some(label) if label.eq_ignore_ascii_case("github.com") => {
            Some(MigrationSourcePurgeReport::GitHub)
        }
        Some(label) => Some(MigrationSourcePurgeReport::Unsupported { host: label }),
        None => Some(MigrationSourcePurgeReport::NotConfigured),
    }
}

fn source_endpoint_provider_label(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| SOURCE_PROVIDER_UNKNOWN_LABEL.to_owned())
}

fn source_endpoint_display(discovery: &GitLfsMigrationDiscovery) -> String {
    discovery
        .source_endpoint
        .as_ref()
        .map(|endpoint| redacted_url_for_display(&endpoint.url))
        .unwrap_or_else(|| SOURCE_ENDPOINT_UNSET_LABEL.to_owned())
}

fn write_migration_dry_run_report<W>(
    output: &mut W,
    report: &MigrationDryRunReport,
) -> io::Result<()>
where
    W: Write,
{
    let available_objects = report.availability.available_objects();
    let unavailable_objects = report.availability.unavailable_objects();
    let available_count = available_objects.len();
    let fetch_count = unavailable_objects.len();
    let total_bytes = migration_objects_total_bytes(report.scan.objects.iter());
    let available_bytes =
        migration_objects_total_bytes(available_objects.iter().map(|local| &local.object));
    let fetch_bytes =
        migration_objects_total_bytes(unavailable_objects.iter().map(|local| &local.object));

    writeln!(output, "lfscloud migrate dry-run")?;
    writeln!(
        output,
        "  worktree: {}",
        report.discovery.worktree_root.display()
    )?;
    writeln!(output, "  mode: {}", report.scan.mode.label())?;
    writeln!(output, "  scope: {}", report.scan.mode.scope_label())?;
    if let Some(warning) = report.scan.mode.scope_warning() {
        writeln!(output, "  warning: {warning}")?;
    }
    writeln!(
        output,
        "  source remote: {} ({})",
        report.source_remote.remote_name,
        report.source_remote.repository_label()
    )?;
    writeln!(
        output,
        "  target remote: {} ({})",
        report.target_remote.remote_name,
        report.target_remote.repository_label()
    )?;
    writeln!(
        output,
        "  source: {}",
        source_endpoint_display(&report.discovery)
    )?;
    writeln!(
        output,
        "  target: {}",
        redacted_url_for_display(&report.route.lfs_url)
    )?;
    writeln!(
        output,
        "  tracked LFS patterns: {}",
        report.discovery.tracked_patterns.len()
    )?;
    for tracked in &report.discovery.tracked_patterns {
        writeln!(
            output,
            "    {} ({}; {})",
            tracked.pattern,
            tracked.source.display(),
            tracked.attributes.join(" ")
        )?;
    }
    writeln!(output, "  config: {}", report.config_path.display())?;
    writeln!(output, "  refs scanned: {}", report.scan.refs_scanned.len())?;
    for ref_name in &report.scan.refs_scanned {
        writeln!(output, "    {ref_name}")?;
    }
    writeln!(
        output,
        "  files touched: {} would update",
        report.would_touch_files.len()
    )?;
    for path in &report.would_touch_files {
        writeln!(output, "    {}", path.display())?;
    }
    writeln!(
        output,
        "  pointer files: {}",
        report.scan.pointer_file_count
    )?;
    writeln!(
        output,
        "  objects discovered: {} ({} bytes total)",
        report.scan.objects.len(),
        total_bytes
    )?;
    for object in report
        .scan
        .objects
        .iter()
        .take(MIGRATION_OBJECT_REPORT_LIMIT)
    {
        writeln!(
            output,
            "    sha256:{} ({} bytes)",
            object.oid,
            object.size.bytes()
        )?;
    }
    if report.scan.objects.len() > MIGRATION_OBJECT_REPORT_LIMIT {
        writeln!(
            output,
            "    ... {} more objects omitted",
            report.scan.objects.len() - MIGRATION_OBJECT_REPORT_LIMIT
        )?;
    }
    writeln!(
        output,
        "  objects fetched: {fetch_count} would fetch, {available_count} already local"
    )?;
    writeln!(
        output,
        "    {fetch_bytes} bytes would fetch, {available_bytes} bytes already local"
    )?;
    writeln!(
        output,
        "  source objects: {available_count} local, {fetch_count} missing locally ({available_bytes} local bytes, {fetch_bytes} missing bytes)"
    )?;
    writeln!(
        output,
        "  target objects: 0 confirmed new, 0 confirmed existing, {} unknown ({} bytes unknown)",
        report.scan.objects.len(),
        total_bytes
    )?;
    writeln!(
        output,
        "    target storage not probed during dry-run; execution checks existence before upload"
    )?;
    writeln!(
        output,
        "  local readiness checks (no remote access probes):"
    )?;
    for check in &report.readiness_checks {
        writeln!(
            output,
            "    {:<10} {:<7} {}",
            check.name,
            check.level.label(),
            check.message
        )?;
    }
    write_migration_dry_run_warnings(output, report, fetch_count, fetch_bytes)?;
    if let Some(source_purge) = &report.source_purge {
        write_migration_source_purge_report(output, source_purge, report)?;
    }

    Ok(())
}

fn migration_objects_total_bytes<'a>(objects: impl IntoIterator<Item = &'a LfsObject>) -> u128 {
    objects
        .into_iter()
        .map(|object| u128::from(object.size.bytes()))
        .sum()
}

fn write_migration_dry_run_warnings<W>(
    output: &mut W,
    report: &MigrationDryRunReport,
    fetch_count: usize,
    fetch_bytes: u128,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(output, "  warnings:")?;
    if report.discovery.tracked_patterns.is_empty() {
        writeln!(
            output,
            "    warning: no tracked LFS patterns were discovered"
        )?;
    }
    if fetch_count > 0 {
        let noun = if fetch_count == 1 {
            "object"
        } else {
            "objects"
        };
        let verb = if fetch_count == 1 { "has" } else { "have" };
        writeln!(
            output,
            "    warning: {fetch_count} {noun} ({fetch_bytes} bytes) {verb} no verified local source; source fetch and remote availability must succeed during execution"
        )?;
    }
    writeln!(
        output,
        "    warning: source and target repository permissions were not probed"
    )?;
    writeln!(
        output,
        "    warning: target storage quota and free capacity were not probed"
    )?;
    if let Some(MigrationSourcePurgeReport::NotConfigured) = &report.source_purge {
        writeln!(
            output,
            "    warning: source purge availability is unknown without a configured source endpoint"
        )?;
    } else if let Some(MigrationSourcePurgeReport::Unsupported { host }) = &report.source_purge {
        writeln!(
            output,
            "    warning: automatic source purge is unsupported for {host}"
        )?;
    }

    Ok(())
}

fn write_migration_source_purge_report<W>(
    output: &mut W,
    source_purge: &MigrationSourcePurgeReport,
    report: &MigrationDryRunReport,
) -> io::Result<()>
where
    W: Write,
{
    let total_bytes = migration_objects_total_bytes(report.scan.objects.iter());

    writeln!(output, "  source purge:")?;
    writeln!(
        output,
        "    source: {}",
        source_endpoint_display(&report.discovery)
    )?;
    match source_purge {
        MigrationSourcePurgeReport::GitHub => {
            writeln!(output, "    provider: GitHub")?;
            writeln!(output, "    automatic purge: unsupported")?;
            writeln!(
                output,
                "    planned candidates: {} ({} bytes; upload not verified)",
                report.scan.objects.len(),
                total_bytes
            )?;
            writeln!(output, "    GitHub LFS purge requires GitHub Support.")?;
            writeln!(
                output,
                "    support URL: https://support.github.com/contact-next/product-selection/repositories"
            )?;
            writeln!(
                output,
                "    suggested subject: Purge Git LFS objects after migration"
            )?;
            writeln!(
                output,
                "    instructions: use GitHub's repository support flow or Virtual Agent only after migration execution verifies every object at the destination."
            )?;
            writeln!(
                output,
                "    purge manifest: unavailable during dry-run planning"
            )?;
            writeln!(
                output,
                "    requirement: generate purge input only from a durable, integrity-verified migration receipt; planned objects are not proof of upload."
            )?;
        }
        MigrationSourcePurgeReport::NotConfigured => {
            writeln!(output, "    provider: {SOURCE_PROVIDER_UNKNOWN_LABEL}")?;
            writeln!(
                output,
                "    automatic purge: unavailable because no source Git LFS endpoint was detected."
            )?;
        }
        MigrationSourcePurgeReport::Unsupported { host } => {
            writeln!(output, "    provider: {host}")?;
            writeln!(
                output,
                "    automatic purge: unsupported by this helper; no source-provider cleanup will be attempted."
            )?;
        }
    }

    Ok(())
}

fn source_endpoint_source_label(source: GitLfsSourceEndpointSource) -> &'static str {
    match source {
        GitLfsSourceEndpointSource::LocalGitConfig => "local Git config",
        GitLfsSourceEndpointSource::RemoteGitConfig => "remote Git config",
        GitLfsSourceEndpointSource::WorktreeLfsConfig => ".lfsconfig",
        GitLfsSourceEndpointSource::RemoteUrlDefault => "remote URL default",
    }
}

fn register_current_worktree_for_gc(layout: &LocalCacheLayout, start_dir: &Path) -> CliResult<()> {
    match register_current_worktree(layout, start_dir) {
        Ok(()) => Ok(()),
        Err(error) if is_git_worktree_discovery_error(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_git_worktree_discovery_error(error: &CliError) -> bool {
    match error {
        CliError::ExternalCommand {
            command, stderr, ..
        } if command == "git rev-parse --show-toplevel" => {
            let stderr = stderr.as_str();
            stderr.contains("not a git repository")
                || stderr.contains("this operation must be run in a work tree")
        }
        _ => false,
    }
}

fn register_current_worktree(layout: &LocalCacheLayout, start_dir: &Path) -> CliResult<()> {
    let repository = GitRepository::discover(start_dir)?;
    register_worktree(layout, &repository)
}

fn register_worktree(layout: &LocalCacheLayout, repository: &GitRepository) -> CliResult<()> {
    let repository_id = repository.remote.repository_label();
    let git_dir = repository.git_dir_path()?;
    let registration = LocalCacheWorktreeRegistration::new(
        repository_id,
        repository.worktree_root.clone(),
        git_dir,
    )
    .map_err(local_cache_cli_error)?;

    layout
        .register_worktree(registration)
        .map_err(local_cache_cli_error)?;

    Ok(())
}

fn local_cache_layout(cache_root: Option<PathBuf>) -> CliResult<LocalCacheLayout> {
    match cache_root {
        Some(cache_root) => Ok(LocalCacheLayout::new(cache_root)),
        None => match default_cache_home_dir() {
            Some(home_dir) => Ok(LocalCacheLayout::from_home_dir(home_dir)),
            None => Err(default_cache_root_error()),
        },
    }
}

fn default_cache_home_dir() -> Option<OsString> {
    std::env::var_os("HOME").or_else(|| {
        if cfg!(windows) {
            std::env::var_os("USERPROFILE")
        } else {
            None
        }
    })
}

fn default_cache_root_error() -> CliError {
    CliError::InvalidArguments {
        message: if cfg!(windows) {
            "HOME or USERPROFILE is not set and --cache-root was not provided".to_owned()
        } else {
            "HOME is not set and --cache-root was not provided".to_owned()
        },
    }
}

fn resolve_cli_path(start_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        start_dir.join(path)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CurrentCheckoutLfsPointerFile {
    path: PathBuf,
    object: LfsObject,
}

#[derive(Debug, Eq, PartialEq)]
struct CurrentCheckoutLfsPointerScan {
    tracked_path_count: usize,
    pointer_files: Vec<CurrentCheckoutLfsPointerFile>,
}

fn fetch_git_lfs_objects(worktree_root: &Path) -> CliResult<()> {
    let mut command = ProcessCommand::new("git");
    command.args(["lfs", "fetch"]).current_dir(worktree_root);
    let output = run_bounded_child_command(
        &mut command,
        "git lfs fetch",
        PULL_FETCH_TIMEOUT,
        MAX_PULL_FETCH_OUTPUT_BYTES,
    )?;

    if output.status.success() {
        Ok(())
    } else {
        Err(CliError::ExternalCommand {
            command: "git lfs fetch".to_owned(),
            status: process_status_text(output.status),
            stderr: sanitized_external_failure_output(&output.stderr, &output.stdout),
        })
    }
}

/// Runs a child while bounding its lifetime and retained output.
///
/// Both output streams are drained on separate threads so a chatty stream
/// cannot fill its OS pipe while the parent waits on the other stream. Each
/// reader retains at most `max_output_bytes`; crossing either limit terminates
/// the whole process tree instead of merely truncating an otherwise unbounded
/// producer.
fn run_bounded_child_command(
    command: &mut ProcessCommand,
    command_name: &str,
    timeout: Duration,
    max_output_bytes: usize,
) -> CliResult<ChildProcessOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_tree(command);

    let mut child = command.spawn().map_err(|source| CliError::Io {
        context: format!("failed to start {command_name}"),
        source,
    })?;
    wait_for_child(
        &mut child,
        command_name,
        ChildProcessOptions {
            timeout: Some(timeout),
            stdout: PipeCapture::HardLimit {
                limit: max_output_bytes,
            },
            stderr: PipeCapture::HardLimit {
                limit: max_output_bytes,
            },
            inherited_pipe_is_error: false,
        },
    )
    .map_err(|error| child_process_cli_error(error, command_name))
}

fn child_process_cli_error(error: ChildProcessError, command_name: &str) -> CliError {
    match error {
        ChildProcessError::Io { context, source } => CliError::Io { context, source },
        ChildProcessError::TimedOut {
            timeout,
            stdout,
            stderr,
        } => CliError::ExternalCommand {
            command: command_name.to_owned(),
            status: format!("timed out after {} seconds", timeout.as_secs_f64()),
            stderr: sanitized_external_failure_output(&stderr, &stdout),
        },
        ChildProcessError::OutputLimit { stream, limit } => CliError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new(format!("{stream} exceeded the {limit}-byte limit")),
        },
        ChildProcessError::InheritedPipe => CliError::Io {
            context: format!("timed out draining output from {command_name}"),
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                "child output pipes remained open after process exit",
            ),
        },
    }
}

#[cfg(test)]
fn current_checkout_lfs_pointer_files(
    worktree_root: &Path,
) -> CliResult<Vec<CurrentCheckoutLfsPointerFile>> {
    Ok(current_checkout_lfs_pointer_scan(worktree_root)?.pointer_files)
}

fn current_checkout_lfs_pointer_scan(
    worktree_root: &Path,
) -> CliResult<CurrentCheckoutLfsPointerScan> {
    let lfs_tracked_paths = current_checkout_lfs_tracked_paths(worktree_root)?;
    let mut pointer_files = Vec::new();
    for relative_path in &lfs_tracked_paths {
        let path = worktree_root.join(relative_path);
        let Some(pointer) = read_current_checkout_pointer_candidate(&path)? else {
            continue;
        };
        if pointer.is_empty() {
            continue;
        }

        pointer_files.push(CurrentCheckoutLfsPointerFile {
            path,
            object: pointer.object,
        });
    }

    Ok(CurrentCheckoutLfsPointerScan {
        tracked_path_count: lfs_tracked_paths.len(),
        pointer_files,
    })
}

fn current_checkout_lfs_tracked_paths(worktree_root: &Path) -> CliResult<Vec<PathBuf>> {
    let output = ProcessCommand::new("git")
        .args(["ls-files", "-z"])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git ls-files -z".to_owned(),
            source,
        })?;

    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git ls-files -z".to_owned(),
            status: process_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    let tracked_paths = output.stdout;
    let mut lfs_tracked_paths = Vec::new();
    if tracked_paths.is_empty() {
        return Ok(lfs_tracked_paths);
    }

    let output = git_check_attr_filter(worktree_root, &tracked_paths)?;
    lfs_tracked_paths.extend(
        parse_lfs_filter_attribute_paths(&output.stdout).map_err(|error| {
            cli_git_path_output_error(error, "git check-attr -z --stdin filter")
        })?,
    );

    Ok(lfs_tracked_paths)
}

fn git_check_attr_filter(
    worktree_root: &Path,
    tracked_paths: &[u8],
) -> CliResult<std::process::Output> {
    let mut child = ProcessCommand::new("git")
        .args(["check-attr", "-z", "--stdin", "filter"])
        .current_dir(worktree_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CliError::Io {
            context: "failed to start git check-attr -z --stdin filter".to_owned(),
            source,
        })?;

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let tracked_paths = tracked_paths.to_owned();
    let stdin_writer = std::thread::spawn(move || {
        let write_result = stdin.write_all(&tracked_paths);
        drop(stdin);

        write_result
    });

    let output = child.wait_with_output().map_err(|source| CliError::Io {
        context: "failed to wait for git check-attr -z --stdin filter".to_owned(),
        source,
    })?;

    let write_result = stdin_writer.join().map_err(|_| CliError::Io {
        context: "git check-attr input writer panicked".to_owned(),
        source: io::Error::other("git check-attr input writer panicked"),
    })?;

    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git check-attr -z --stdin filter".to_owned(),
            status: process_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    write_result.map_err(|source| CliError::Io {
        context: "failed to write git check-attr path input".to_owned(),
        source,
    })?;

    Ok(output)
}

fn git_lfs_objects_dir(repository: &GitRepository) -> CliResult<PathBuf> {
    let git_common_dir = repository.git_common_dir_path()?;
    let storage_dir = match configured_git_lfs_storage_dir(&repository.worktree_root)? {
        Some(storage_dir) if storage_dir.is_absolute() => storage_dir,
        Some(storage_dir) => git_common_dir.join(storage_dir),
        None => git_common_dir.join("lfs"),
    };

    Ok(storage_dir.join("objects"))
}

fn configured_git_lfs_storage_dir(worktree_root: &Path) -> CliResult<Option<PathBuf>> {
    let output = ProcessCommand::new("git")
        .args(["config", "--get", "lfs.storage"])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git config --get lfs.storage".to_owned(),
            source,
        })?;

    if output.status.success() {
        let storage =
            String::from_utf8(output.stdout).map_err(|_| CliError::ExternalCommandOutput {
                command: "git config --get lfs.storage".to_owned(),
                message: SanitizedMessage::new("git returned non-UTF-8 lfs.storage output"),
            })?;
        let storage = storage.trim_end();

        Ok((!storage.is_empty()).then(|| PathBuf::from(storage)))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(CliError::ExternalCommand {
            command: "git config --get lfs.storage".to_owned(),
            status: process_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        })
    }
}

fn cli_git_path_output_error(error: GitPathOutputError, command: &str) -> CliError {
    let message = match error {
        GitPathOutputError::MalformedAttributeOutput => "git returned malformed attribute output",
        #[cfg(not(unix))]
        GitPathOutputError::NonUtf8Path => "git returned non-UTF-8 path output",
        GitPathOutputError::PathOutsideWorktree => "git returned a path outside the worktree",
    };
    CliError::ExternalCommandOutput {
        command: command.to_owned(),
        message: SanitizedMessage::new(message),
    }
}

fn read_current_checkout_pointer_candidate(path: &Path) -> CliResult<Option<LfsPointer>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CliError::Io {
                context: format!("failed to inspect checkout path {}", path.display()),
                source,
            });
        }
    };
    if !metadata.is_file() || metadata.len() >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(None);
    }

    let contents = fs::read(path).map_err(|source| CliError::Io {
        context: format!("failed to read checkout path {}", path.display()),
        source,
    })?;
    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents).ok())
}

// `contained_path` must come from `contained_worktree_file_path`, which
// canonicalizes its parent and rejects symlinks or traversal outside the
// worktree before Git sees the repository-relative path.
fn indexed_lfs_object_for_dehydration(
    worktree_root: &Path,
    contained_path: &Path,
) -> CliResult<LfsObject> {
    let relative_path =
        dehydration_relative_path_from_contained_file(worktree_root, contained_path)?;
    require_lfs_filter(worktree_root, &relative_path, contained_path)?;
    let blob_oid = index_blob_oid(worktree_root, &relative_path, contained_path)?;
    let pointer = read_index_lfs_pointer(worktree_root, &blob_oid, contained_path)?;

    Ok(pointer.object)
}

fn dehydration_relative_path_from_contained_file(
    worktree_root: &Path,
    contained_path: &Path,
) -> CliResult<PathBuf> {
    let root = dunce::canonicalize(worktree_root).map_err(|source| CliError::Io {
        context: format!(
            "failed to resolve Git worktree root {}",
            worktree_root.display()
        ),
        source,
    })?;
    contained_path
        .strip_prefix(&root)
        .map(Path::to_path_buf)
        .map_err(|_| CliError::InvalidArguments {
            message: format!(
                "dehydration path must be contained in the current Git worktree: {}",
                contained_path.display()
            ),
        })
}

fn contained_worktree_file_path(
    worktree_root: &Path,
    path: &Path,
    operation: &'static str,
) -> CliResult<PathBuf> {
    let root = dunce::canonicalize(worktree_root).map_err(|source| CliError::Io {
        context: format!(
            "failed to resolve Git worktree root {}",
            worktree_root.display()
        ),
        source,
    })?;
    let parent = path.parent().ok_or_else(|| CliError::InvalidArguments {
        message: format!(
            "{operation} path must be contained in the current Git worktree: {}",
            path.display()
        ),
    })?;
    let parent = dunce::canonicalize(parent).map_err(|source| CliError::Io {
        context: format!("failed to resolve {operation} path {}", path.display()),
        source,
    })?;
    let relative_parent = parent
        .strip_prefix(&root)
        .map_err(|_| CliError::InvalidArguments {
            message: format!(
                "{operation} path must be contained in the current Git worktree: {}",
                path.display()
            ),
        })?;
    let file_name = path.file_name().ok_or_else(|| CliError::InvalidArguments {
        message: format!("{operation} path is not a file: {}", path.display()),
    })?;
    let path = root.join(relative_parent).join(file_name);
    let metadata = fs::symlink_metadata(&path).map_err(|source| CliError::Io {
        context: format!("failed to inspect {operation} path {}", path.display()),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CliError::InvalidArguments {
            message: format!(
                "{operation} path must be a regular file and not a symbolic link: {}",
                path.display()
            ),
        });
    }

    Ok(path)
}

fn require_lfs_filter(
    worktree_root: &Path,
    relative_path: &Path,
    display_path: &Path,
) -> CliResult<()> {
    let output = ProcessCommand::new("git")
        .args(["check-attr", "-z", "filter", "--"])
        .arg(relative_path)
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git check-attr -z filter".to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git check-attr -z filter -- <path>".to_owned(),
            status: process_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    let mut fields = output.stdout.split(|byte| *byte == b'\0');
    let returned_path = fields.next();
    let attribute = fields.next();
    let value = fields.next();
    let terminator = fields.next();
    if returned_path.is_none()
        || attribute != Some(&b"filter"[..])
        || value != Some(&b"lfs"[..])
        || terminator != Some(&[][..])
        || fields.next().is_some()
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "dehydration path must be tracked with filter=lfs: {}",
                display_path.display()
            ),
        });
    }

    Ok(())
}

fn index_blob_oid(
    worktree_root: &Path,
    relative_path: &Path,
    display_path: &Path,
) -> CliResult<String> {
    let output = ProcessCommand::new("git")
        .args(["--literal-pathspecs", "ls-files", "--stage", "-z", "--"])
        .arg(relative_path)
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git ls-files --stage -z".to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git --literal-pathspecs ls-files --stage -z -- <path>".to_owned(),
            status: process_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    let records = output
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>();
    let [record] = records.as_slice() else {
        return Err(CliError::InvalidArguments {
            message: format!(
                "dehydration path must have one tracked index entry: {}",
                display_path.display()
            ),
        });
    };
    let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
        return Err(index_entry_parse_error());
    };
    let metadata = &record[..separator];
    let fields = metadata
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let [_mode, oid, stage] = fields.as_slice() else {
        return Err(index_entry_parse_error());
    };
    if *stage != b"0" {
        return Err(CliError::InvalidArguments {
            message: format!(
                "dehydration path has an unmerged index entry: {}",
                display_path.display()
            ),
        });
    }

    std::str::from_utf8(oid)
        .map(str::to_owned)
        .map_err(|_| index_entry_parse_error())
}

fn index_entry_parse_error() -> CliError {
    CliError::ExternalCommandOutput {
        command: "git --literal-pathspecs ls-files --stage -z -- <path>".to_owned(),
        message: SanitizedMessage::new("git returned malformed index metadata"),
    }
}

fn read_index_lfs_pointer(
    worktree_root: &Path,
    blob_oid: &str,
    display_path: &Path,
) -> CliResult<LfsPointer> {
    let size_output = ProcessCommand::new("git")
        .args(["cat-file", "-s", blob_oid])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git cat-file -s".to_owned(),
            source,
        })?;
    if !size_output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git cat-file -s <index-object>".to_owned(),
            status: process_status_text(size_output.status),
            stderr: sanitized_external_stderr(&size_output.stderr),
        });
    }
    let size = std::str::from_utf8(&size_output.stdout)
        .ok()
        .and_then(|size| size.trim().parse::<u64>().ok())
        .ok_or_else(|| CliError::ExternalCommandOutput {
            command: "git cat-file -s <index-object>".to_owned(),
            message: SanitizedMessage::new("git returned an invalid index object size"),
        })?;
    if size >= LFS_POINTER_SIZE_CUTOFF {
        return Err(invalid_index_pointer_error(display_path));
    }

    let pointer_output = ProcessCommand::new("git")
        .args(["cat-file", "blob", blob_oid])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git cat-file blob".to_owned(),
            source,
        })?;
    if !pointer_output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git cat-file blob <index-object>".to_owned(),
            status: process_status_text(pointer_output.status),
            stderr: sanitized_external_stderr(&pointer_output.stderr),
        });
    }
    let contents = std::str::from_utf8(&pointer_output.stdout)
        .map_err(|_| invalid_index_pointer_error(display_path))?;

    LfsPointer::parse(contents).map_err(|_| invalid_index_pointer_error(display_path))
}

fn invalid_index_pointer_error(path: &Path) -> CliError {
    CliError::InvalidArguments {
        message: format!(
            "dehydration path must have a valid Git LFS pointer in the index: {}",
            path.display()
        ),
    }
}

fn publish_dehydrated_object_to_git_lfs(
    layout: &LocalCacheLayout,
    git_lfs_objects_dir: &Path,
    dehydration: &LocalCacheDehydration,
) -> CliResult<()> {
    if dehydration.status == LocalCacheDehydrationStatus::AlreadyDehydrated
        && !dehydration.cache_path.is_file()
    {
        return Ok(());
    }

    let oid = dehydration.object.oid.as_hex();
    let destination = git_lfs_objects_dir
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid);
    layout
        .materialize_object(&dehydration.object, destination)
        .map_err(local_cache_cli_error)?;

    Ok(())
}

fn write_hydrate_result<W>(
    output: &mut W,
    materialization: &LocalCacheMaterialization,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "hydrated {} sha256:{} ({} bytes) {}",
        materialization.destination_path.display(),
        materialization.object.oid,
        materialization.object.size,
        materialization_status_label(materialization.status)
    )
}

fn write_dehydrate_result<W>(output: &mut W, dehydration: &LocalCacheDehydration) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "dehydrated {} sha256:{} ({} bytes) {}",
        dehydration.pointer_path.display(),
        dehydration.object.oid,
        dehydration.object.size,
        dehydration_status_label(dehydration.status)
    )
}

fn write_pull_result<W>(
    output: &mut W,
    ingest: &LocalCacheIngest,
    materialization: &LocalCacheMaterialization,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "pulled {} sha256:{} ({} bytes) {} {}",
        materialization.destination_path.display(),
        materialization.object.oid,
        materialization.object.size,
        ingest_status_label(ingest.status),
        materialization_status_label(materialization.status)
    )
}

fn write_gc_result<W>(
    output: &mut W,
    cache_root: &Path,
    report: &LocalCacheGarbageCollection,
) -> io::Result<()>
where
    W: Write,
{
    let action = if report.dry_run {
        "would remove"
    } else {
        "removed"
    };

    writeln!(output, "lfscloud gc")?;
    writeln!(output, "  cache: {}", cache_root.display())?;
    writeln!(
        output,
        "  worktrees: {} active, {} unavailable, {} {}",
        report.active_worktree_count,
        report.unavailable_worktrees.len(),
        report.pruned_worktrees.len(),
        if report.dry_run {
            "would prune"
        } else {
            "pruned"
        }
    )?;
    writeln!(
        output,
        "  objects: {} retained, {} protected, {} {}, {} skipped",
        report.retained_objects.len(),
        report.protected_objects.len(),
        report.unreferenced_objects.len(),
        action,
        report.skipped_cache_paths.len()
    )?;

    for object in &report.unreferenced_objects {
        write_gc_object(output, action, object)?;
    }
    for object in &report.protected_objects {
        write_gc_object(output, "protected while worktree unavailable", object)?;
    }
    for registration in &report.unavailable_worktrees {
        writeln!(
            output,
            "unavailable worktree {} ({})",
            registration.worktree_root.display(),
            registration.repository_id
        )?;
    }
    for registration in &report.pruned_worktrees {
        let action = if report.dry_run {
            "would prune"
        } else {
            "pruned"
        };
        writeln!(
            output,
            "{action} worktree {} ({})",
            registration.worktree_root.display(),
            registration.repository_id
        )?;
    }
    for path in &report.skipped_cache_paths {
        writeln!(output, "skipped {}", path.display())?;
    }

    Ok(())
}

fn write_gc_object<W>(
    output: &mut W,
    action: &str,
    object: &LocalCacheGarbageCollectionObject,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "{action} {} sha256:{} ({} bytes)",
        object.path.display(),
        object.oid,
        object.size_bytes
    )
}

fn materialization_status_label(status: LocalCacheMaterializationStatus) -> &'static str {
    match status {
        LocalCacheMaterializationStatus::AlreadyMaterialized => "already-materialized",
        LocalCacheMaterializationStatus::CopyOnWriteCloned => "copy-on-write-cloned",
        LocalCacheMaterializationStatus::Copied => "copied",
    }
}

fn ingest_status_label(status: LocalCacheIngestStatus) -> &'static str {
    match status {
        LocalCacheIngestStatus::AlreadyCached => "already-cached",
        LocalCacheIngestStatus::Copied => "cached",
    }
}

fn dehydration_status_label(status: LocalCacheDehydrationStatus) -> &'static str {
    match status {
        LocalCacheDehydrationStatus::AlreadyDehydrated => "already-dehydrated",
        LocalCacheDehydrationStatus::ReplacedWithPointer => "replaced-with-pointer",
        LocalCacheDehydrationStatus::CachedAndReplacedWithPointer => {
            "cached-and-replaced-with-pointer"
        }
    }
}

fn local_cache_cli_error(error: crate::LocalCacheError) -> CliError {
    CliError::LocalCache { source: error }
}

#[derive(Debug, Default)]
struct StatusReport {
    checks: Vec<StatusCheck>,
}

impl StatusReport {
    fn new() -> Self {
        Self::default()
    }

    fn ok(&mut self, name: &'static str, message: impl Into<String>) {
        self.push(StatusLevel::Ok, name, message);
    }

    fn warning(&mut self, name: &'static str, message: impl Into<String>) {
        self.push(StatusLevel::Warning, name, message);
    }

    fn error(&mut self, name: &'static str, message: impl Into<String>) {
        self.push(StatusLevel::Error, name, message);
    }

    fn push(&mut self, level: StatusLevel, name: &'static str, message: impl Into<String>) {
        self.checks.push(StatusCheck {
            level,
            name,
            message: message.into(),
        });
    }

    fn has_errors(&self) -> bool {
        self.checks
            .iter()
            .any(|check| check.level == StatusLevel::Error)
    }

    fn write<W>(&self, output: &mut W) -> io::Result<()>
    where
        W: Write,
    {
        writeln!(output, "lfscloud status")?;
        for check in &self.checks {
            writeln!(
                output,
                "  {:<10} {:<7} {}",
                check.name,
                check.level.label(),
                check.message
            )?;
        }

        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StatusLevel {
    Ok,
    Warning,
    Error,
}

impl StatusLevel {
    fn label(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

#[derive(Debug)]
struct StatusCheck {
    level: StatusLevel,
    name: &'static str,
    message: String,
}

fn report_cache_status(report: &mut StatusReport, cache_root: Option<PathBuf>) {
    let layout = match cache_root {
        Some(cache_root) => LocalCacheLayout::new(cache_root),
        None => match default_cache_home_dir() {
            Some(home_dir) => LocalCacheLayout::from_home_dir(home_dir),
            None => {
                report.error("cache", default_cache_root_error().to_string());
                return;
            }
        },
    };
    let root = layout.root();
    let objects_dir = layout.objects_dir();

    if objects_dir.is_dir() {
        report.ok(
            "cache",
            format!("objects directory exists at {}", objects_dir.display()),
        );
    } else if root.exists() && !root.is_dir() {
        report.error(
            "cache",
            format!("cache root is not a directory: {}", root.display()),
        );
    } else if objects_dir.exists() {
        report.error(
            "cache",
            format!("objects path is not a directory: {}", objects_dir.display()),
        );
    } else {
        report.warning(
            "cache",
            format!(
                "objects directory will be created on first ingest at {}",
                objects_dir.display()
            ),
        );
    }
}

fn probe_server_reachable(server_url: &str) -> CliResult<()> {
    // Callers validate the transport policy while building the repository
    // route; this helper performs only the lower-level TCP reachability probe.
    let url = crate::init::validate_server_url(server_url, true)?;
    let host = url.host_str().ok_or_else(|| CliError::InvalidArguments {
        message: "server URL must include a host".to_owned(),
    })?;
    let port = url
        .port_or_known_default()
        .ok_or_else(|| CliError::InvalidArguments {
            message: "server URL must include a port or use a known scheme".to_owned(),
        })?;
    let addresses = resolve_socket_addresses_with_timeout(host.to_owned(), port)?;

    let mut last_error = None;
    let connect_deadline = Instant::now() + STATUS_SERVER_CONNECT_TIMEOUT;
    for address in addresses {
        let remaining = connect_deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(_) => return Ok(()),
            Err(error) => last_error = Some(error),
        }
    }

    Err(CliError::Io {
        context: format!("failed to connect to {host}:{port}"),
        source: last_error.unwrap_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "no socket addresses resolved")
        }),
    })
}

async fn probe_authenticated_migration_target(
    lfs_url: &str,
    allow_insecure_http: bool,
    token: &LfsSessionToken,
) -> CliResult<()> {
    let mut batch_url = crate::init::validate_server_url(lfs_url, allow_insecure_http)?;
    append_url_path_segments(&mut batch_url, "objects/batch")?;

    let client = redirect_free_http_client("failed to create migration target probe client")?;
    let response = client
        .post(batch_url)
        .bearer_auth(token.as_str())
        .header("Accept", "application/vnd.git-lfs+json")
        .header("Content-Type", "application/vnd.git-lfs+json")
        .json(&serde_json::json!({
            "operation": "upload",
            "transfers": ["basic"],
            "objects": [],
        }))
        .timeout(MIGRATION_TARGET_PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|source| CliError::Io {
            context: "failed to authenticate the migration target repository".to_owned(),
            source: io::Error::other(source),
        })?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(CliError::ExternalCommandOutput {
            command: "migration target repository authentication".to_owned(),
            message: SanitizedMessage::new(format!(
                "server returned HTTP status {}",
                response.status().as_u16()
            )),
        })
    }
}

fn resolve_socket_addresses_with_timeout(host: String, port: u16) -> CliResult<Vec<SocketAddr>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let thread_host = host.clone();
    std::thread::Builder::new()
        .name("lfscloud-status-resolver".to_owned())
        .spawn(move || {
            let result = (thread_host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect::<Vec<_>>())
                .map_err(|source| CliError::Io {
                    context: format!("failed to resolve {thread_host}:{port}"),
                    source,
                });
            let _ = sender.send(result);
        })
        .map_err(|source| CliError::Io {
            context: format!("failed to start resolver for {host}:{port}"),
            source,
        })?;

    receiver
        .recv_timeout(STATUS_SERVER_CONNECT_TIMEOUT)
        .map_err(|_| CliError::Io {
            context: format!("timed out resolving {host}:{port}"),
            source: io::Error::new(io::ErrorKind::TimedOut, "DNS resolution timed out"),
        })?
}

fn validate_status_storage(storage: &StorageProviderConfig) -> CliResult<()> {
    match storage {
        StorageProviderConfig::GoogleDrive(storage) => {
            validate_google_drive_status_storage(storage)
        }
    }
}

fn validate_google_drive_status_storage(storage: &GoogleDriveStorageConfig) -> CliResult<()> {
    GoogleDriveGcloudTokenProvider::new()
        .validate_local_readiness(&storage.id, &storage.credentials)
        .map_err(|_| CliError::InvalidArguments {
            message: format!(
                "Google Drive credential for {} is not usable; check the configured gcloud ADC credentials directory",
                storage.id
            ),
        })
}

fn auth_url_for_server(server_url: &str, route_path: &str) -> CliResult<String> {
    // Login and logout callers obtain this base from `LfsInitRoute`, which has
    // already enforced the CLI's insecure-HTTP opt-in. Revalidation accepts
    // HTTP here so loopback and explicitly opted-in LAN routes remain usable.
    let mut auth_url = crate::init::validate_server_url(server_url, true)?;
    append_url_path_segments(&mut auth_url, route_path)?;

    Ok(auth_url.to_string())
}

fn append_url_path_segments(url: &mut Url, route_path: &str) -> CliResult<()> {
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| CliError::InvalidArguments {
            message: "URL cannot be used as a route base".to_owned(),
        })?;
    segments.extend(route_path.split('/').filter(|segment| !segment.is_empty()));

    Ok(())
}

fn redirect_free_http_client(context: &'static str) -> CliResult<Client> {
    // Token-bearing requests must never forward credentials to a redirect
    // target, even when that target shares the original host.
    Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|source| CliError::Io {
            context: context.to_owned(),
            source: io::Error::other(source),
        })
}

fn block_on_reqwest<T>(
    future: impl Future<Output = Result<T, reqwest::Error>>,
    context: &'static str,
) -> CliResult<T> {
    // The synchronous CLI handlers run inside the process Tokio runtime; move
    // their reqwest futures through its handle without nesting another runtime.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future)).map_err(
        |source| CliError::Io {
            context: context.to_owned(),
            source: io::Error::other(source),
        },
    )
}

fn process_status_text(status: std::process::ExitStatus) -> String {
    status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "terminated by signal".to_owned())
}

fn sanitized_external_stderr(stderr: &[u8]) -> SanitizedMessage {
    const MAX_EXTERNAL_STDERR_LEN: usize = 1024;

    let mut message = String::from_utf8_lossy(stderr).into_owned();
    message = message.replace(['\r', '\n'], " ");
    if message.len() > MAX_EXTERNAL_STDERR_LEN {
        let boundary = (0..=MAX_EXTERNAL_STDERR_LEN)
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

fn sanitized_external_failure_output(stderr: &[u8], stdout: &[u8]) -> SanitizedMessage {
    if stdout.is_empty() {
        return sanitized_external_stderr(stderr);
    }

    let mut combined = Vec::with_capacity(stderr.len() + stdout.len() + 9);
    combined.extend_from_slice(stderr);
    if !stderr.is_empty() {
        combined.push(b'\n');
    }
    combined.extend_from_slice(b"stdout: ");
    combined.extend_from_slice(stdout);

    sanitized_external_stderr(&combined)
}

fn output_error(source: io::Error) -> CliError {
    CliError::Io {
        context: "failed to write command output".to_owned(),
        source,
    }
}

impl InitCommand {
    fn target(&self) -> GitLfsConfigTarget {
        if self.local {
            GitLfsConfigTarget::LocalRepository
        } else {
            GitLfsConfigTarget::WorktreeFile
        }
    }
}

fn write_init_change<W>(output: &mut W, change: &GitLfsConfigChange) -> io::Result<()>
where
    W: Write,
{
    writeln!(output, "configured {}", change.target.label())?;
    writeln!(output, "  path: {}", change.path.display())?;
    match change.previous_url.as_deref() {
        Some(previous_url) if previous_url == change.new_url => {
            writeln!(
                output,
                "  lfs.url unchanged: {}",
                redacted_url_for_display(&change.new_url)
            )?;
        }
        Some(previous_url) => {
            writeln!(
                output,
                "  - lfs.url: {}",
                redacted_url_for_display(previous_url)
            )?;
            writeln!(
                output,
                "  + lfs.url: {}",
                redacted_url_for_display(&change.new_url)
            )?;
        }
        None => {
            writeln!(output, "  - lfs.url: <unset>")?;
            writeln!(
                output,
                "  + lfs.url: {}",
                redacted_url_for_display(&change.new_url)
            )?;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(unix)]
    use std::time::Instant;
    use std::{
        collections::BTreeMap,
        fs, io,
        path::{Path, PathBuf},
        process::Command as ProcessCommand,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Duration,
    };

    use axum::{
        Json, Router,
        body::Bytes,
        extract::{Path as AxumPath, State},
        http::{
            HeaderMap, HeaderValue, StatusCode, Uri,
            header::{CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, LOCATION, RANGE},
        },
        response::{IntoResponse, Response},
        routing::{get, post, put},
    };
    use clap::{CommandFactory, Parser};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    #[cfg(unix)]
    use super::run_bounded_child_command;
    use super::{
        Cli, DehydrateCommand, GcCommand, HydrateCommand, InitCommand, LoginCommand, LoginTerminal,
        LogoutCommand, MAX_LOGIN_TOKEN_INPUT_BYTES, MigrateCommand, MigrationGoogleDriveStorage,
        PullCommand, SessionRevocationStatus, StatusCommand, current_checkout_lfs_pointer_files,
        current_checkout_lfs_pointer_scan, dispatch, execute_migration_with_storage,
        github_personal_access_token_login_url_for_server, is_git_worktree_discovery_error,
        prepare_migration_execution, probe_authenticated_migration_target, probe_server_reachable,
        read_bounded_login_token, read_hidden_login_token, run_dehydrate_from_dir, run_gc_from_dir,
        run_hydrate_from_dir, run_init_from_dir, run_login_from_dir, run_logout_from_dir,
        run_migrate_from_dir, run_pull_from_dir, run_status_from_dir,
        session_revocation_url_for_server, tracing_config, validate_status_storage,
        write_init_change,
    };
    use crate::google_drive::{
        GoogleDriveAccessToken, GoogleDriveAccessTokenCache, GoogleDriveAccessTokenSource,
    };
    use crate::{
        CliError, DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, GitCredentialApproval,
        GitCredentialRejection, GitLfsConfigChange, GitLfsConfigTarget, GitRepository,
        GoogleDriveObjectStore, GoogleDriveStorageConfig, LfsObject, LfsObjectSize, LfsOid,
        LfsPointer, LfsSessionToken, LocalCacheError, LocalCacheLayout,
        LocalCacheWorktreeRegistration, MetadataDatabase, ProviderFuture, RepositoryMapping,
        SanitizedMessage, ServeOptions, StorageDeleteOutcome, StorageError, StorageProvider,
        StorageProviderConfig, StorageResult, StoredObject,
    };

    // The shared contract expects this parent-level fixture name in both its
    // integration-test and unit-test inclusion contexts.
    use object_for_bytes as lfs_object_for_bytes;

    mod storage_provider_contract {
        include!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/support/storage_provider_contract.rs"
        ));
    }

    use storage_provider_contract::assert_storage_provider_contract;

    struct RecordingMigrationStorage {
        provider_id: String,
        objects: Mutex<BTreeMap<LfsObject, Vec<u8>>>,
        failing_object: Option<LfsObject>,
    }

    impl RecordingMigrationStorage {
        fn new(provider_id: impl Into<String>) -> Self {
            Self {
                provider_id: provider_id.into(),
                objects: Mutex::new(BTreeMap::new()),
                failing_object: None,
            }
        }

        fn failing(mut self, object: LfsObject) -> Self {
            self.failing_object = Some(object);
            self
        }

        fn object_bytes(&self, object: &LfsObject) -> Option<Vec<u8>> {
            self.objects
                .lock()
                .expect("recording migration storage lock should not poison")
                .get(object)
                .cloned()
        }
    }

    impl StorageProvider for RecordingMigrationStorage {
        fn provider_id(&self) -> &str {
            &self.provider_id
        }

        fn object_exists<'a>(
            &'a self,
            _repository_namespace: &'a str,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<bool>> {
            Box::pin(async move {
                Ok(self
                    .objects
                    .lock()
                    .expect("recording migration storage lock should not poison")
                    .contains_key(object))
            })
        }

        fn upload_object<'a>(
            &'a self,
            repository_namespace: &'a str,
            object: &'a LfsObject,
            source: &'a Path,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                if self.failing_object.as_ref() == Some(object) {
                    return Err(StorageError::Retryable {
                        provider: self.provider_id.clone(),
                        message: "simulated migration upload failure".to_owned(),
                    });
                }
                let bytes = fs::read(source).map_err(|error| StorageError::StagedFileRead {
                    provider: self.provider_id.clone(),
                    path: source.to_path_buf(),
                    source: error,
                })?;
                self.objects
                    .lock()
                    .expect("recording migration storage lock should not poison")
                    .insert(object.clone(), bytes);
                Ok(StoredObject::new(
                    &self.provider_id,
                    repository_namespace,
                    object.clone(),
                    format!("recorded-{}", object.oid.as_hex()),
                ))
            })
        }

        fn download_object<'a>(
            &'a self,
            _repository_namespace: &'a str,
            object: &'a LfsObject,
            _destination: &'a Path,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                Err(StorageError::ObjectNotFound {
                    provider: self.provider_id.clone(),
                    oid: object.oid.as_hex().to_owned(),
                    size: object.size.bytes(),
                })
            })
        }

        fn delete_or_mark_object<'a>(
            &'a self,
            _repository_namespace: &'a str,
            _object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
            Box::pin(async {
                Ok(StorageDeleteOutcome::Retained {
                    reason: "test storage retains migration objects".to_owned(),
                })
            })
        }
    }

    struct FixedDriveTokenSource;

    impl GoogleDriveAccessTokenSource for FixedDriveTokenSource {
        fn access_token<'a>(
            &'a self,
            _storage: &'a GoogleDriveStorageConfig,
        ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
            Box::pin(async { Ok(GoogleDriveAccessToken::for_test("contract-access-token")) })
        }
    }

    struct CountingDriveTokenSource {
        calls: AtomicUsize,
    }

    impl GoogleDriveAccessTokenSource for CountingDriveTokenSource {
        fn access_token<'a>(
            &'a self,
            _storage: &'a GoogleDriveStorageConfig,
        ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(GoogleDriveAccessToken::for_test(format!(
                    "contract-access-token-{call}"
                )))
            })
        }
    }

    #[derive(Clone)]
    struct DriveContractObject {
        backend_id: String,
        repository_namespace: String,
        oid: String,
        size: u64,
        bytes: Vec<u8>,
    }

    struct DriveStorageContractServer {
        base_url: String,
        state: Arc<DriveStorageContractState>,
        task: tokio::task::JoinHandle<()>,
    }

    impl DriveStorageContractServer {
        async fn start() -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("Drive contract server should bind");
            let address = listener
                .local_addr()
                .expect("Drive contract server address should be available");
            let base_url = format!("http://{address}");
            let state = Arc::new(DriveStorageContractState {
                base_url: base_url.clone(),
                objects: Mutex::new(BTreeMap::new()),
                pending_uploads: Mutex::new(BTreeMap::new()),
                next_upload_session_id: AtomicUsize::new(1),
                next_backend_id: AtomicUsize::new(1),
                upload_count: AtomicUsize::new(0),
            });
            let app = Router::new()
                .route("/drive/v3/files", get(drive_contract_list))
                .route(
                    "/upload/drive/v3/files",
                    post(drive_contract_initiate_upload),
                )
                .route(
                    "/upload_session/{session_id}",
                    put(drive_contract_complete_upload),
                )
                .route("/drive/v3/files/{file_id}", get(drive_contract_download))
                .with_state(state.clone());
            let task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("Drive contract server should run");
            });

            Self {
                base_url,
                state,
                task,
            }
        }

        fn upload_count(&self) -> usize {
            self.state.upload_count.load(Ordering::SeqCst)
        }
    }

    impl Drop for DriveStorageContractServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    struct DriveStorageContractState {
        base_url: String,
        objects: Mutex<BTreeMap<(String, String, u64), DriveContractObject>>,
        pending_uploads: Mutex<BTreeMap<String, DriveContractUploadSession>>,
        next_upload_session_id: AtomicUsize,
        next_backend_id: AtomicUsize,
        upload_count: AtomicUsize,
    }

    struct DriveContractUploadSession {
        metadata: serde_json::Value,
        bytes: Vec<u8>,
    }

    async fn drive_contract_list(
        State(state): State<Arc<DriveStorageContractState>>,
        uri: Uri,
    ) -> Response {
        let query = drive_contract_query(&uri);
        if query.contains("lfsCloudFolderKind") {
            let shard =
                drive_contract_property(&query, "lfsCloudShard").unwrap_or_else(|| "00".to_owned());
            return Json(serde_json::json!({
                "files": [{
                    "id": format!("drive-shard-{shard}"),
                    "name": format!("lfscloud-sha256-{shard}"),
                    "mimeType": "application/vnd.google-apps.folder",
                    "parents": ["drive-root"],
                    "trashed": false,
                    "appProperties": {
                        "lfsCloudFolderKind": "objectShard",
                        "lfsCloudShard": shard
                    }
                }]
            }))
            .into_response();
        }

        let repository_namespace = drive_contract_property(&query, "lfsCloudRepoNamespace")
            .expect("Drive contract object query must include lfsCloudRepoNamespace");
        let oid = drive_contract_property(&query, "lfsCloudOid")
            .expect("Drive contract object query must include lfsCloudOid");
        let size = drive_contract_property(&query, "lfsCloudSize")
            .expect("Drive contract object query must include lfsCloudSize")
            .parse::<u64>()
            .expect("Drive contract object query size must parse");
        let objects = state
            .objects
            .lock()
            .expect("Drive contract objects lock should not poison");
        let files = objects
            .get(&(repository_namespace, oid, size))
            .map_or_else(Vec::new, |object| vec![drive_contract_object_json(object)]);
        Json(serde_json::json!({ "files": files })).into_response()
    }

    async fn drive_contract_initiate_upload(
        State(state): State<Arc<DriveStorageContractState>>,
        body: Bytes,
    ) -> Response {
        let metadata: serde_json::Value =
            serde_json::from_slice(&body).expect("Drive contract upload metadata should be JSON");
        let session_id = format!(
            "session-{}",
            state.next_upload_session_id.fetch_add(1, Ordering::SeqCst)
        );
        state
            .pending_uploads
            .lock()
            .expect("Drive contract pending uploads lock should not poison")
            .insert(
                session_id.clone(),
                DriveContractUploadSession {
                    metadata,
                    bytes: Vec::new(),
                },
            );

        let mut response = StatusCode::OK.into_response();
        response.headers_mut().insert(
            LOCATION,
            HeaderValue::from_str(&format!("{}/upload_session/{session_id}", state.base_url))
                .expect("Drive contract upload location should be a valid header"),
        );
        response
    }

    async fn drive_contract_complete_upload(
        AxumPath(session_id): AxumPath<String>,
        State(state): State<Arc<DriveStorageContractState>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let content_range = headers
            .get(CONTENT_RANGE)
            .and_then(|value| value.to_str().ok())
            .expect("Drive contract upload must include a valid Content-Range");
        let mut pending_uploads = state
            .pending_uploads
            .lock()
            .expect("Drive contract pending uploads lock should not poison");
        let session = pending_uploads
            .get_mut(&session_id)
            .expect("Drive contract upload session should have metadata");
        let properties = &session.metadata["appProperties"];
        let repository_namespace = properties["lfsCloudRepoNamespace"]
            .as_str()
            .expect("Drive contract namespace should be present")
            .to_owned();
        let oid = properties["lfsCloudOid"]
            .as_str()
            .expect("Drive contract OID should be present")
            .to_owned();
        let size = properties["lfsCloudSize"]
            .as_str()
            .expect("Drive contract size should be present")
            .parse::<u64>()
            .expect("Drive contract size should parse");
        if content_range == format!("bytes */{size}") {
            return drive_contract_incomplete_upload_response(session.bytes.len());
        }
        let (start, end, total) = drive_contract_upload_range(content_range);
        assert_eq!(
            total, size,
            "Drive contract upload total must match metadata"
        );
        assert_eq!(
            start,
            u64::try_from(session.bytes.len()).expect("session byte count should fit u64"),
            "Drive contract upload chunks must be contiguous"
        );
        assert_eq!(
            end - start + 1,
            u64::try_from(body.len()).expect("chunk length should fit u64"),
            "Drive contract Content-Range must match the chunk body"
        );
        session.bytes.extend_from_slice(&body);
        let committed_size =
            u64::try_from(session.bytes.len()).expect("session byte count should fit u64");
        assert!(
            committed_size <= size,
            "Drive contract upload must not exceed its declared size"
        );
        if committed_size < size {
            return drive_contract_incomplete_upload_response(session.bytes.len());
        }
        let completed = pending_uploads
            .remove(&session_id)
            .expect("completed Drive contract session should still exist");
        drop(pending_uploads);
        let backend_id = format!(
            "drive-contract-{}",
            state.next_backend_id.fetch_add(1, Ordering::SeqCst)
        );
        let object = DriveContractObject {
            backend_id,
            repository_namespace: repository_namespace.clone(),
            oid: oid.clone(),
            size,
            bytes: completed.bytes,
        };
        state
            .objects
            .lock()
            .expect("Drive contract objects lock should not poison")
            .insert((repository_namespace, oid, size), object.clone());
        state.upload_count.fetch_add(1, Ordering::SeqCst);

        (
            StatusCode::CREATED,
            [(CONTENT_TYPE, "application/json")],
            Json(drive_contract_object_json(&object)),
        )
            .into_response()
    }

    fn drive_contract_upload_range(value: &str) -> (u64, u64, u64) {
        let (range, total) = value
            .strip_prefix("bytes ")
            .and_then(|value| value.split_once('/'))
            .expect("Drive contract Content-Range must use bytes start-end/total");
        let (start, end) = range
            .split_once('-')
            .expect("Drive contract Content-Range must include a byte range");
        (
            start
                .parse::<u64>()
                .expect("Drive contract range start should parse"),
            end.parse::<u64>()
                .expect("Drive contract range end should parse"),
            total
                .parse::<u64>()
                .expect("Drive contract range total should parse"),
        )
    }

    fn drive_contract_incomplete_upload_response(committed_size: usize) -> Response {
        let mut response = StatusCode::from_u16(308)
            .expect("308 should be a valid status")
            .into_response();
        if committed_size > 0 {
            response.headers_mut().insert(
                RANGE,
                HeaderValue::from_str(&format!("bytes=0-{}", committed_size - 1))
                    .expect("Drive contract committed range should be a valid header"),
            );
        }
        response
    }

    async fn drive_contract_download(
        AxumPath(file_id): AxumPath<String>,
        State(state): State<Arc<DriveStorageContractState>>,
        uri: Uri,
    ) -> Response {
        let object = state
            .objects
            .lock()
            .expect("Drive contract objects lock should not poison")
            .values()
            .find(|object| object.backend_id == file_id)
            .cloned();
        let Some(object) = object else {
            return StatusCode::NOT_FOUND.into_response();
        };

        if drive_contract_query_pair(&uri, "alt").as_deref() == Some("media") {
            let mut response = (
                StatusCode::OK,
                [(CONTENT_TYPE, "application/octet-stream")],
                object.bytes,
            )
                .into_response();
            response.headers_mut().insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&object.size.to_string())
                    .expect("Drive contract content length should be a valid header"),
            );
            response
        } else {
            Json(drive_contract_object_json(&object)).into_response()
        }
    }

    fn drive_contract_query(uri: &Uri) -> String {
        drive_contract_query_pair(uri, "q").unwrap_or_default()
    }

    fn drive_contract_query_pair(uri: &Uri, expected_key: &str) -> Option<String> {
        url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
            .find_map(|(key, value)| (key == expected_key).then(|| value.into_owned()))
    }

    fn drive_contract_property(query: &str, key: &str) -> Option<String> {
        let marker = format!("key='{key}' and value='");
        query
            .split_once(&marker)?
            .1
            .split_once('\'')
            .map(|(value, _)| value.to_owned())
    }

    fn drive_contract_object_json(object: &DriveContractObject) -> serde_json::Value {
        let oid_prefix = object
            .oid
            .get(..2)
            .expect("Drive contract OID must contain a two-character shard prefix");
        serde_json::json!({
            "id": object.backend_id,
            "name": format!("sha256-{}-{}.lfs", object.oid, object.size),
            "size": object.size.to_string(),
            "parents": [format!("drive-shard-{oid_prefix}")],
            "trashed": false,
            "appProperties": {
                "lfsCloudVersion": "1",
                "lfsCloudRepoNamespace": object.repository_namespace,
                "lfsCloudOid": object.oid,
                "lfsCloudSize": object.size.to_string()
            }
        })
    }

    #[tokio::test]
    async fn google_drive_object_store_satisfies_shared_storage_contract() {
        let server = DriveStorageContractServer::start().await;
        let store = GoogleDriveObjectStore::with_api_base_url(
            drive_contract_storage_config(),
            "github.com/owner/repo",
            GoogleDriveAccessToken::for_test("contract-access-token"),
            &server.base_url,
        )
        .expect("Drive contract store should build");

        let report = assert_storage_provider_contract(
            &store,
            "github.com/owner/repo",
            "github.com/owner/isolated",
        )
        .await;

        assert!(!report.isolated_object_was_created);
        assert!(matches!(
            report.deletion,
            StorageDeleteOutcome::Retained { .. }
        ));
        assert_eq!(
            server.upload_count(),
            1,
            "verified idempotent re-upload must not create another Drive object"
        );
    }

    #[tokio::test]
    async fn migration_google_drive_storage_satisfies_shared_storage_contract() {
        let server = DriveStorageContractServer::start().await;
        let storage = MigrationGoogleDriveStorage {
            storage: drive_contract_storage_config(),
            repository_namespace: "github.com/owner/repo".to_owned(),
            token_source: Arc::new(FixedDriveTokenSource),
            token_cache: GoogleDriveAccessTokenCache::default(),
            metadata: Arc::new(
                MetadataDatabase::open_in_memory().expect("Drive contract metadata should open"),
            ),
            api_base_url: Some(server.base_url.clone()),
        };

        let report = assert_storage_provider_contract(
            &storage,
            "github.com/owner/repo",
            "github.com/owner/isolated",
        )
        .await;

        assert!(!report.isolated_object_was_created);
        assert!(matches!(
            report.deletion,
            StorageDeleteOutcome::Retained { .. }
        ));
        assert_eq!(
            server.upload_count(),
            1,
            "migration's locked idempotent re-upload must reuse the Drive object"
        );
    }

    #[tokio::test]
    async fn migration_upload_acquires_drive_token_after_upload_lock() {
        let server = DriveStorageContractServer::start().await;
        let source_root = tempfile::tempdir().expect("migration source root should be created");
        let source = source_root.path().join("object.bin");
        let object_bytes = b"migration upload lock token refresh";
        fs::write(&source, object_bytes).expect("migration source should be written");
        let object = lfs_object_for_bytes(object_bytes);
        let repository_namespace = "github.com/owner/repo";
        let storage_config = drive_contract_storage_config();
        let metadata = Arc::new(
            MetadataDatabase::open(source_root.path().join("metadata.sqlite3"))
                .expect("migration metadata should open"),
        );
        let held_lock = metadata
            .acquire_object_upload_lock(
                repository_namespace.to_owned(),
                storage_config.id.clone(),
                object.clone(),
            )
            .await
            .expect("migration upload lock should be acquired")
            .expect("file-backed metadata should return an upload lock");
        let token_source = Arc::new(CountingDriveTokenSource {
            calls: AtomicUsize::new(0),
        });
        let storage = MigrationGoogleDriveStorage {
            storage: storage_config,
            repository_namespace: repository_namespace.to_owned(),
            token_source: token_source.clone(),
            token_cache: GoogleDriveAccessTokenCache::default(),
            metadata,
            api_base_url: Some(server.base_url.clone()),
        };

        let upload = tokio::spawn(async move {
            StorageProvider::upload_object(&storage, repository_namespace, &object, &source).await
        });
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(
            token_source.calls.load(Ordering::SeqCst),
            0,
            "migration must not capture a Drive token before the upload lock is available"
        );

        drop(held_lock);
        tokio::time::timeout(Duration::from_secs(10), upload)
            .await
            .expect("migration upload should complete after the lock is released")
            .expect("migration upload task should join")
            .expect("migration upload should succeed");
        assert_eq!(
            token_source.calls.load(Ordering::SeqCst),
            1,
            "migration should acquire a current Drive token after the upload lock"
        );
    }

    fn drive_contract_storage_config() -> GoogleDriveStorageConfig {
        GoogleDriveStorageConfig {
            id: "drive-user-a".to_owned(),
            credentials: crate::GoogleDriveGcloudCredentialsConfig {
                config_dir: ".gcloud-drive".into(),
                executable: "gcloud".into(),
            },
            root_folder_id: "drive-root".to_owned(),
            display_name: None,
        }
    }

    #[test]
    fn root_command_exposes_shared_global_flags() {
        let command = Cli::command();
        let config = command
            .get_arguments()
            .find(|argument| argument.get_id() == "config")
            .expect("config flag should be registered");
        let log_level = command
            .get_arguments()
            .find(|argument| argument.get_id() == "log_level")
            .expect("log-level flag should be registered");

        assert!(config.is_global_set());
        assert!(log_level.is_global_set());
    }

    #[test]
    fn root_command_requires_a_subcommand() {
        let error = Cli::try_parse_from(["lfscloud"]).expect_err("command should be required");
        let rendered = error.to_string();

        assert!(rendered.contains("Usage: lfscloud"));
        assert!(rendered.contains("Commands:"));
    }

    #[test]
    fn init_command_accepts_required_server_url() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "--config",
            "custom-lfscloud.yml",
            "init",
            "--server",
            "http://127.0.0.1:8080",
            "--allow-insecure-http",
        ])
        .expect("init command should parse");

        let super::Command::Init(command) = cli.command else {
            panic!("init subcommand should parse");
        };

        assert_eq!(cli.config, Some("custom-lfscloud.yml".into()));
        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.allow_insecure_http);
        assert!(!command.local);
    }

    #[test]
    fn init_command_accepts_local_config_option() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "init",
            "--server",
            "http://127.0.0.1:8080",
            "--local",
        ])
        .expect("init command should parse");

        let super::Command::Init(command) = cli.command else {
            panic!("init subcommand should parse");
        };

        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.local);
    }

    #[test]
    fn login_command_accepts_server_url() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "login",
            "--server",
            "http://127.0.0.1:8080",
            "--allow-insecure-http",
        ])
        .expect("login command should parse");

        let super::Command::Login(command) = cli.command else {
            panic!("login subcommand should parse");
        };

        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.allow_insecure_http);
    }

    #[test]
    fn logout_command_accepts_server_url_and_insecure_http_option() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "logout",
            "--server",
            "http://127.0.0.1:8080",
            "--allow-insecure-http",
        ])
        .expect("logout command should parse");

        let super::Command::Logout(command) = cli.command else {
            panic!("logout subcommand should parse");
        };

        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.allow_insecure_http);
    }

    #[test]
    fn status_command_accepts_server_and_cache_root_options() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "--config",
            "lfscloud.test.yml",
            "status",
            "--server",
            "http://127.0.0.1:8080",
            "--allow-insecure-http",
            "--cache-root",
            "/tmp/lfscloud-cache",
        ])
        .expect("status command should parse");

        let super::Command::Status(command) = cli.command else {
            panic!("status subcommand should parse");
        };

        assert_eq!(cli.config, Some("lfscloud.test.yml".into()));
        assert_eq!(command.server, Some("http://127.0.0.1:8080".to_owned()));
        assert!(command.allow_insecure_http);
        assert_eq!(command.cache_root, Some("/tmp/lfscloud-cache".into()));
    }

    #[test]
    fn pull_command_accepts_cache_root_option() {
        let cli = Cli::try_parse_from(["lfscloud", "pull", "--cache-root", "/tmp/lfscloud-cache"])
            .expect("pull command should parse");

        let super::Command::Pull(command) = cli.command else {
            panic!("pull subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfscloud-cache".into()));
    }

    #[test]
    fn hydrate_command_accepts_cache_root_and_paths() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "hydrate",
            "--cache-root",
            "/tmp/lfscloud-cache",
            "asset/model.bin",
            "asset/texture.bin",
        ])
        .expect("hydrate command should parse");

        let super::Command::Hydrate(command) = cli.command else {
            panic!("hydrate subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfscloud-cache".into()));
        assert_eq!(
            command.paths,
            vec![
                PathBuf::from("asset/model.bin"),
                PathBuf::from("asset/texture.bin")
            ]
        );
    }

    #[test]
    fn dehydrate_command_accepts_cache_root_and_paths() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "dehydrate",
            "--cache-root",
            "/tmp/lfscloud-cache",
            "asset/model.bin",
        ])
        .expect("dehydrate command should parse");

        let super::Command::Dehydrate(command) = cli.command else {
            panic!("dehydrate subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfscloud-cache".into()));
        assert_eq!(command.paths, vec![PathBuf::from("asset/model.bin")]);
    }

    #[test]
    fn gc_command_accepts_cache_root_dry_run_and_explicit_prune_options() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "gc",
            "--cache-root",
            "/tmp/lfscloud-cache",
            "--dry-run",
            "--prune-unavailable-worktrees",
        ])
        .expect("gc command should parse");

        let super::Command::Gc(command) = cli.command else {
            panic!("gc subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfscloud-cache".into()));
        assert!(command.dry_run);
        assert!(command.prune_unavailable_worktrees);
    }

    #[test]
    fn migrate_command_accepts_dry_run_scope_and_cache_options() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "--config",
            "lfscloud.test.yml",
            "migrate",
            "--server",
            "http://127.0.0.1:8080",
            "--dry-run",
            "--all-refs",
            "--cache-root",
            "/tmp/lfscloud-cache",
            "--purge-source-lfs",
            "--allow-insecure-http",
            "--source-remote",
            "upstream",
            "--allow-cross-remote",
        ])
        .expect("migrate command should parse");

        let super::Command::Migrate(command) = cli.command else {
            panic!("migrate subcommand should parse");
        };

        assert_eq!(cli.config, Some("lfscloud.test.yml".into()));
        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.allow_insecure_http);
        assert_eq!(command.source_remote, "upstream");
        assert!(command.allow_cross_remote);
        assert!(command.dry_run);
        assert!(command.all_refs);
        assert!(command.purge_source_lfs);
        assert!(command.refs.is_empty());
        assert_eq!(command.cache_root, Some("/tmp/lfscloud-cache".into()));
    }

    #[test]
    fn migrate_command_defaults_source_remote_to_origin() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "migrate",
            "--server",
            "http://127.0.0.1:8080",
            "--dry-run",
        ])
        .expect("migrate command should use the safe source remote default");

        let super::Command::Migrate(command) = cli.command else {
            panic!("migrate subcommand should parse");
        };

        assert_eq!(command.source_remote, "origin");
        assert!(!command.allow_cross_remote);
    }

    #[test]
    fn migrate_command_accepts_execution_and_rejects_conflicting_ref_scopes() {
        let execution = Cli::try_parse_from([
            "lfscloud",
            "migrate",
            "--server",
            "http://127.0.0.1:8080",
            "--all-refs",
            "--purge-source-lfs",
        ])
        .expect("migrate should accept non-dry-run execution");
        let super::Command::Migrate(execution) = execution.command else {
            panic!("migrate subcommand should parse");
        };
        assert!(!execution.dry_run);
        assert!(execution.all_refs);
        assert!(execution.purge_source_lfs);

        let conflicting_scopes = Cli::try_parse_from([
            "lfscloud",
            "migrate",
            "--server",
            "http://127.0.0.1:8080",
            "--dry-run",
            "--ref",
            "main",
            "--all-refs",
        ])
        .expect_err("migrate should reject conflicting ref scopes");
        assert!(conflicting_scopes.to_string().contains("--ref"));
        assert!(conflicting_scopes.to_string().contains("--all-refs"));
    }

    #[test]
    fn source_endpoint_provider_label_uses_host_or_unknown() {
        assert_eq!(
            super::source_endpoint_provider_label(
                "https://lfs.example.com/owner/repo.git/info/lfs"
            ),
            "lfs.example.com"
        );
        assert_eq!(
            super::source_endpoint_provider_label("not a url?token=query-secret"),
            super::SOURCE_PROVIDER_UNKNOWN_LABEL
        );
    }

    #[test]
    fn serve_command_accepts_global_config_before_subcommand() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "--config",
            "custom-lfscloud.yml",
            "serve",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
        ])
        .expect("serve command should parse");

        let super::Command::Serve(command) = cli.command else {
            panic!("serve subcommand should parse");
        };
        let options = command.serve_options(cli.config);

        assert_eq!(
            options,
            ServeOptions::new(
                Some("custom-lfscloud.yml".into()),
                Some("0.0.0.0".to_owned()),
                Some(9000),
            )
        );
    }

    #[test]
    fn serve_command_accepts_global_config_after_subcommand() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "serve",
            "--config",
            "custom-lfscloud.yml",
            "--host",
            "0.0.0.0",
            "--port",
            "9000",
        ])
        .expect("serve command should parse");

        let super::Command::Serve(command) = cli.command else {
            panic!("serve subcommand should parse");
        };
        let options = command.serve_options(cli.config);

        assert_eq!(
            options,
            ServeOptions::new(
                Some("custom-lfscloud.yml".into()),
                Some("0.0.0.0".to_owned()),
                Some(9000),
            )
        );
    }

    #[test]
    fn default_tracing_config_uses_rust_log_env_override() {
        let cli = Cli::try_parse_from(["lfscloud", "serve"]).expect("serve command should parse");
        let config = tracing_config(&cli);

        assert_eq!(config.default_filter, DEFAULT_LOG_FILTER);
        assert_eq!(config.env_filter_var.as_deref(), Some(DEFAULT_LOG_ENV_VAR));
    }

    #[test]
    fn explicit_log_level_overrides_rust_log_env() {
        let cli = Cli::try_parse_from(["lfscloud", "--log-level", "warn,lfscloud=debug", "serve"])
            .expect("serve command should parse");
        let config = tracing_config(&cli);

        assert_eq!(config.default_filter, "warn,lfscloud=debug");
        assert!(config.env_filter_var.is_none());
    }

    #[derive(Debug, Eq, PartialEq)]
    enum Invoked {
        Serve(ServeOptions),
        Init(InitCommand),
        Login(LoginCommand),
        Logout(LogoutCommand),
        Status(StatusCommand, Option<PathBuf>),
        Pull(PullCommand),
        Hydrate(HydrateCommand),
        Dehydrate(DehydrateCommand),
        Gc(GcCommand),
        Migrate(MigrateCommand, Option<PathBuf>),
    }

    fn record_invocation(recorder: &Mutex<Option<Invoked>>, invoked: Invoked) {
        let previous = recorder
            .lock()
            .expect("dispatch recorder mutex should lock")
            .replace(invoked);
        assert!(
            previous.is_none(),
            "dispatch must invoke exactly one runner"
        );
    }

    #[tokio::test]
    async fn dispatches_every_subcommand_to_its_matching_runner() {
        let cases = [
            (
                vec![
                    "lfscloud",
                    "--config",
                    "lfscloud.test.yml",
                    "serve",
                    "--host",
                    "127.0.0.2",
                    "--port",
                    "8088",
                ],
                Invoked::Serve(ServeOptions::new(
                    Some("lfscloud.test.yml".into()),
                    Some("127.0.0.2".to_owned()),
                    Some(8088),
                )),
            ),
            (
                vec![
                    "lfscloud",
                    "init",
                    "--server",
                    "http://lfs.example.com",
                    "--allow-insecure-http",
                    "--local",
                ],
                Invoked::Init(InitCommand {
                    server: "http://lfs.example.com".to_owned(),
                    allow_insecure_http: true,
                    local: true,
                }),
            ),
            (
                vec![
                    "lfscloud",
                    "login",
                    "--server",
                    "http://lfs.example.com",
                    "--allow-insecure-http",
                ],
                Invoked::Login(LoginCommand {
                    server: "http://lfs.example.com".to_owned(),
                    allow_insecure_http: true,
                }),
            ),
            (
                vec![
                    "lfscloud",
                    "logout",
                    "--server",
                    "http://lfs.example.com",
                    "--allow-insecure-http",
                ],
                Invoked::Logout(LogoutCommand {
                    server: "http://lfs.example.com".to_owned(),
                    allow_insecure_http: true,
                }),
            ),
            (
                vec![
                    "lfscloud",
                    "--config",
                    "lfscloud.test.yml",
                    "status",
                    "--server",
                    "http://lfs.example.com",
                    "--allow-insecure-http",
                    "--cache-root",
                    "/tmp/lfscloud-status-cache",
                ],
                Invoked::Status(
                    StatusCommand {
                        server: Some("http://lfs.example.com".to_owned()),
                        allow_insecure_http: true,
                        cache_root: Some("/tmp/lfscloud-status-cache".into()),
                    },
                    Some("lfscloud.test.yml".into()),
                ),
            ),
            (
                vec![
                    "lfscloud",
                    "pull",
                    "--cache-root",
                    "/tmp/lfscloud-pull-cache",
                ],
                Invoked::Pull(PullCommand {
                    cache_root: Some("/tmp/lfscloud-pull-cache".into()),
                }),
            ),
            (
                vec![
                    "lfscloud",
                    "hydrate",
                    "--cache-root",
                    "/tmp/lfscloud-hydrate-cache",
                    "asset/model.bin",
                    "asset/audio.bin",
                ],
                Invoked::Hydrate(HydrateCommand {
                    cache_root: Some("/tmp/lfscloud-hydrate-cache".into()),
                    paths: vec!["asset/model.bin".into(), "asset/audio.bin".into()],
                }),
            ),
            (
                vec![
                    "lfscloud",
                    "dehydrate",
                    "--cache-root",
                    "/tmp/lfscloud-dehydrate-cache",
                    "asset/model.bin",
                    "asset/audio.bin",
                ],
                Invoked::Dehydrate(DehydrateCommand {
                    cache_root: Some("/tmp/lfscloud-dehydrate-cache".into()),
                    paths: vec!["asset/model.bin".into(), "asset/audio.bin".into()],
                }),
            ),
            (
                vec![
                    "lfscloud",
                    "gc",
                    "--cache-root",
                    "/tmp/lfscloud-gc-cache",
                    "--dry-run",
                    "--prune-unavailable-worktrees",
                ],
                Invoked::Gc(GcCommand {
                    cache_root: Some("/tmp/lfscloud-gc-cache".into()),
                    dry_run: true,
                    prune_unavailable_worktrees: true,
                }),
            ),
            (
                vec![
                    "lfscloud",
                    "--config",
                    "lfscloud.test.yml",
                    "migrate",
                    "--server",
                    "http://lfs.example.com",
                    "--allow-insecure-http",
                    "--cache-root",
                    "/tmp/lfscloud-migrate-cache",
                    "--source-remote",
                    "upstream",
                    "--allow-cross-remote",
                    "--ref",
                    "main",
                    "--ref",
                    "feature",
                    "--dry-run",
                    "--purge-source-lfs",
                ],
                Invoked::Migrate(
                    MigrateCommand {
                        server: "http://lfs.example.com".to_owned(),
                        allow_insecure_http: true,
                        cache_root: Some("/tmp/lfscloud-migrate-cache".into()),
                        source_remote: "upstream".to_owned(),
                        allow_cross_remote: true,
                        refs: vec!["main".to_owned(), "feature".to_owned()],
                        all_refs: false,
                        dry_run: true,
                        purge_source_lfs: true,
                    },
                    Some("lfscloud.test.yml".into()),
                ),
            ),
        ];

        for (args, expected) in cases {
            let cli = Cli::try_parse_from(&args).expect("subcommand should parse");
            let recorded = Mutex::new(None);
            let recorder = &recorded;

            dispatch(
                cli,
                |options| async move {
                    record_invocation(recorder, Invoked::Serve(options));
                    Ok(())
                },
                |command| {
                    record_invocation(recorder, Invoked::Init(command));
                    Ok(())
                },
                |command| {
                    record_invocation(recorder, Invoked::Login(command));
                    Ok(())
                },
                |command| {
                    record_invocation(recorder, Invoked::Logout(command));
                    Ok(())
                },
                |command, config| {
                    record_invocation(recorder, Invoked::Status(command, config));
                    Ok(())
                },
                |command| {
                    record_invocation(recorder, Invoked::Pull(command));
                    Ok(())
                },
                |command| {
                    record_invocation(recorder, Invoked::Hydrate(command));
                    Ok(())
                },
                |command| {
                    record_invocation(recorder, Invoked::Dehydrate(command));
                    Ok(())
                },
                |command| {
                    record_invocation(recorder, Invoked::Gc(command));
                    Ok(())
                },
                |command, config| {
                    record_invocation(recorder, Invoked::Migrate(command, config));
                    Ok(())
                },
            )
            .await
            .expect("dispatch should succeed");

            assert_eq!(
                recorded
                    .into_inner()
                    .expect("dispatch recorder mutex should not poison"),
                Some(expected),
                "wrong runner or parsed fields for {args:?}",
            );
        }
    }
    #[test]
    fn init_writes_lfsconfig_from_current_repo_origin() {
        require_git();

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let nested = repo.path().join("nested/path");
        fs::create_dir_all(&nested).expect("nested directory should be created");
        let mut output = Vec::new();

        run_init_from_dir(
            InitCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                local: false,
            },
            &nested,
            &mut output,
        )
        .expect("init config should be written");

        let lfs_url = "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs";
        assert_eq!(
            String::from_utf8(output).expect("output should be UTF-8"),
            format!(
                "configured .lfsconfig\n  path: {}\n  - lfs.url: <unset>\n  + lfs.url: {lfs_url}\n",
                dunce::canonicalize(repo.path())
                    .expect("repo path should canonicalize")
                    .join(".lfsconfig")
                    .display()
            )
        );
        assert_eq!(
            read_git_config(
                repo.path(),
                &["config", "--file", ".lfsconfig", "--get", "lfs.url"]
            ),
            lfs_url
        );
    }

    #[test]
    fn init_updates_existing_lfsconfig_with_diff_output() {
        require_git();

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        fs::write(
            repo.path().join(".lfsconfig"),
            "[lfs]\n\turl = https://old.example/info/lfs\n",
        )
        .expect("existing .lfsconfig should be written");
        let mut output = Vec::new();

        run_init_from_dir(
            InitCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                local: false,
            },
            repo.path(),
            &mut output,
        )
        .expect("init config should be updated");

        let lfs_url = "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs";
        assert_eq!(
            String::from_utf8(output).expect("output should be UTF-8"),
            format!(
                "configured .lfsconfig\n  path: {}\n  - lfs.url: https://old.example/info/lfs\n  + lfs.url: {lfs_url}\n",
                dunce::canonicalize(repo.path())
                    .expect("repo path should canonicalize")
                    .join(".lfsconfig")
                    .display()
            )
        );
        assert_eq!(
            read_git_config(
                repo.path(),
                &["config", "--file", ".lfsconfig", "--get", "lfs.url"]
            ),
            lfs_url
        );
    }

    #[test]
    fn init_summary_redacts_sensitive_previous_lfs_url() {
        let change = GitLfsConfigChange {
            target: GitLfsConfigTarget::WorktreeFile,
            path: Path::new(".lfsconfig").to_owned(),
            previous_url: Some(
                "https://user:old-secret@old.example/info/lfs?token=query-secret#fragment-secret"
                    .to_owned(),
            ),
            new_url: "https://lfs.example.com/owner/repo.git/info/lfs".to_owned(),
        };
        let mut output = Vec::new();

        write_init_change(&mut output, &change).expect("summary should be written");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(
            rendered.contains("https://REDACTED:REDACTED@old.example/info/lfs?REDACTED#REDACTED")
        );
        assert!(!rendered.contains("old-secret"));
        assert!(!rendered.contains("query-secret"));
        assert!(!rendered.contains("fragment-secret"));
    }

    #[test]
    fn init_local_option_writes_local_git_config_without_lfsconfig() {
        require_git();

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let mut output = Vec::new();

        run_init_from_dir(
            InitCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                local: true,
            },
            repo.path(),
            &mut output,
        )
        .expect("local init config should be written");

        let lfs_url = "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs";
        assert_eq!(
            String::from_utf8(output).expect("output should be UTF-8"),
            format!(
                "configured local Git config\n  path: {}\n  - lfs.url: <unset>\n  + lfs.url: {lfs_url}\n",
                dunce::canonicalize(repo.path())
                    .expect("repo path should canonicalize")
                    .join(".git")
                    .join("config")
                    .display()
            )
        );
        assert_eq!(
            read_git_config(repo.path(), &["config", "--local", "--get", "lfs.url"]),
            lfs_url
        );
        assert!(
            !repo.path().join(".lfsconfig").exists(),
            "local-only init should not create .lfsconfig"
        );
    }

    #[test]
    fn status_reports_ready_repository_mapping_auth_storage_and_cache() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        fs::create_dir_all(cache_root.join("objects")).expect("cache objects dir should exist");
        fs::create_dir_all(&repo).expect("repository directory should be created");
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:Owner/Repo.git"],
        );
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_status_from_dir(
            StatusCommand {
                server: None,
                allow_insecure_http: false,
                cache_root: Some(cache_root),
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
            |lfs_url| {
                assert_eq!(
                    lfs_url,
                    "http://127.0.0.1:8080/github.com/Owner/Repo.git/info/lfs"
                );
                Ok(())
            },
            |_| Ok(()),
        )
        .expect("status should pass when every check is ready");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfscloud status"));
        assert!(rendered.contains("config     ok"));
        assert!(rendered.contains("repository ok"));
        assert!(rendered.contains("server     ok"));
        assert!(rendered.contains("route      ok"));
        assert!(rendered.contains("mapping    ok      github-main:owner/repo -> drive-user-a"));
        assert!(rendered.contains("auth       ok      local LFS credential found"));
        assert!(
            rendered
                .contains("storage    ok      google_drive drive-user-a credential is configured")
        );
        assert!(rendered.contains("cache      ok"));
    }

    #[test]
    fn status_reports_failures_without_leaking_credential_secrets() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repository directory should be created");
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        let error = run_status_from_dir(
            StatusCommand {
                server: Some("http://127.0.0.1:8080".to_owned()),
                allow_insecure_http: false,
                cache_root: Some(temp.path().join("cache")),
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| {
                Err(CliError::InvalidArguments {
                    message: "connection refused".to_owned(),
                })
            },
            |_| {
                Err(CliError::InvalidArguments {
                    message: "missing token secret".to_owned(),
                })
            },
            |_| {
                Err(CliError::InvalidArguments {
                    message: "credential env var is missing".to_owned(),
                })
            },
        )
        .expect_err("failed checks should make status fail");

        assert!(matches!(error, CliError::StatusFailed { .. }));
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("server     error"));
        assert!(rendered.contains("auth       error"));
        assert!(rendered.contains("storage    error"));
        assert!(rendered.contains("cache      warning"));
        assert!(!rendered.contains("password="));
    }

    #[test]
    fn status_redacts_unsafe_server_override_before_route_validation() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repository directory should be created");
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let unsafe_server_url =
            "http://user:secret@127.0.0.1:8080?token=query-secret#fragment-secret";
        let mut output = Vec::new();

        let error = run_status_from_dir(
            StatusCommand {
                server: Some(unsafe_server_url.to_owned()),
                allow_insecure_http: false,
                cache_root: Some(temp.path().join("cache")),
            },
            Some(config_path),
            &repo,
            &mut output,
            |server_url| {
                assert_eq!(server_url, unsafe_server_url);
                Ok(())
            },
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect_err("unsafe server URL should make status fail route validation");

        assert!(matches!(error, CliError::StatusFailed { .. }));
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("server     ok"));
        assert!(rendered.contains("REDACTED"));
        assert!(!rendered.contains("user:secret"));
        assert!(!rendered.contains("query-secret"));
        assert!(!rendered.contains("fragment-secret"));
    }

    #[test]
    fn status_probe_rejects_unsafe_server_url_components() {
        let error = probe_server_reachable(
            "http://user:secret@127.0.0.1:8080?token=query-secret#fragment-secret",
        )
        .expect_err("unsafe server URL should fail before probing reachability");

        assert!(
            matches!(error, CliError::InvalidArguments { message } if message.contains("credentials"))
        );
    }

    #[tokio::test]
    async fn migration_target_probe_authenticates_the_repository_batch_route() {
        let observed = Arc::new(Mutex::new(None));
        let observed_for_route = Arc::clone(&observed);
        let app = Router::new().route(
            "/github.com/owner/repo.git/info/lfs/objects/batch",
            post(
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let observed = Arc::clone(&observed_for_route);
                    async move {
                        *observed
                            .lock()
                            .expect("migration target probe record should not poison") = Some((
                            headers
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned),
                            body,
                        ));
                        StatusCode::OK
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("migration target probe listener should bind");
        let address = listener
            .local_addr()
            .expect("migration target probe address should be available");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("migration target probe server should run");
        });
        let token = LfsSessionToken::from_secret("migration-session-token")
            .expect("migration session token should be valid");

        probe_authenticated_migration_target(
            &format!("http://{address}/github.com/owner/repo.git/info/lfs"),
            false,
            &token,
        )
        .await
        .expect("repository-scoped authenticated probe should succeed");
        server.abort();

        let observed = observed
            .lock()
            .expect("migration target probe record should not poison")
            .clone()
            .expect("migration target probe request should be recorded");
        assert_eq!(
            observed.0.as_deref(),
            Some("Bearer migration-session-token")
        );
        assert_eq!(observed.1["operation"], "upload");
        assert_eq!(observed.1["objects"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn migration_target_probe_rejects_an_inactive_session() {
        let app = Router::new().route(
            "/github.com/owner/repo.git/info/lfs/objects/batch",
            post(|| async { StatusCode::UNAUTHORIZED }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("migration target probe listener should bind");
        let address = listener
            .local_addr()
            .expect("migration target probe address should be available");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("migration target probe server should run");
        });
        let token = LfsSessionToken::from_secret("expired-migration-session")
            .expect("migration session token should be valid");

        let error = probe_authenticated_migration_target(
            &format!("http://{address}/github.com/owner/repo.git/info/lfs"),
            false,
            &token,
        )
        .await
        .expect_err("inactive migration session should fail before migration work");
        server.abort();

        assert!(
            matches!(error, CliError::ExternalCommandOutput { command, message }
            if command == "migration target repository authentication"
                && message.as_str().contains("401"))
        );
    }

    #[tokio::test]
    async fn migration_target_probe_rejects_non_loopback_http_without_opt_in() {
        let token = LfsSessionToken::from_secret("migration-session-token")
            .expect("migration session token should be valid");

        let error = probe_authenticated_migration_target(
            "http://example.com/github.com/owner/repo.git/info/lfs",
            false,
            &token,
        )
        .await
        .expect_err("non-loopback HTTP should require explicit opt-in");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
    }

    #[test]
    fn status_storage_validation_uses_generic_credential_error() {
        let directory = TempDir::new().expect("temporary directory should be created");
        let storage = StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig {
            id: "drive-user-a".to_owned(),
            credentials: crate::GoogleDriveGcloudCredentialsConfig {
                config_dir: directory.path().join("missing-gcloud-drive"),
                executable: PathBuf::from("gcloud"),
            },
            root_folder_id: "root-folder".to_owned(),
            display_name: None,
        });

        let error = validate_status_storage(&storage)
            .expect_err("missing storage credential should fail validation");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
        let rendered = error.to_string();
        assert!(rendered.contains("drive-user-a"));
        assert!(rendered.contains("gcloud ADC"));
        assert!(!rendered.contains("missing-gcloud-drive"));
    }

    #[test]
    fn status_storage_validation_accepts_generated_gcloud_state() {
        let directory = TempDir::new().expect("temporary directory should be created");
        fs::write(
            directory
                .path()
                .join("application_default_credentials.json"),
            "{}",
        )
        .expect("ADC marker file should be written");
        let storage = StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig {
            id: "drive-user-a".to_owned(),
            credentials: crate::GoogleDriveGcloudCredentialsConfig {
                config_dir: directory.path().to_owned(),
                executable: PathBuf::from("rustc"),
            },
            root_folder_id: "root-folder".to_owned(),
            display_name: None,
        });

        validate_status_storage(&storage)
            .expect("generated gcloud ADC state should pass local status validation");
    }

    #[test]
    fn status_storage_validation_reports_missing_gcloud_state_generically() {
        let directory = TempDir::new().expect("temporary directory should be created");
        let storage = StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig {
            id: "drive-user-a".to_owned(),
            credentials: crate::GoogleDriveGcloudCredentialsConfig {
                config_dir: directory.path().join("private-gcloud-drive"),
                executable: PathBuf::from("gcloud"),
            },
            root_folder_id: "root-folder".to_owned(),
            display_name: None,
        });

        let error = validate_status_storage(&storage)
            .expect_err("missing gcloud ADC state should fail validation");

        let rendered = error.to_string();
        assert!(rendered.contains("drive-user-a"));
        assert!(rendered.contains("gcloud"));
        assert!(!rendered.contains("private-gcloud-drive"));
    }

    #[test]
    fn migrate_dry_run_reports_current_checkout_plan_without_writes() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        run_git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:Owner/Repo.git",
            ],
        );
        run_git(
            &repo,
            &["config", "filter.lfs.clean", "git-lfs clean -- %f"],
        );
        run_git(
            &repo,
            &["config", "filter.lfs.smudge", "git-lfs smudge -- %f"],
        );
        run_git(
            &repo,
            &["config", "filter.lfs.process", "git-lfs filter-process"],
        );
        run_git(&repo, &["config", "filter.lfs.required", "true"]);
        let local_git_config_path = GitRepository::discover(&repo)
            .expect("temporary repository should be discovered")
            .local_git_config_path()
            .expect("local Git config path should resolve");
        let object = object_for_bytes(b"migration object already local");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        write_git_lfs_source_object(&repo, &object, b"migration object already local");
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: false,
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
            |lfs_url| {
                assert_eq!(
                    lfs_url,
                    "http://127.0.0.1:8080/github.com/Owner/Repo.git/info/lfs"
                );
                Ok(())
            },
            |_| Ok(()),
        )
        .expect("dry-run migration plan should be reported");

        assert!(
            !repo.join(".lfsconfig").exists(),
            "dry-run must not write Git LFS config"
        );
        assert!(
            !cache_root.exists(),
            "dry-run must not create local cache state"
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfscloud migrate dry-run"));
        assert!(rendered.contains("mode: current-checkout"));
        assert!(rendered.contains("scope: current checkout index only"));
        assert!(rendered.contains(
            "warning: other refs were not scanned and may reference additional LFS objects"
        ));
        assert!(rendered.contains("use --all-refs for a full provider move"));
        assert!(rendered.contains("refs scanned: 1"));
        assert!(rendered.contains("current checkout"));
        assert!(rendered.contains("files touched: 2 would update"));
        assert!(rendered.contains(".lfsconfig"));
        assert!(rendered.contains(&local_git_config_path.display().to_string()));
        assert!(rendered.contains("tracked LFS patterns: 1"));
        assert!(rendered.contains("*.bin (.gitattributes; filter=lfs)"));
        assert!(rendered.contains("pointer files: 1"));
        assert!(rendered.contains(&format!(
            "objects discovered: 1 ({} bytes total)",
            object.size.bytes()
        )));
        assert!(rendered.contains("objects fetched: 0 would fetch, 1 already local"));
        assert!(rendered.contains(&format!(
            "0 bytes would fetch, {} bytes already local",
            object.size.bytes()
        )));
        assert!(rendered.contains("source objects: 1 local, 0 missing locally"));
        assert!(
            rendered.contains("target objects: 0 confirmed new, 0 confirmed existing, 1 unknown")
        );
        assert!(rendered.contains("target storage not probed during dry-run"));
        assert!(!rendered.contains("objects uploaded:"));
        assert!(rendered.contains("local readiness checks (no remote access probes):"));
        assert!(rendered.contains("git-lfs"));
        assert!(rendered.contains("lfs-filters ok"));
        assert!(rendered.contains("source-config"));
        assert!(rendered.contains("server-tcp"));
        assert!(rendered.contains("lfs-credential"));
        assert!(rendered.contains("storage-credential"));
        assert!(rendered.contains("source repository access not probed"));
        assert!(rendered.contains("server authentication and repository access not probed"));
        assert!(rendered.contains("Drive root access not probed"));
        assert!(rendered.contains("warnings:"));
        assert!(rendered.contains("repository permissions were not probed"));
        assert!(rendered.contains("storage quota and free capacity were not probed"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[tokio::test]
    async fn migrate_execution_uploads_every_historical_asset_version_before_reconfiguring() {
        require_git();
        require_git_lfs();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.name", "LFS Cloud Migration Test"]);
        run_git(
            &repo,
            &["config", "user.email", "migration@example.invalid"],
        );
        run_git(&repo, &["config", "commit.gpgSign", "false"]);
        run_git(&repo, &["lfs", "install", "--local"]);

        let first_bytes = b"historical LFS asset version one\n";
        let latest_bytes = b"latest LFS asset version two with different bytes\n";
        let first_object = object_for_bytes(first_bytes);
        let latest_object = object_for_bytes(latest_bytes);
        write_file(
            &repo.join(".gitattributes"),
            b"assets/*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        write_file(
            &repo.join("assets/model.bin"),
            LfsPointer::new(first_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_git_lfs_source_object(&repo, &first_object, first_bytes);
        run_git(&repo, &["add", ".gitattributes", "assets/model.bin"]);
        run_git(&repo, &["commit", "-m", "Add first LFS asset version"]);
        let first_commit = read_git_config(&repo, &["rev-parse", "HEAD"]);

        write_file(
            &repo.join("assets/model.bin"),
            LfsPointer::new(latest_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_git_lfs_source_object(&repo, &latest_object, latest_bytes);
        run_git(&repo, &["add", "assets/model.bin"]);
        run_git(&repo, &["commit", "-m", "Change LFS asset bytes"]);

        let command = MigrateCommand {
            server: "http://127.0.0.1:8080".to_owned(),
            allow_insecure_http: false,
            cache_root: Some(temp.path().join("cache")),
            source_remote: "origin".to_owned(),
            allow_cross_remote: false,
            refs: Vec::new(),
            all_refs: true,
            dry_run: false,
            purge_source_lfs: false,
        };
        let context = prepare_migration_execution(command, &repo)
            .expect("historical migration execution should prepare")
            .scan_fetched_refs()
            .expect("historical migration execution should scan fetched refs");
        let mapping = RepositoryMapping {
            id: "github-main:owner/repo".to_owned(),
            repo_provider: "github-main".to_owned(),
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
            provider_repository_id: "8675309".to_owned(),
            storage_provider: "drive-user-a".to_owned(),
        };
        let storage = RecordingMigrationStorage::new("drive-user-a");

        let result = execute_migration_with_storage(&context, &mapping, &storage)
            .await
            .expect("historical migration should complete");

        assert_eq!(context.scan.objects.len(), 2);
        assert_eq!(result.storage_upload.uploaded_objects.len(), 2);
        assert!(result.storage_upload.failed_objects.is_empty());
        assert_eq!(
            storage.object_bytes(&first_object).as_deref(),
            Some(first_bytes.as_slice())
        );
        assert_eq!(
            storage.object_bytes(&latest_object).as_deref(),
            Some(latest_bytes.as_slice())
        );
        let receipt = fs::read_to_string(&result.storage_upload.checkpoint_path)
            .expect("durable migration receipt should be readable");
        assert_eq!(receipt.lines().count(), 2);
        assert!(receipt.contains(first_object.oid.as_hex()));
        assert!(receipt.contains(latest_object.oid.as_hex()));

        let target_url = "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs";
        assert!(
            fs::read_to_string(repo.join(".lfsconfig"))
                .expect("migrated .lfsconfig should be readable")
                .contains(target_url)
        );
        assert_eq!(
            read_git_config(&repo, &["config", "--local", "--get", "lfs.url"]),
            target_url
        );

        run_git(&repo, &["checkout", "--quiet", &first_commit]);
        assert_eq!(
            read_git_config(&repo, &["config", "--local", "--get", "lfs.url"]),
            target_url,
            "the local override must keep pre-.lfsconfig history on LFS Cloud"
        );
        assert_eq!(
            fs::read(repo.join("assets/model.bin"))
                .expect("historical LFS asset should remain materializable"),
            first_bytes
        );
    }

    #[tokio::test]
    async fn migrate_execution_does_not_reconfigure_after_a_partial_upload() {
        require_git();
        require_git_lfs();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.name", "LFS Cloud Migration Test"]);
        run_git(
            &repo,
            &["config", "user.email", "migration@example.invalid"],
        );
        run_git(&repo, &["config", "commit.gpgSign", "false"]);
        run_git(&repo, &["lfs", "install", "--local"]);
        let bytes = b"migration object that will fail at the target\n";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs -text\n");
        write_file(
            &repo.join("asset.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        write_git_lfs_source_object(&repo, &object, bytes);
        run_git(&repo, &["add", ".gitattributes", "asset.bin"]);
        run_git(&repo, &["commit", "-m", "Add LFS asset"]);
        let context = prepare_migration_execution(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(temp.path().join("cache")),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: true,
                dry_run: false,
                purge_source_lfs: false,
            },
            &repo,
        )
        .expect("migration execution should prepare")
        .scan_fetched_refs()
        .expect("migration execution should scan fetched refs");
        let mapping = RepositoryMapping {
            id: "github-main:owner/repo".to_owned(),
            repo_provider: "github-main".to_owned(),
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
            provider_repository_id: "8675309".to_owned(),
            storage_provider: "drive-user-a".to_owned(),
        };
        let storage = RecordingMigrationStorage::new("drive-user-a").failing(object.clone());

        let error = execute_migration_with_storage(&context, &mapping, &storage)
            .await
            .expect_err("partial target upload should fail migration execution");

        assert!(
            matches!(error, CliError::MigrationUploadFailed { failures: 1, oid, .. }
            if oid == object.oid.as_hex())
        );
        assert!(!repo.join(".lfsconfig").exists());
        let local_url = ProcessCommand::new("git")
            .args(["config", "--local", "--get", "lfs.url"])
            .current_dir(&repo)
            .output()
            .expect("local Git config lookup should start");
        assert_eq!(local_url.status.code(), Some(1));
        assert!(local_url.stdout.is_empty());
    }

    #[test]
    fn migrate_requires_acknowledgement_for_cross_remote_identity() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:target/repo.git",
            ],
        );
        run_git(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "git@github.com:source/repo.git",
            ],
        );
        let command = |allow_cross_remote| MigrateCommand {
            server: "http://127.0.0.1:8080".to_owned(),
            allow_insecure_http: false,
            cache_root: Some(temp.path().join("cache")),
            source_remote: "upstream".to_owned(),
            allow_cross_remote,
            refs: Vec::new(),
            all_refs: false,
            dry_run: true,
            purge_source_lfs: false,
        };
        let mut denied_output = Vec::new();

        let error = run_migrate_from_dir(
            command(false),
            Some(temp.path().join("missing-config.yml")),
            &repo,
            &mut denied_output,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect_err("cross-repository migration should require explicit acknowledgement");

        assert!(matches!(error, CliError::InvalidArguments { message }
            if message.contains("github.com/source/repo")
                && message.contains("github.com/target/repo")
                && message.contains("--allow-cross-remote")));
        assert!(denied_output.is_empty());

        let mut allowed_output = Vec::new();
        run_migrate_from_dir(
            command(true),
            Some(temp.path().join("missing-config.yml")),
            &repo,
            &mut allowed_output,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("acknowledged cross-repository dry run should report the plan");

        let rendered = String::from_utf8(allowed_output).expect("output should be UTF-8");
        assert!(rendered.contains("source remote: upstream (github.com/source/repo)"));
        assert!(rendered.contains("target remote: origin (github.com/target/repo)"));
        assert!(rendered.contains("source: https://github.com/source/repo.git/info/lfs"));
        assert!(
            rendered.contains("target: http://127.0.0.1:8080/github.com/target/repo.git/info/lfs")
        );
    }

    #[test]
    fn migrate_dry_run_reports_missing_objects_as_would_fetch_without_fetching() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"migration object missing locally");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: false,
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| {
                Err(CliError::InvalidArguments {
                    message: "probe failed".to_owned(),
                })
            },
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration plan should be reported");

        assert!(
            !cache_root.exists(),
            "dry-run must not create cache state while planning fetches"
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("objects fetched: 1 would fetch, 0 already local"));
        assert!(rendered.contains(&format!(
            "{} bytes would fetch, 0 bytes already local",
            object.size.bytes()
        )));
        assert!(rendered.contains("source objects: 0 local, 1 missing locally"));
        assert!(
            rendered.contains("target objects: 0 confirmed new, 0 confirmed existing, 1 unknown")
        );
        assert!(rendered.contains("target storage not probed during dry-run"));
        assert!(!rendered.contains("objects uploaded:"));
        assert!(rendered.contains("server-tcp warning"));
        assert!(rendered.contains(&format!(
            "1 object ({} bytes) has no verified local source",
            object.size.bytes()
        )));
    }

    #[test]
    fn migrate_dry_run_withholds_unverified_github_purge_manifest() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"migration object for GitHub purge report");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: true,
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration purge helper should be reported");

        assert!(
            !cache_root.exists(),
            "purge helper dry-run must not create local cache state"
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("source purge:"));
        assert!(rendered.contains("    source: https://github.com/owner/repo.git/info/lfs"));
        assert!(rendered.contains("provider: GitHub"));
        assert!(rendered.contains("automatic purge: unsupported"));
        assert!(rendered.contains("GitHub LFS purge requires GitHub Support."));
        assert!(
            rendered
                .contains("https://support.github.com/contact-next/product-selection/repositories")
        );
        assert!(rendered.contains("suggested subject: Purge Git LFS objects after migration"));
        assert!(rendered.contains("planned candidates: 1"));
        assert!(rendered.contains("upload not verified"));
        assert!(rendered.contains("purge manifest: unavailable during dry-run planning"));
        assert!(rendered.contains("durable, integrity-verified migration receipt"));
        assert!(
            !rendered
                .lines()
                .any(|line| line.starts_with("      sha256:"))
        );
        assert!(rendered.contains(object.oid.as_hex()));
        assert!(rendered.contains(&format!("{} bytes", object.size.bytes())));
    }

    #[test]
    fn migrate_dry_run_reports_custom_source_as_unsupported_purge_provider() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        run_git(
            &repo,
            &[
                "config",
                "--local",
                "lfs.url",
                "https://lfs.example.com/owner/repo.git/info/lfs",
            ],
        );
        let object = object_for_bytes(b"migration object from custom source");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: true,
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration purge helper should be reported");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("source: https://lfs.example.com/owner/repo.git/info/lfs"));
        assert!(rendered.contains("source purge:"));
        assert!(rendered.contains("    source: https://lfs.example.com/owner/repo.git/info/lfs"));
        assert!(rendered.contains("provider: lfs.example.com"));
        assert!(!rendered.contains("provider: GitHub"));
        assert!(!rendered.contains("GitHub LFS purge requires GitHub Support."));
    }

    #[test]
    fn migrate_dry_run_caps_object_listing_but_keeps_counts() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        for index in 0..=super::MIGRATION_OBJECT_REPORT_LIMIT {
            let bytes = format!("migration object {index}");
            let object = object_for_bytes(bytes.as_bytes());
            write_file(
                &repo.join(format!("asset/model-{index}.bin")),
                LfsPointer::new(object).to_pointer_file().as_bytes(),
            );
        }
        run_git(&repo, &["add", "."]);
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: false,
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration plan should be reported");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("objects discovered: 101"));
        assert!(rendered.contains("... 1 more objects omitted"));
        assert!(
            rendered.contains("target objects: 0 confirmed new, 0 confirmed existing, 101 unknown")
        );
        assert!(rendered.contains("target storage not probed during dry-run"));
        assert!(!rendered.contains("objects uploaded:"));
    }

    #[test]
    fn migrate_dry_run_purge_report_does_not_bypass_object_listing_limit() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        for index in 0..=super::MIGRATION_OBJECT_REPORT_LIMIT {
            let bytes = format!("migration object {index}");
            let object = object_for_bytes(bytes.as_bytes());
            write_file(
                &repo.join(format!("asset/model-{index}.bin")),
                LfsPointer::new(object).to_pointer_file().as_bytes(),
            );
        }
        run_git(&repo, &["add", "."]);
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: true,
            },
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration purge helper should be reported");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        let main_listing_count = rendered
            .lines()
            .filter(|line| line.starts_with("    sha256:") && !line.starts_with("      sha256:"))
            .count();
        assert!(rendered.contains("... 1 more objects omitted"));
        assert_eq!(main_listing_count, super::MIGRATION_OBJECT_REPORT_LIMIT);
        assert!(rendered.contains("planned candidates: 101"));
        assert!(rendered.contains("purge manifest: unavailable during dry-run planning"));
        assert!(
            !rendered
                .lines()
                .any(|line| line.starts_with("      sha256:"))
        );
    }

    #[test]
    fn migrate_execution_requires_all_refs_before_repository_writes() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");

        let error = prepare_migration_execution(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: false,
                purge_source_lfs: false,
            },
            temp.path(),
        )
        .expect_err("execution without all-ref coverage should be rejected");

        assert!(matches!(error, CliError::InvalidArguments { message }
                if message.contains("requires --all-refs") && message.contains("historical")));
        assert!(!cache_root.exists());
    }

    #[test]
    fn pull_fetches_ingests_and_hydrates_current_checkout_pointers() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"object already fetched by git lfs";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        write_file(&repo.join("README.md"), b"not a pointer");
        run_git(
            &repo,
            &["add", ".gitattributes", "asset/model.bin", "README.md"],
        );
        write_git_lfs_source_object(&repo, &object, bytes);
        let fetched_root = Arc::new(Mutex::new(None));
        let fetched_root_for_runner = Arc::clone(&fetched_root);
        let mut output = Vec::new();

        run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &repo,
            &mut output,
            move |worktree_root| {
                *fetched_root_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(worktree_root.to_path_buf());
                Ok(())
            },
        )
        .expect("pull should hydrate fetched objects");

        assert_eq!(
            *fetched_root.lock().expect("capture mutex should lock"),
            Some(dunce::canonicalize(&repo).expect("repo path should canonicalize"))
        );
        assert_eq!(
            fs::read(&worktree_file).expect("hydrated file should be readable"),
            bytes
        );
        let layout = LocalCacheLayout::new(cache_root);
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfscloud pull"));
        assert!(rendered.contains("fetched Git LFS objects"));
        assert!(rendered.contains("tracked paths: 1"));
        assert!(rendered.contains("pointers: 1"));
        assert!(rendered.contains("pulled"));
        assert!(rendered.contains("cached"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn pull_ingests_from_configured_git_lfs_storage_dir() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "lfs.storage", "custom-lfs"]);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"object fetched into custom git lfs storage";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        write_git_lfs_source_object_in(
            &repo.join(".git").join("custom-lfs").join("objects"),
            &object,
            bytes,
        );
        let mut output = Vec::new();

        run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &repo,
            &mut output,
            |_| Ok(()),
        )
        .expect("pull should hydrate from custom git lfs storage");

        assert_eq!(
            fs::read(&worktree_file).expect("hydrated file should be readable"),
            bytes
        );
        let layout = LocalCacheLayout::new(cache_root);
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
    }

    #[test]
    fn pull_ingests_from_git_common_dir_for_linked_worktree() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.email", "lfscloud@example.invalid"]);
        run_git(&repo, &["config", "user.name", "LFS Cloud Test"]);
        let bytes = b"object fetched into common git lfs storage";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        run_git(&repo, &["commit", "-m", "add lfs pointer"]);
        let output = ProcessCommand::new("git")
            .args([
                "worktree",
                "add",
                linked.to_str().expect("test path should be UTF-8"),
            ])
            .current_dir(&repo)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .output()
            .expect("git worktree add should start");
        assert!(
            output.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        write_git_lfs_source_object(&repo, &object, bytes);
        let mut output = Vec::new();

        run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &linked,
            &mut output,
            |_| Ok(()),
        )
        .expect("pull should hydrate from the linked worktree common git dir");

        assert_eq!(
            fs::read(linked.join("asset/model.bin")).expect("hydrated file should be readable"),
            bytes
        );
        let layout = LocalCacheLayout::new(cache_root);
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
    }

    #[test]
    fn pull_propagates_git_lfs_fetch_failure_before_cache_mutation() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"not fetched");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", "asset/model.bin"]);
        let mut output = Vec::new();

        let error = run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &repo,
            &mut output,
            |_| {
                Err(CliError::ExternalCommand {
                    command: "git lfs fetch".to_owned(),
                    status: "exit status: 2".to_owned(),
                    stderr: SanitizedMessage::new("git lfs is unavailable"),
                })
            },
        )
        .expect_err("fetch failure should stop pull");

        assert!(matches!(error, CliError::ExternalCommand { .. }));
        assert!(output.is_empty());
        assert!(
            !cache_root.exists(),
            "pull should not create cache state after fetch failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pull_process_runner_rejects_unbounded_concurrent_output() {
        let mut command = ProcessCommand::new("/bin/sh");
        command.args([
            "-c",
            "(while :; do printf 'stdout-data'; done) & \
             (while :; do printf 'stderr-data' >&2; done) & wait",
        ]);
        let started = Instant::now();

        let error = run_bounded_child_command(
            &mut command,
            "test pull fetch",
            Duration::from_secs(5),
            1024,
        )
        .expect_err("unbounded command output should be rejected");

        assert!(
            matches!(error, CliError::ExternalCommandOutput { message, .. }
                if message.as_str().contains("exceeded the 1024-byte limit"))
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "output overflow should stop the process before the timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pull_process_runner_terminates_descendants_on_timeout() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let escaped_marker = temp.path().join("escaped");
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .args(["-c", "(sleep 1; printf escaped > \"$1\") & wait", "sh"])
            .arg(&escaped_marker);

        let error = run_bounded_child_command(
            &mut command,
            "test pull fetch",
            Duration::from_millis(50),
            1024,
        )
        .expect_err("stalled command should time out");

        assert!(matches!(error, CliError::ExternalCommand { status, .. }
                if status.contains("timed out")));
        std::thread::sleep(Duration::from_millis(1_100));
        assert!(
            !escaped_marker.exists(),
            "the timed-out command's descendant must not outlive the boundary"
        );
    }

    #[test]
    fn pull_reports_failures_after_attempting_remaining_pointers() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let missing_file = repo.join("asset/missing.bin");
        let available_file = repo.join("asset/available.bin");
        let missing_object = object_for_bytes(b"missing fetched object");
        let available_bytes = b"available fetched object";
        let available_object = object_for_bytes(available_bytes);
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        write_file(
            &missing_file,
            LfsPointer::new(missing_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &available_file,
            LfsPointer::new(available_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        run_git(
            &repo,
            &[
                "add",
                ".gitattributes",
                "asset/available.bin",
                "asset/missing.bin",
            ],
        );
        write_git_lfs_source_object(&repo, &available_object, available_bytes);
        let mut output = Vec::new();

        let error = run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root),
            },
            &repo,
            &mut output,
            |_| Ok(()),
        )
        .expect_err("one missing fetched object should fail pull");

        assert!(matches!(
            error,
            CliError::PullFailed {
                failures: 1,
                path,
                ..
            } if path == dunce::canonicalize(&missing_file)
                .unwrap_or_else(|_| missing_file.clone())
        ));
        assert_eq!(
            fs::read(&available_file).expect("available file should be hydrated"),
            available_bytes
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("tracked paths: 2"));
        assert!(rendered.contains("pointers: 2"));
        assert!(rendered.contains("failed"));
        assert!(rendered.contains("missing.bin"));
        assert!(rendered.contains("pulled"));
        assert!(rendered.contains("available.bin"));
    }

    #[test]
    fn current_checkout_pointer_scan_uses_lfs_tracked_files_only() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let tracked_object = object_for_bytes(b"tracked pointer object");
        let untracked_object = object_for_bytes(b"untracked pointer object");
        let ordinary_object = object_for_bytes(b"ordinary tracked pointer-shaped object");
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/tracked.bin"),
            LfsPointer::new(tracked_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("asset/untracked.bin"),
            LfsPointer::new(untracked_object)
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("docs/pointer-example.txt"),
            LfsPointer::new(ordinary_object)
                .to_pointer_file()
                .as_bytes(),
        );
        run_git(
            &repo,
            &[
                "add",
                ".gitattributes",
                "asset/tracked.bin",
                "docs/pointer-example.txt",
            ],
        );

        let pointers = current_checkout_lfs_pointer_files(&repo)
            .expect("pointer scan should inspect tracked files");

        assert_eq!(pointers.len(), 1);
        assert_eq!(pointers[0].object, tracked_object);
        assert_eq!(pointers[0].path, repo.join("asset/tracked.bin"));
    }

    #[test]
    fn current_checkout_pointer_scan_reports_tracked_and_pointer_counts() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"tracked pointer object");
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/pointer.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        write_file(&repo.join("asset/empty.bin"), b"");
        write_file(&repo.join("asset/hydrated.bin"), b"already hydrated bytes");
        run_git(
            &repo,
            &[
                "add",
                ".gitattributes",
                "asset/pointer.bin",
                "asset/empty.bin",
                "asset/hydrated.bin",
            ],
        );

        let scan = current_checkout_lfs_pointer_scan(&repo)
            .expect("pointer scan should inspect tracked files");

        assert_eq!(scan.tracked_path_count, 3);
        assert_eq!(scan.pointer_files.len(), 1);
        assert_eq!(scan.pointer_files[0].object, object);
    }

    #[cfg(unix)]
    #[test]
    fn current_checkout_pointer_scan_accepts_non_utf8_tracked_paths() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"non UTF-8 path object");
        let non_utf8_name = OsString::from_vec(b"nonutf8-\xff.bin".to_vec());
        let worktree_file = repo.join("asset").join(PathBuf::from(non_utf8_name));
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        fs::create_dir_all(worktree_file.parent().expect("path should have parent"))
            .expect("non-UTF-8 path parent should be created");
        if fs::write(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        )
        .is_err()
        {
            return;
        }
        run_git(&repo, &["add", "-A"]);

        let pointers = current_checkout_lfs_pointer_files(&repo)
            .expect("pointer scan should accept non-UTF-8 paths");

        assert_eq!(pointers.len(), 1);
        assert_eq!(pointers[0].object, object);
        assert_eq!(pointers[0].path, worktree_file);
    }

    #[test]
    fn hydrate_replaces_pointer_file_with_verified_cache_object() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"cached model bytes";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        let mut output = Vec::new();

        run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("hydrate should replace pointer with cache bytes");

        assert_eq!(
            fs::read(&worktree_file).expect("hydrated file should be readable"),
            bytes
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("hydrated"));
        #[cfg(target_os = "macos")]
        {
            let file_system = rustix::fs::statfs(temp.path())
                .expect("test filesystem should be inspectable")
                .f_fstypename;
            let is_apfs = file_system
                .iter()
                .copied()
                .take_while(|byte| *byte != 0)
                .map(|byte| byte as u8)
                .eq(b"apfs".iter().copied());
            if is_apfs {
                assert!(rendered.contains("copy-on-write-cloned"));
            } else {
                assert!(rendered.contains("copied"));
            }
        }
        #[cfg(not(target_os = "macos"))]
        assert!(rendered.contains("copied"));
        assert!(
            rendered.contains(
                &dunce::canonicalize(&worktree_file)
                    .expect("worktree file should canonicalize")
                    .display()
                    .to_string()
            )
        );
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn dehydrate_caches_clean_file_and_writes_pointer() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"hydrated model bytes";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        stage_lfs_pointer(&repo, "asset/model.bin", &object);
        write_file(&worktree_file, bytes);
        let mut output = Vec::new();

        run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("dehydrate should cache bytes and write pointer");

        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cached file should be readable"),
            bytes
        );
        assert_eq!(
            fs::read_to_string(&worktree_file).expect("pointer file should be readable"),
            LfsPointer::new(object.clone()).to_pointer_file()
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("dehydrated"));
        assert!(rendered.contains("cached-and-replaced-with-pointer"));
        assert!(
            rendered.contains(
                &dunce::canonicalize(&worktree_file)
                    .expect("worktree file should canonicalize")
                    .display()
                    .to_string()
            )
        );
        assert!(rendered.contains(object.oid.as_hex()));

        let mut gc_output = Vec::new();
        run_gc_from_dir(
            GcCommand {
                cache_root: Some(layout.root().to_path_buf()),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut gc_output,
        )
        .expect("gc should retain the dehydrated pointer's cached bytes");
        assert!(layout.object_path(&object).exists());
    }

    #[test]
    fn dehydrate_accepts_existing_pointer_as_idempotent() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let object = object_for_bytes(b"already dehydrated bytes");
        let pointer = LfsPointer::new(object.clone()).to_pointer_file();
        stage_lfs_pointer(&repo, "asset/model.bin", &object);
        let mut output = Vec::new();

        run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("existing pointer should be accepted");

        assert_eq!(
            fs::read_to_string(&worktree_file).expect("pointer file should be readable"),
            pointer
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("already-dehydrated"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn dehydrate_rejects_dirty_lfs_content_without_caching_it() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let clean_object = object_for_bytes(b"clean hydrated model bytes");
        let dirty_bytes = b"dirty edit that must not be preserved as LFS content";
        let dirty_object = object_for_bytes(dirty_bytes);
        stage_lfs_pointer(&repo, "asset/model.bin", &clean_object);
        write_file(&worktree_file, dirty_bytes);
        let mut output = Vec::new();

        let error = run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root.clone()),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("dirty LFS content must not be dehydrated");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::IntegrityMismatch { .. }
            }
        ));
        assert_eq!(
            fs::read(&worktree_file).expect("dirty file should remain readable"),
            dirty_bytes
        );
        assert!(
            !LocalCacheLayout::new(cache_root)
                .object_path(&dirty_object)
                .exists()
        );
        assert!(output.is_empty());
    }

    #[test]
    fn dehydrate_rejects_untracked_and_non_lfs_paths() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        write_file(&repo.join("untracked.bin"), b"untracked bytes");
        write_file(&repo.join("tracked.txt"), b"ordinary tracked bytes");
        run_git(&repo, &["add", "tracked.txt"]);

        for path in ["untracked.bin", "tracked.txt"] {
            let mut output = Vec::new();
            let error = run_dehydrate_from_dir(
                DehydrateCommand {
                    cache_root: Some(cache_root.clone()),
                    paths: vec![PathBuf::from(path)],
                },
                &repo,
                &mut output,
            )
            .expect_err("only tracked filter=lfs paths may be dehydrated");

            assert!(matches!(error, CliError::InvalidArguments { .. }));
            assert!(output.is_empty());
        }

        assert!(!cache_root.join("objects").exists());
    }

    #[test]
    fn dehydrate_rejects_paths_outside_the_current_worktree() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside.bin");
        init_git_repo_with_origin(&repo);
        write_file(&outside, b"outside bytes");
        for path in [outside.clone(), PathBuf::from("../outside.bin")] {
            let mut output = Vec::new();
            let error = run_dehydrate_from_dir(
                DehydrateCommand {
                    cache_root: Some(cache_root.clone()),
                    paths: vec![path],
                },
                &repo,
                &mut output,
            )
            .expect_err("outside paths must not be dehydrated");

            assert!(matches!(error, CliError::InvalidArguments { .. }));
            assert!(output.is_empty());
        }
        assert_eq!(
            fs::read(&outside).expect("outside file should remain readable"),
            b"outside bytes"
        );
        assert!(!cache_root.join("objects").exists());
    }

    #[test]
    fn hydrate_rejects_paths_outside_the_current_worktree() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside.bin");
        init_git_repo_with_origin(&repo);
        let bytes = b"outside cached bytes";
        let object = object_for_bytes(bytes);
        let pointer = LfsPointer::new(object.clone()).to_pointer_file();
        write_file(&outside, pointer.as_bytes());
        write_file(
            &LocalCacheLayout::new(&cache_root).object_path(&object),
            bytes,
        );
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![outside.clone()],
            },
            &repo,
            &mut output,
        )
        .expect_err("outside paths must not be hydrated");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
        assert_eq!(
            fs::read_to_string(&outside).expect("outside pointer should remain readable"),
            pointer
        );
        assert!(output.is_empty());
    }

    #[test]
    fn dehydrate_republishes_cache_bytes_for_real_git_lfs_push() {
        require_git_lfs();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.name", "LFS Cloud Test"]);
        run_git(&repo, &["config", "user.email", "lfscloud@example.invalid"]);
        run_git(&repo, &["lfs", "install", "--local"]);
        run_git(temp.path(), &["init", "--bare", "remote.git"]);
        write_file(
            &repo.join(".gitattributes"),
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"object restored to Git LFS media before push";
        let object = object_for_bytes(bytes);
        write_file(&worktree_file, bytes);
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        run_git(&repo, &["commit", "-m", "Add LFS object"]);
        let local_media = repo.join(".git").join("lfs").join("objects");
        fs::remove_dir_all(&local_media).expect("local Git LFS media should be removable");
        let mut output = Vec::new();

        run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("dehydrate should restore Git LFS media");
        run_git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                remote
                    .to_str()
                    .expect("temporary remote path should be UTF-8"),
            ],
        );
        run_git(&repo, &["lfs", "push", "origin", "HEAD"]);

        let oid = object.oid.as_hex();
        let remote_object = remote
            .join("lfs")
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(oid);
        assert_eq!(
            fs::read(remote_object).expect("pushed Git LFS object should be readable"),
            bytes
        );
    }

    #[test]
    fn hydrate_rejects_non_pointer_worktree_content_with_local_cache_error() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        write_file(&worktree_file, b"plain worktree bytes");
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("non-pointer content should not hydrate");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::PointerParse { path, .. }
            } if path == dunce::canonicalize(&worktree_file).unwrap_or(worktree_file)
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn hydrate_reports_missing_cache_object_as_local_cache_error() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let object = object_for_bytes(b"not cached yet");
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("missing cache object should fail hydration");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::MissingCacheObject { oid, .. }
            } if oid == object.oid
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn dehydrate_rejects_non_file_path_before_cache_mutation() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        fs::create_dir_all(repo.join("asset/model.bin")).expect("test directory should be created");
        let mut output = Vec::new();

        let error = run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("directory path should not dehydrate");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
        assert!(output.is_empty());
    }

    #[test]
    fn hydrate_stops_when_one_of_multiple_paths_fails() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let missing_cache_file = repo.join("asset/missing.bin");
        let cached_file = repo.join("asset/cached.bin");
        let missing_object = object_for_bytes(b"missing cache object");
        let cached_object = object_for_bytes(b"cached object");
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(
            &missing_cache_file,
            LfsPointer::new(missing_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        let cached_pointer = LfsPointer::new(cached_object.clone()).to_pointer_file();
        write_file(&cached_file, cached_pointer.as_bytes());
        write_file(&layout.object_path(&cached_object), b"cached object");
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![
                    PathBuf::from("asset/missing.bin"),
                    PathBuf::from("asset/cached.bin"),
                ],
            },
            &repo,
            &mut output,
        )
        .expect_err("first missing cache object should stop hydration");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::MissingCacheObject { oid, .. }
            } if oid == missing_object.oid
        ));
        assert!(output.is_empty());
        assert_eq!(
            fs::read_to_string(&cached_file).expect("second path should remain readable"),
            cached_pointer
        );
    }

    #[test]
    fn gc_removes_unreferenced_cache_objects_and_reports_summary() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let keep_bytes = b"gc referenced object";
        let remove_bytes = b"gc unreferenced object";
        let keep_object = object_for_bytes(keep_bytes);
        let remove_object = object_for_bytes(remove_bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(&layout.object_path(&keep_object), keep_bytes);
        write_file(&layout.object_path(&remove_object), remove_bytes);
        stage_lfs_pointer(&repo, "asset/model.bin", &keep_object);
        let mut output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut output,
        )
        .expect("gc should remove unreferenced cache objects");

        assert!(layout.object_path(&keep_object).exists());
        assert!(!layout.object_path(&remove_object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfscloud gc"));
        assert!(rendered.contains("worktrees: 1 active, 0 unavailable, 0 pruned"));
        assert!(rendered.contains("objects: 1 retained, 0 protected, 1 removed, 0 skipped"));
        assert!(rendered.contains(remove_object.oid.as_hex()));
    }

    #[test]
    fn gc_dry_run_reports_without_removing_cache_objects() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let bytes = b"gc dry-run unreferenced object";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(&layout.object_path(&object), bytes);
        let mut output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: true,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut output,
        )
        .expect("gc dry-run should report unreferenced cache objects");

        assert!(layout.object_path(&object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("objects: 0 retained, 0 protected, 1 would remove, 0 skipped"));
        assert!(rendered.contains("would remove"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn gc_requires_explicit_pruning_before_collecting_with_unavailable_worktrees() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let missing_repo = temp.path().join("disconnected-repo");
        init_git_repo_with_origin(&repo);
        let bytes = b"possibly referenced by disconnected worktree";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        let missing_registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/disconnected",
            &missing_repo,
            missing_repo.join(".git"),
        )
        .expect("missing worktree registration should validate");
        layout
            .register_worktree(missing_registration)
            .expect("missing worktree should register");
        write_file(&layout.object_path(&object), bytes);
        let mut protected_output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root.clone()),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut protected_output,
        )
        .expect("ordinary gc should preserve objects for unavailable worktrees");

        assert!(layout.object_path(&object).exists());
        let rendered = String::from_utf8(protected_output).expect("output should be UTF-8");
        assert!(rendered.contains("1 unavailable, 0 pruned"));
        assert!(rendered.contains("1 protected, 0 removed"));
        assert!(rendered.contains("unavailable worktree"));
        assert!(rendered.contains("protected while worktree unavailable"));

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: false,
                prune_unavailable_worktrees: true,
            },
            &repo,
            &mut Vec::new(),
        )
        .expect("explicit pruning should permit collection");

        assert!(!layout.object_path(&object).exists());
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees()
                .len(),
            1
        );
    }

    #[test]
    fn gc_runs_outside_git_worktree() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let start_dir = temp.path().join("outside-repo");
        let bytes = b"gc outside repo object";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        fs::create_dir_all(&start_dir).expect("start directory should be created");
        write_file(&layout.object_path(&object), bytes);
        let mut output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &start_dir,
            &mut output,
        )
        .expect("gc should run without a current Git worktree");

        assert!(!layout.object_path(&object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("worktrees: 0 active, 0 unavailable, 0 pruned"));
        assert!(rendered.contains("objects: 0 retained, 0 protected, 1 removed, 0 skipped"));
    }

    #[test]
    fn gc_ignores_only_non_worktree_git_discovery_failures() {
        let outside_worktree = CliError::ExternalCommand {
            command: "git rev-parse --show-toplevel".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: SanitizedMessage::new(
                "fatal: not a git repository (or any of the parent directories): .git",
            ),
        };
        assert!(is_git_worktree_discovery_error(&outside_worktree));

        let bare_repository = CliError::ExternalCommand {
            command: "git rev-parse --show-toplevel".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: SanitizedMessage::new("fatal: this operation must be run in a work tree"),
        };
        assert!(is_git_worktree_discovery_error(&bare_repository));

        let unsafe_repository = CliError::ExternalCommand {
            command: "git rev-parse --show-toplevel".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: SanitizedMessage::new(
                "fatal: detected dubious ownership in repository at '/tmp/repo'",
            ),
        };
        assert!(!is_git_worktree_discovery_error(&unsafe_repository));

        let start_failure = CliError::Io {
            context: "failed to start git rev-parse --show-toplevel".to_owned(),
            source: io::Error::new(io::ErrorKind::NotFound, "git"),
        };
        assert!(!is_git_worktree_discovery_error(&start_failure));
    }

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

    fn status_config(public_url: &str) -> String {
        format!(
            r#"
server:
  host: 127.0.0.1
  port: 8080
  public_url: {public_url}

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: github-pat

storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
    root_folder_id: root-folder

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#
        )
    }

    fn object_for_bytes(bytes: &[u8]) -> LfsObject {
        let oid = LfsOid::new(format!("{:x}", Sha256::digest(bytes)))
            .expect("test SHA-256 object id should parse");

        LfsObject::new(
            oid,
            LfsObjectSize::new(u64::try_from(bytes.len()).expect("test bytes should fit u64")),
        )
    }

    fn write_file(path: &Path, contents: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("file parent should be created");
        }
        fs::write(path, contents).expect("test file should be written");
    }

    fn write_git_lfs_source_object(repo: &Path, object: &LfsObject, contents: &[u8]) {
        write_git_lfs_source_object_in(
            &repo.join(".git").join("lfs").join("objects"),
            object,
            contents,
        );
    }

    fn write_git_lfs_source_object_in(objects_dir: &Path, object: &LfsObject, contents: &[u8]) {
        let oid = object.oid.as_hex();
        let path = objects_dir.join(&oid[..2]).join(&oid[2..4]).join(oid);
        write_file(&path, contents);
    }

    fn init_git_repo_with_origin(repo: &Path) {
        fs::create_dir_all(repo).expect("temporary repository directory should be created");
        run_git(repo, &["init"]);
        run_git(
            repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
    }

    fn stage_lfs_pointer(repo: &Path, relative_path: &str, object: &LfsObject) {
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join(relative_path),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(repo, &["add", ".gitattributes", relative_path]);
    }

    fn require_git() {
        let output = ProcessCommand::new("git")
            .arg("--version")
            .output()
            .expect("Git is required to run CLI integration tests");
        assert!(
            output.status.success(),
            "Git is required to run CLI integration tests: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn require_git_lfs() {
        let output = ProcessCommand::new("git")
            .args(["lfs", "version"])
            .output()
            .expect("Git LFS is required to run CLI integration tests");
        assert!(
            output.status.success(),
            "Git LFS is required to run CLI integration tests: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn run_git(current_dir: &Path, args: &[&str]) {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(current_dir)
            .output()
            .expect("git command should start");

        assert!(
            output.status.success(),
            "git command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn read_git_config(current_dir: &Path, args: &[&str]) -> String {
        let output = ProcessCommand::new("git")
            .args(args)
            .current_dir(current_dir)
            .output()
            .expect("git config command should start");

        assert!(
            output.status.success(),
            "git config command failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout)
            .expect("git config output should be UTF-8")
            .trim_end()
            .to_owned()
    }
}
