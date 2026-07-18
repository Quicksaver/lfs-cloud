//! Command-line parsing and dispatch for LFS Cloud.
//!
//! This module keeps the binary target small while making CLI behavior
//! testable without binding sockets. The process entry point owns global
//! tracing initialization, while parser and dispatch helpers stay side-effect
//! free for focused tests.

#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs::{self, File},
    future::Future,
    io::{self, BufRead, Read, Write},
    net::{SocketAddr, TcpStream, ToSocketAddrs},
    path::{Component, Path, PathBuf},
    process::{Child, Command as ProcessCommand, ExitStatus, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};
use reqwest::{Client, StatusCode as HttpStatusCode, redirect::Policy};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{CliError, CliResult, SanitizedMessage, git::redacted_url_for_display};
use crate::{
    GITHUB_OAUTH_LOGIN_PATH, GitCredentialApproval, GitCredentialLookup, GitCredentialRejection,
    GitLfsConfigChange, GitLfsConfigTarget, GitLfsHistoryPointers, GitLfsMigrationDiscovery,
    GitLfsSourceEndpointSource, GitRepository, GoogleDriveCredentialLoader,
    GoogleDriveStorageConfig, LFS_SESSION_REVOKE_PATH, LfsInitRoute, LfsObject, LfsObjectSize,
    LfsOid, LfsPointer, LfsSessionToken, LocalCacheDehydration, LocalCacheDehydrationStatus,
    LocalCacheGarbageCollection, LocalCacheGarbageCollectionObject, LocalCacheIngest,
    LocalCacheIngestStatus, LocalCacheLayout, LocalCacheMaterialization,
    LocalCacheMaterializationStatus, LocalCacheWorktreeRegistration,
    LocalMigrationObjectAvailability, ServeOptions, ServerConfig, StorageProviderConfig,
    TracingConfig, check_local_migration_objects, discover_git_lfs_migration,
    enumerate_all_fetched_ref_lfs_pointers, enumerate_current_checkout_lfs_pointers,
    enumerate_selected_ref_lfs_pointers, init_tracing,
};

const STATUS_SERVER_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const SESSION_REVOCATION_TIMEOUT: Duration = Duration::from_secs(30);
const PULL_FETCH_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_PULL_FETCH_OUTPUT_BYTES: usize = 256 * 1024;
const CHILD_OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(100);
const CHILD_OUTPUT_DRAIN_AFTER_KILL: Duration = Duration::from_secs(1);
const MIGRATION_OBJECT_REPORT_LIMIT: usize = 100;
const SOURCE_ENDPOINT_UNSET_LABEL: &str = "<unset>";
const SOURCE_PROVIDER_UNKNOWN_LABEL: &str = "unknown";
const MAX_CLI_POINTER_CANDIDATE_SIZE: u64 = 64 * 1024;

#[derive(Debug, Parser)]
#[command(name = "lfs-cloud", version, about, propagate_version = true)]
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
    /// Start GitHub OAuth login and store the local LFS token for this repo.
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
    /// Plan migration from an existing Git LFS provider.
    Migrate(MigrateCommand),
}

#[derive(Debug, Args)]
struct ServeCommand {
    /// Host or interface address to bind.
    #[arg(long)]
    host: Option<String>,

    /// TCP port to bind.
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Debug, Args)]
struct LoginCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,

    /// Print the login URL without trying to open a browser.
    #[arg(long)]
    no_open: bool,
}

#[derive(Debug, Args)]
struct LogoutCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,
}

#[derive(Debug, Args)]
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

#[derive(Debug, Args)]
struct StatusCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: Option<String>,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,

    /// Local cache root to inspect instead of ~/.lfs-cloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct PullCommand {
    /// Local cache root to use instead of ~/.lfs-cloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct HydrateCommand {
    /// Local cache root to use instead of ~/.lfs-cloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Git LFS pointer files to replace with cached object bytes.
    #[arg(value_name = "PATH", required = true)]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct DehydrateCommand {
    /// Local cache root to use instead of ~/.lfs-cloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Clean hydrated files to replace with Git LFS pointers.
    #[arg(value_name = "PATH", required = true)]
    paths: Vec<PathBuf>,
}

#[derive(Debug, Args)]
struct GcCommand {
    /// Local cache root to clean instead of ~/.lfs-cloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Report objects and worktree registrations that would be removed.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Args)]
struct MigrateCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    allow_insecure_http: bool,

    /// Local cache root to inspect instead of ~/.lfs-cloud.
    #[arg(long, value_name = "PATH")]
    cache_root: Option<PathBuf>,

    /// Scan one selected branch, tag, or ref. Can be repeated.
    #[arg(long = "ref", value_name = "REF", conflicts_with = "all_refs")]
    refs: Vec<String>,

    /// Scan every fetched local branch, remote-tracking branch, and tag.
    #[arg(long, conflicts_with = "refs")]
    all_refs: bool,

    /// Report the migration plan without fetching, uploading, or writing config.
    #[arg(long, required = true)]
    dry_run: bool,

    /// Include GitHub source-LFS purge helper text in the migration report.
    ///
    /// GitHub does not expose a normal self-service API for arbitrary LFS
    /// object deletion, so this flag reports support-flow instructions instead
    /// of attempting source-provider mutation.
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
            .context("failed to run lfs-cloud server"),
        Command::Login(command) => login(command).context("failed to complete lfs-cloud login"),
        Command::Logout(command) => logout(command).context("failed to complete lfs-cloud logout"),
        Command::Init(command) => init(command).context("failed to resolve lfs-cloud init route"),
        Command::Status(command) => {
            status(command, cli.config).context("failed to check lfs-cloud status")
        }
        Command::Pull(command) => pull(command).context("failed to pull LFS objects"),
        Command::Hydrate(command) => hydrate(command).context("failed to hydrate paths"),
        Command::Dehydrate(command) => dehydrate(command).context("failed to dehydrate paths"),
        Command::Gc(command) => gc(command).context("failed to garbage collect local cache"),
        Command::Migrate(command) => {
            migrate(command, cli.config).context("failed to plan lfs-cloud migration")
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
    let mut input = io::stdin().lock();
    let mut stdout = io::stdout().lock();

    run_login(command, &mut input, &mut stdout)
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

    run_migrate_from_dir(
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

fn run_login<R, W>(command: LoginCommand, input: &mut R, output: &mut W) -> anyhow::Result<()>
where
    R: BufRead,
    W: Write,
{
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;

    run_login_from_dir(
        command,
        &current_dir,
        input,
        output,
        open_url_in_default_browser,
        |approval| approval.approve_in_dir(&current_dir),
    )
    .map_err(anyhow::Error::from)
}

fn run_login_from_dir<R, W, O, A>(
    command: LoginCommand,
    start_dir: impl AsRef<Path>,
    input: &mut R,
    output: &mut W,
    mut open_browser: O,
    mut approve_credential: A,
) -> CliResult<()>
where
    R: BufRead,
    W: Write,
    O: FnMut(&str) -> CliResult<()>,
    A: FnMut(GitCredentialApproval) -> CliResult<()>,
{
    let repository = GitRepository::discover(start_dir.as_ref()).map_err(login_discovery_error)?;
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    let login_url = login_url_for_server(&route.server_url)?;

    writeln!(output, "authorize LFS Cloud with GitHub:").map_err(output_error)?;
    writeln!(output, "  {login_url}").map_err(output_error)?;
    if command.no_open {
        writeln!(output, "browser open skipped").map_err(output_error)?;
    } else {
        match open_browser(&login_url) {
            Ok(()) => writeln!(output, "opened browser for GitHub OAuth").map_err(output_error)?,
            Err(error) => writeln!(output, "browser open failed: {error}").map_err(output_error)?,
        }
    }
    writeln!(
        output,
        "paste the lfs_token value from the callback response, then press Enter."
    )
    .map_err(output_error)?;
    write!(output, "lfs_token: ").map_err(output_error)?;
    output.flush().map_err(output_error)?;

    let mut token = String::new();
    input.read_line(&mut token).map_err(|source| CliError::Io {
        context: "failed to read lfs_token from stdin".to_owned(),
        source,
    })?;
    let token =
        LfsSessionToken::from_secret(token.trim()).map_err(|_| CliError::InvalidArguments {
            message: "lfs_token was invalid or blank".to_owned(),
        })?;
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
    let mut url = crate::init::validate_server_url(server_url, true)?;
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| CliError::InvalidArguments {
            message: "server URL cannot be used as a route base".to_owned(),
        })?;
    segments.extend(LFS_SESSION_REVOKE_PATH.trim_start_matches('/').split('/'));
    drop(segments);

    Ok(url.to_string())
}

fn request_lfs_session_revocation(
    revoke_url: &str,
    token: &LfsSessionToken,
) -> CliResult<SessionRevocationStatus> {
    let client = Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|source| CliError::Io {
            context: "failed to create LFS session revocation client".to_owned(),
            source: io::Error::other(source),
        })?;
    let response = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(
            client
                .delete(revoke_url)
                .bearer_auth(token.as_str())
                .timeout(SESSION_REVOCATION_TIMEOUT)
                .send(),
        )
    })
    .map_err(|source| CliError::Io {
        context: "failed to request LFS session revocation".to_owned(),
        source: io::Error::other(source),
    })?;

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
                message: "lfs-cloud login requires an origin remote; add the repository remote before logging in".to_owned(),
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
    writeln!(output, "lfs-cloud pull").map_err(output_error)?;
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
    register_current_worktree(&layout, start_dir)?;

    for path in command.paths {
        let path = resolve_cli_path(start_dir, &path);
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
    register_current_worktree(&layout, start_dir)?;

    for path in command.paths {
        let path = resolve_cli_path(start_dir, &path);
        let object = object_for_dehydration_path(&path)?;
        let dehydration = layout
            .dehydrate_file(&object, &path)
            .map_err(local_cache_cli_error)?;
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
        .garbage_collect(command.dry_run)
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
            message: "lfs-cloud migrate currently supports planning with --dry-run only".to_owned(),
        });
    }

    let start_dir = start_dir.as_ref();
    let repository = GitRepository::discover(start_dir)?;
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    let discovery = discover_git_lfs_migration(start_dir)?;
    let scan = migration_pointer_scan(start_dir, &command)?;
    let cache_layout = Some(local_cache_layout(command.cache_root.clone())?);
    let availability =
        check_local_migration_objects(start_dir, scan.objects.iter(), cache_layout.as_ref())?;
    let config_path = config_path.unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let access_checks = migration_access_checks(
        &config_path,
        &repository,
        MigrationTargetAccess {
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
        scan,
        availability,
        route,
        config_path,
        access_checks,
        would_touch_files: migration_dry_run_touched_files(&repository),
        source_purge,
    };

    write_migration_dry_run_report(output, &report).map_err(output_error)
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
}

#[derive(Debug)]
struct MigrationDryRunReport {
    discovery: GitLfsMigrationDiscovery,
    scan: MigrationPointerScan,
    availability: LocalMigrationObjectAvailability,
    route: LfsInitRoute,
    config_path: PathBuf,
    access_checks: Vec<MigrationAccessCheck>,
    would_touch_files: Vec<PathBuf>,
    source_purge: Option<MigrationSourcePurgeReport>,
}

#[derive(Debug)]
struct MigrationAccessCheck {
    name: &'static str,
    level: StatusLevel,
    message: String,
}

#[derive(Clone, Copy, Debug)]
struct MigrationTargetAccess<'a> {
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
) -> CliResult<MigrationPointerScan> {
    if command.all_refs {
        let history = enumerate_all_fetched_ref_lfs_pointers(start_dir)?;
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

fn migration_access_checks<P, A, S>(
    config_path: &Path,
    repository: &GitRepository,
    target: MigrationTargetAccess<'_>,
    discovery: &GitLfsMigrationDiscovery,
    probe_server: &mut P,
    lookup_credential: &mut A,
    validate_storage: &mut S,
) -> Vec<MigrationAccessCheck>
where
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
{
    let mut checks = Vec::new();
    checks.push(migration_source_access_check(discovery));
    checks.push(migration_target_access_check(
        target.server_url,
        probe_server,
    ));

    checks.push(match lookup_credential(target.lfs_url) {
        Ok(()) => MigrationAccessCheck {
            name: "auth",
            level: StatusLevel::Ok,
            message: "local LFS credential found".to_owned(),
        },
        Err(error) => MigrationAccessCheck {
            name: "auth",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        },
    });

    match ServerConfig::load_from_path(config_path) {
        Ok(config) => {
            checks.push(MigrationAccessCheck {
                name: "config",
                level: StatusLevel::Ok,
                message: format!("loaded {}", config_path.display()),
            });
            migration_config_access_checks(&mut checks, &config, repository, validate_storage);
        }
        Err(error) => checks.push(MigrationAccessCheck {
            name: "config",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        }),
    }

    checks
}

fn migration_target_access_check<P>(server_url: &str, probe_server: &mut P) -> MigrationAccessCheck
where
    P: FnMut(&str) -> CliResult<()>,
{
    let display = redacted_url_for_display(server_url);
    match probe_server(server_url) {
        Ok(()) => MigrationAccessCheck {
            name: "target",
            level: StatusLevel::Ok,
            message: format!("{display} is reachable"),
        },
        Err(error) => MigrationAccessCheck {
            name: "target",
            level: StatusLevel::Warning,
            message: format!("{display} is unreachable: {error}"),
        },
    }
}

fn migration_source_access_check(discovery: &GitLfsMigrationDiscovery) -> MigrationAccessCheck {
    match (&discovery.source_endpoint, discovery.installation.installed) {
        (Some(endpoint), true) => MigrationAccessCheck {
            name: "source",
            level: StatusLevel::Ok,
            message: format!(
                "{} ({})",
                redacted_url_for_display(&endpoint.url),
                source_endpoint_source_label(endpoint.source)
            ),
        },
        (Some(endpoint), false) => MigrationAccessCheck {
            name: "source",
            level: StatusLevel::Warning,
            message: format!(
                "{} configured, but git lfs is not available for source fetches",
                redacted_url_for_display(&endpoint.url)
            ),
        },
        (None, _) => MigrationAccessCheck {
            name: "source",
            level: StatusLevel::Warning,
            message: "source Git LFS endpoint is not configured".to_owned(),
        },
    }
}

fn migration_config_access_checks<S>(
    checks: &mut Vec<MigrationAccessCheck>,
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
        checks.push(MigrationAccessCheck {
            name: "mapping",
            level: StatusLevel::Warning,
            message: format!(
                "no server config entry for {}",
                repository.remote.repository_label()
            ),
        });
        return;
    };

    checks.push(MigrationAccessCheck {
        name: "mapping",
        level: StatusLevel::Ok,
        message: format!("{} -> {}", mapping.id, mapping.storage_provider),
    });

    let Some(storage) = config.storage_providers.get(&mapping.storage_provider) else {
        checks.push(MigrationAccessCheck {
            name: "storage",
            level: StatusLevel::Warning,
            message: format!(
                "mapping {} references unknown storage provider {}",
                mapping.id, mapping.storage_provider
            ),
        });
        return;
    };

    checks.push(match validate_storage(storage) {
        Ok(()) => MigrationAccessCheck {
            name: "storage",
            level: StatusLevel::Ok,
            message: format!(
                "{} {} credential is configured",
                storage.provider_type(),
                storage.id()
            ),
        },
        Err(error) => MigrationAccessCheck {
            name: "storage",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        },
    });
}

fn migration_dry_run_touched_files(repository: &GitRepository) -> Vec<PathBuf> {
    vec![repository.worktree_root.join(".lfsconfig")]
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
    let available_count = report.availability.available_objects().len();
    let fetch_count = report.availability.unavailable_objects().len();

    writeln!(output, "lfs-cloud migrate dry-run")?;
    writeln!(
        output,
        "  worktree: {}",
        report.discovery.worktree_root.display()
    )?;
    writeln!(output, "  mode: {}", report.scan.mode.label())?;
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
        "  objects discovered: {}",
        report.scan.objects.len()
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
        "  objects uploaded: {available_count} ready to upload, {fetch_count} after fetch"
    )?;
    writeln!(output, "  access checks:")?;
    for check in &report.access_checks {
        writeln!(
            output,
            "    {:<10} {:<7} {}",
            check.name,
            check.level.label(),
            check.message
        )?;
    }
    if let Some(source_purge) = &report.source_purge {
        write_migration_source_purge_report(output, source_purge, report)?;
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
    let total_bytes = report
        .scan
        .objects
        .iter()
        .map(|object| u128::from(object.size.bytes()))
        .sum::<u128>();

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
                "    report objects: {} ({} bytes)",
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
                "    instructions: use GitHub's repository support flow or Virtual Agent, then provide the object IDs and sizes from this report if requested."
            )?;
            writeln!(
                output,
                "    purge manifest: complete object list for GitHub Support"
            )?;
            for object in &report.scan.objects {
                writeln!(
                    output,
                    "      sha256:{} ({} bytes)",
                    object.oid,
                    object.size.bytes()
                )?;
            }
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
    let repository_id = repository.remote.repository_label();
    let git_dir = repository.git_dir_path()?;
    let registration =
        LocalCacheWorktreeRegistration::new(repository_id, repository.worktree_root, git_dir)
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

#[derive(Debug)]
struct BoundedChildOutput {
    status: ExitStatus,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

#[derive(Debug)]
struct BoundedPipeOutput {
    bytes: Vec<u8>,
    exceeded_limit: bool,
}

#[derive(Debug)]
enum BoundedPipeEvent {
    Stdout(io::Result<BoundedPipeOutput>),
    Stderr(io::Result<BoundedPipeOutput>),
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
) -> CliResult<BoundedChildOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_child_process_tree(command);

    let mut child = command.spawn().map_err(|source| CliError::Io {
        context: format!("failed to start {command_name}"),
        source,
    })?;
    let stdout = child.stdout.take().ok_or_else(|| CliError::Io {
        context: format!("failed to capture stdout for {command_name}"),
        source: io::Error::other("child stdout was not piped"),
    })?;
    let stderr = child.stderr.take().ok_or_else(|| CliError::Io {
        context: format!("failed to capture stderr for {command_name}"),
        source: io::Error::other("child stderr was not piped"),
    })?;

    let (sender, receiver) = mpsc::channel();
    let stdout_sender = sender.clone();
    let stdout_reader = std::thread::spawn(move || {
        let result = read_pipe_with_hard_limit(stdout, max_output_bytes);
        let _ = stdout_sender.send(BoundedPipeEvent::Stdout(result));
    });
    let stderr_reader = std::thread::spawn(move || {
        let result = read_pipe_with_hard_limit(stderr, max_output_bytes);
        let _ = sender.send(BoundedPipeEvent::Stderr(result));
    });

    let deadline = Instant::now() + timeout;
    let mut status = None;
    let mut drain_deadline = None;
    let mut stdout = None;
    let mut stderr = None;

    loop {
        while let Ok(event) = receiver.try_recv() {
            let (stream_name, result, destination) = match event {
                BoundedPipeEvent::Stdout(result) => ("stdout", result, &mut stdout),
                BoundedPipeEvent::Stderr(result) => ("stderr", result, &mut stderr),
            };
            let output = match result {
                Ok(output) => output,
                Err(source) => {
                    terminate_child_process_tree(&mut child, command_name)?;
                    return Err(CliError::Io {
                        context: format!("failed to read {stream_name} from {command_name}"),
                        source,
                    });
                }
            };
            if output.exceeded_limit {
                terminate_child_process_tree(&mut child, command_name)?;
                return Err(CliError::ExternalCommandOutput {
                    command: command_name.to_owned(),
                    message: SanitizedMessage::new(format!(
                        "{stream_name} exceeded the {max_output_bytes}-byte limit"
                    )),
                });
            }
            *destination = Some(output.bytes);
        }

        if status.is_none() {
            status = child.try_wait().map_err(|source| CliError::Io {
                context: format!("failed to wait for {command_name}"),
                source,
            })?;
            if status.is_some() {
                drain_deadline = Some(Instant::now() + CHILD_OUTPUT_DRAIN_GRACE);
            }
        }

        if let Some(status) = status.filter(|_| stdout.is_some() && stderr.is_some()) {
            let stdout = stdout.take().expect("stdout was checked above");
            let stderr = stderr.take().expect("stderr was checked above");
            stdout_reader.join().map_err(|_| CliError::Io {
                context: format!("stdout reader thread panicked for {command_name}"),
                source: io::Error::other("stdout reader thread panicked"),
            })?;
            stderr_reader.join().map_err(|_| CliError::Io {
                context: format!("stderr reader thread panicked for {command_name}"),
                source: io::Error::other("stderr reader thread panicked"),
            })?;
            return Ok(BoundedChildOutput {
                status,
                stdout,
                stderr,
            });
        }

        if status.is_none() && Instant::now() >= deadline {
            terminate_child_process_tree(&mut child, command_name)?;
            collect_pipe_events_after_kill(
                &receiver,
                &mut stdout,
                &mut stderr,
                CHILD_OUTPUT_DRAIN_AFTER_KILL,
            );
            return Err(CliError::ExternalCommand {
                command: command_name.to_owned(),
                status: format!("timed out after {} seconds", timeout.as_secs_f64()),
                stderr: sanitized_external_failure_output(
                    stderr.as_deref().unwrap_or_default(),
                    stdout.as_deref().unwrap_or_default(),
                ),
            });
        }

        if status.is_some_and(|_| drain_deadline.is_some_and(|deadline| Instant::now() >= deadline))
        {
            // A descendant inherited one of the pipes after the direct child
            // exited. Terminate the remaining process group before waiting for
            // EOF so this command boundary cannot hang on the descendant.
            stop_child_process_tree(&child);
            collect_pipe_events_after_kill(
                &receiver,
                &mut stdout,
                &mut stderr,
                CHILD_OUTPUT_DRAIN_AFTER_KILL,
            );
            if stdout.is_none() || stderr.is_none() {
                return Err(CliError::Io {
                    context: format!("timed out draining output from {command_name}"),
                    source: io::Error::new(
                        io::ErrorKind::TimedOut,
                        "child output pipes remained open after process exit",
                    ),
                });
            }
        }

        std::thread::sleep(Duration::from_millis(10));
    }
}

fn read_pipe_with_hard_limit(
    mut pipe: impl Read,
    max_output_bytes: usize,
) -> io::Result<BoundedPipeOutput> {
    let mut bytes = Vec::with_capacity(max_output_bytes.min(8192));
    let mut buffer = [0_u8; 8192];
    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            return Ok(BoundedPipeOutput {
                bytes,
                exceeded_limit: false,
            });
        }

        let remaining = max_output_bytes.saturating_sub(bytes.len());
        bytes.extend_from_slice(&buffer[..count.min(remaining)]);
        if count > remaining {
            return Ok(BoundedPipeOutput {
                bytes,
                exceeded_limit: true,
            });
        }
    }
}

fn collect_pipe_events_after_kill(
    receiver: &mpsc::Receiver<BoundedPipeEvent>,
    stdout: &mut Option<Vec<u8>>,
    stderr: &mut Option<Vec<u8>>,
    timeout: Duration,
) {
    let deadline = Instant::now() + timeout;
    while (stdout.is_none() || stderr.is_none()) && Instant::now() < deadline {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or_default();
        let Ok(event) = receiver.recv_timeout(remaining) else {
            break;
        };
        match event {
            BoundedPipeEvent::Stdout(Ok(output)) => *stdout = Some(output.bytes),
            BoundedPipeEvent::Stderr(Ok(output)) => *stderr = Some(output.bytes),
            BoundedPipeEvent::Stdout(Err(_)) | BoundedPipeEvent::Stderr(Err(_)) => {}
        }
    }
}

#[cfg(unix)]
fn configure_child_process_tree(command: &mut ProcessCommand) {
    use std::os::unix::process::CommandExt;

    command.process_group(0);
}

#[cfg(not(unix))]
fn configure_child_process_tree(_command: &mut ProcessCommand) {}

fn terminate_child_process_tree(child: &mut Child, command_name: &str) -> CliResult<()> {
    stop_child_process_tree(child);
    if child
        .try_wait()
        .map_err(|source| CliError::Io {
            context: format!("failed to wait for stopped {command_name}"),
            source,
        })?
        .is_none()
    {
        child.kill().map_err(|source| CliError::Io {
            context: format!("failed to stop {command_name}"),
            source,
        })?;
        child.wait().map_err(|source| CliError::Io {
            context: format!("failed to reap stopped {command_name}"),
            source,
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn stop_child_process_tree(child: &Child) {
    signal_child_process_group("TERM", child.id());
    std::thread::sleep(Duration::from_millis(50));
    signal_child_process_group("KILL", child.id());
}

#[cfg(unix)]
fn signal_child_process_group(signal: &str, process_group_id: u32) {
    let _ = ProcessCommand::new("kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{process_group_id}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn stop_child_process_tree(child: &Child) {
    let _ = ProcessCommand::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn stop_child_process_tree(_child: &Child) {}

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
    let mut fields = output
        .stdout
        .split(|byte| *byte == b'\0')
        .collect::<Vec<_>>();
    if fields.last() == Some(&&[][..]) {
        fields.pop();
    }
    let chunks = fields.chunks_exact(3);
    if !chunks.remainder().is_empty() {
        return Err(git_check_attr_parse_error());
    }
    for chunk in chunks {
        let [relative_path, attribute, value] = chunk else {
            unreachable!("chunks_exact yielded a non-triple chunk");
        };
        if *attribute != b"filter" || *value != b"lfs" {
            continue;
        }

        let relative_path = safe_git_relative_path(relative_path)?;
        lfs_tracked_paths.push(relative_path);
    }

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

fn git_check_attr_parse_error() -> CliError {
    CliError::ExternalCommandOutput {
        command: "git check-attr -z --stdin filter".to_owned(),
        message: SanitizedMessage::new("git returned malformed attribute output"),
    }
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

fn safe_git_relative_path(relative_path: &[u8]) -> CliResult<PathBuf> {
    let path = git_path_bytes_to_path_buf(relative_path, "git check-attr -z --stdin filter")?;
    let valid = !path.is_absolute()
        && path.components().next().is_some()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)));

    if valid {
        Ok(path)
    } else {
        Err(CliError::ExternalCommandOutput {
            command: "git check-attr -z --stdin filter".to_owned(),
            message: SanitizedMessage::new("git returned a path outside the worktree"),
        })
    }
}

#[cfg(unix)]
fn git_path_bytes_to_path_buf(relative_path: &[u8], _command: &str) -> CliResult<PathBuf> {
    Ok(PathBuf::from(OsString::from_vec(relative_path.to_owned())))
}

#[cfg(not(unix))]
fn git_path_bytes_to_path_buf(relative_path: &[u8], command: &str) -> CliResult<PathBuf> {
    String::from_utf8(relative_path.to_owned())
        .map(PathBuf::from)
        .map_err(|_| CliError::ExternalCommandOutput {
            command: command.to_owned(),
            message: SanitizedMessage::new("git returned non-UTF-8 path output"),
        })
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
    if !metadata.is_file() || metadata.len() > MAX_CLI_POINTER_CANDIDATE_SIZE {
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

fn object_for_dehydration_path(path: &Path) -> CliResult<LfsObject> {
    let metadata = fs::metadata(path).map_err(|source| CliError::Io {
        context: format!("failed to inspect dehydration path {}", path.display()),
        source,
    })?;
    if !metadata.is_file() {
        return Err(CliError::InvalidArguments {
            message: format!("dehydration path is not a file: {}", path.display()),
        });
    }

    let mut file = File::open(path).map_err(|source| CliError::Io {
        context: format!("failed to open dehydration path {}", path.display()),
        source,
    })?;

    object_for_dehydration_file(path, &mut file, metadata.len())
}

fn object_for_dehydration_file(path: &Path, file: &mut File, size: u64) -> CliResult<LfsObject> {
    if size > MAX_CLI_POINTER_CANDIDATE_SIZE {
        return hash_file_object_from_reader(path, file);
    }

    let mut contents = Vec::new();
    Read::by_ref(file)
        .take(MAX_CLI_POINTER_CANDIDATE_SIZE + 1)
        .read_to_end(&mut contents)
        .map_err(|source| CliError::Io {
            context: format!("failed to read dehydration path {}", path.display()),
            source,
        })?;
    if contents.len() as u64 > MAX_CLI_POINTER_CANDIDATE_SIZE {
        return hash_file_object_with_prefix(path, &contents, file);
    }

    // Path-only dehydrate has no separate expected object identity, so small
    // valid pointer files are accepted as already dehydrated before hashing.
    // Larger files are treated as content to keep pointer probing bounded.
    if let Ok(contents) = std::str::from_utf8(&contents)
        && let Ok(pointer) = LfsPointer::parse(contents)
    {
        return Ok(pointer.object);
    }

    hash_file_object_with_prefix(path, &contents, file)
}

fn hash_file_object_from_reader(path: &Path, file: &mut File) -> CliResult<LfsObject> {
    hash_file_object_with_prefix(path, &[], file)
}

fn hash_file_object_with_prefix(
    path: &Path,
    prefix: &[u8],
    file: &mut File,
) -> CliResult<LfsObject> {
    let mut hasher = Sha256::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];

    if !prefix.is_empty() {
        hasher.update(prefix);
        size = u64::try_from(prefix.len()).expect("pointer candidate size fits u64");
    }

    loop {
        let bytes_read = file.read(&mut buffer).map_err(|source| CliError::Io {
            context: format!("failed to read dehydration path {}", path.display()),
            source,
        })?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
        size = size
            .checked_add(bytes_read as u64)
            .ok_or_else(|| CliError::InvalidArguments {
                message: format!("file is too large to dehydrate: {}", path.display()),
            })?;
    }

    let oid = LfsOid::new(format!("{:x}", hasher.finalize())).map_err(|source| {
        CliError::InvalidArguments {
            message: format!(
                "failed to build SHA-256 object id for {}: {source}",
                path.display()
            ),
        }
    })?;

    Ok(LfsObject::new(oid, LfsObjectSize::new(size)))
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

    writeln!(output, "lfs-cloud gc")?;
    writeln!(output, "  cache: {}", cache_root.display())?;
    writeln!(
        output,
        "  worktrees: {} active, {} {}",
        report.active_worktree_count,
        report.pruned_worktrees.len(),
        if report.dry_run {
            "would prune"
        } else {
            "pruned"
        }
    )?;
    writeln!(
        output,
        "  objects: {} retained, {} {}, {} skipped",
        report.retained_objects.len(),
        report.unreferenced_objects.len(),
        action,
        report.skipped_cache_paths.len()
    )?;

    for object in &report.unreferenced_objects {
        write_gc_object(output, action, object)?;
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
        LocalCacheMaterializationStatus::CopyOnWriteAttempted => "copy-on-write-attempted",
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
        writeln!(output, "lfs-cloud status")?;
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

fn resolve_socket_addresses_with_timeout(host: String, port: u16) -> CliResult<Vec<SocketAddr>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let thread_host = host.clone();
    std::thread::Builder::new()
        .name("lfs-cloud-status-resolver".to_owned())
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
    GoogleDriveCredentialLoader::new()
        .load_from_environment(storage)
        .map(|_| ())
        .map_err(|_| CliError::InvalidArguments {
            message: format!(
                "Google Drive credential for {} is not usable; check the configured credentials_ref environment value",
                storage.id
            ),
        })
}

fn login_url_for_server(server_url: &str) -> CliResult<String> {
    let mut login_url = Url::parse(server_url).map_err(|source| CliError::InvalidArguments {
        message: format!("server URL is not valid: {source}"),
    })?;
    if !matches!(login_url.scheme(), "http" | "https") || login_url.host_str().is_none() {
        return Err(CliError::InvalidArguments {
            message: "server URL must be a valid http or https URL".to_owned(),
        });
    }
    if !login_url.username().is_empty() || login_url.password().is_some() {
        return Err(CliError::InvalidArguments {
            message: "server URL must not include credentials".to_owned(),
        });
    }
    if login_url.query().is_some() || login_url.fragment().is_some() {
        return Err(CliError::InvalidArguments {
            message: "server URL must not include a query string or fragment".to_owned(),
        });
    }
    if login_url.path().ends_with('/') && login_url.path() != "/" {
        return Err(CliError::InvalidArguments {
            message: "server URL must not end with a trailing slash".to_owned(),
        });
    }
    {
        let mut segments =
            login_url
                .path_segments_mut()
                .map_err(|()| CliError::InvalidArguments {
                    message: "server URL cannot be used as a route base".to_owned(),
                })?;
        segments.extend(GITHUB_OAUTH_LOGIN_PATH.trim_start_matches('/').split('/'));
    }

    Ok(login_url.to_string())
}

fn open_url_in_default_browser(url: &str) -> CliResult<()> {
    let (program, args): (&str, Vec<&str>) = match std::env::consts::OS {
        "macos" => ("open", vec![url]),
        "windows" => ("rundll32", vec!["url.dll,FileProtocolHandler", url]),
        _ => ("xdg-open", vec![url]),
    };
    let output = ProcessCommand::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|source| CliError::Io {
            context: format!("failed to start {program}"),
            source,
        })?;

    if output.status.success() {
        return Ok(());
    }

    Err(CliError::ExternalCommand {
        command: program.to_owned(),
        status: process_status_text(output.status),
        stderr: sanitize_browser_stderr(&output.stderr),
    })
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

fn sanitize_browser_stderr(stderr: &[u8]) -> SanitizedMessage {
    const MAX_BROWSER_STDERR_LEN: usize = 512;

    let mut message = String::from_utf8_lossy(stderr).into_owned();
    message = message.replace(['\r', '\n'], " ");
    if message.len() > MAX_BROWSER_STDERR_LEN {
        let boundary = (0..=MAX_BROWSER_STDERR_LEN)
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
    use std::{
        fs, io,
        path::{Path, PathBuf},
        process::Command as ProcessCommand,
        sync::{Arc, Mutex},
        time::{Duration, Instant},
    };

    use clap::{CommandFactory, Parser};
    use sha2::{Digest, Sha256};
    use tempfile::TempDir;

    use super::{
        Cli, DehydrateCommand, GcCommand, HydrateCommand, InitCommand, LoginCommand, LogoutCommand,
        MigrateCommand, PullCommand, SessionRevocationStatus, StatusCommand,
        current_checkout_lfs_pointer_files, current_checkout_lfs_pointer_scan, dispatch,
        is_git_worktree_discovery_error, login_url_for_server, probe_server_reachable,
        run_bounded_child_command, run_dehydrate_from_dir, run_gc_from_dir, run_hydrate_from_dir,
        run_init_from_dir, run_login_from_dir, run_logout_from_dir, run_migrate_from_dir,
        run_pull_from_dir, run_status_from_dir, sanitize_browser_stderr, tracing_config,
        validate_status_storage, write_init_change,
    };
    use crate::{
        CliError, DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, GitCredentialApproval,
        GitCredentialRejection, GitLfsConfigChange, GitLfsConfigTarget, GoogleDriveStorageConfig,
        LfsObject, LfsObjectSize, LfsOid, LfsPointer, LfsSessionToken, LocalCacheError,
        LocalCacheLayout, SanitizedMessage, ServeOptions, StorageProviderConfig,
    };

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
        let error = Cli::try_parse_from(["lfs-cloud"]).expect_err("command should be required");
        let rendered = error.to_string();

        assert!(rendered.contains("Usage: lfs-cloud"));
        assert!(rendered.contains("Commands:"));
    }

    #[test]
    fn init_command_accepts_required_server_url() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "--config",
            "custom-lfs-cloud.yml",
            "init",
            "--server",
            "http://127.0.0.1:8080",
            "--allow-insecure-http",
        ])
        .expect("init command should parse");

        let super::Command::Init(command) = cli.command else {
            panic!("init subcommand should parse");
        };

        assert_eq!(cli.config, Some("custom-lfs-cloud.yml".into()));
        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.allow_insecure_http);
        assert!(!command.local);
    }

    #[test]
    fn init_command_accepts_local_config_option() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
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
    fn login_command_accepts_server_url_and_no_open_option() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "login",
            "--server",
            "http://127.0.0.1:8080",
            "--no-open",
            "--allow-insecure-http",
        ])
        .expect("login command should parse");

        let super::Command::Login(command) = cli.command else {
            panic!("login subcommand should parse");
        };

        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.allow_insecure_http);
        assert!(command.no_open);
    }

    #[test]
    fn logout_command_accepts_server_url_and_insecure_http_option() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
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
            "lfs-cloud",
            "--config",
            "lfs-cloud.test.yml",
            "status",
            "--server",
            "http://127.0.0.1:8080",
            "--allow-insecure-http",
            "--cache-root",
            "/tmp/lfs-cloud-cache",
        ])
        .expect("status command should parse");

        let super::Command::Status(command) = cli.command else {
            panic!("status subcommand should parse");
        };

        assert_eq!(cli.config, Some("lfs-cloud.test.yml".into()));
        assert_eq!(command.server, Some("http://127.0.0.1:8080".to_owned()));
        assert!(command.allow_insecure_http);
        assert_eq!(command.cache_root, Some("/tmp/lfs-cloud-cache".into()));
    }

    #[test]
    fn pull_command_accepts_cache_root_option() {
        let cli =
            Cli::try_parse_from(["lfs-cloud", "pull", "--cache-root", "/tmp/lfs-cloud-cache"])
                .expect("pull command should parse");

        let super::Command::Pull(command) = cli.command else {
            panic!("pull subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfs-cloud-cache".into()));
    }

    #[test]
    fn hydrate_command_accepts_cache_root_and_paths() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "hydrate",
            "--cache-root",
            "/tmp/lfs-cloud-cache",
            "asset/model.bin",
            "asset/texture.bin",
        ])
        .expect("hydrate command should parse");

        let super::Command::Hydrate(command) = cli.command else {
            panic!("hydrate subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfs-cloud-cache".into()));
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
            "lfs-cloud",
            "dehydrate",
            "--cache-root",
            "/tmp/lfs-cloud-cache",
            "asset/model.bin",
        ])
        .expect("dehydrate command should parse");

        let super::Command::Dehydrate(command) = cli.command else {
            panic!("dehydrate subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfs-cloud-cache".into()));
        assert_eq!(command.paths, vec![PathBuf::from("asset/model.bin")]);
    }

    #[test]
    fn gc_command_accepts_cache_root_and_dry_run_option() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "gc",
            "--cache-root",
            "/tmp/lfs-cloud-cache",
            "--dry-run",
        ])
        .expect("gc command should parse");

        let super::Command::Gc(command) = cli.command else {
            panic!("gc subcommand should parse");
        };

        assert_eq!(command.cache_root, Some("/tmp/lfs-cloud-cache".into()));
        assert!(command.dry_run);
    }

    #[test]
    fn migrate_command_accepts_dry_run_scope_and_cache_options() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "--config",
            "lfs-cloud.test.yml",
            "migrate",
            "--server",
            "http://127.0.0.1:8080",
            "--dry-run",
            "--all-refs",
            "--cache-root",
            "/tmp/lfs-cloud-cache",
            "--purge-source-lfs",
            "--allow-insecure-http",
        ])
        .expect("migrate command should parse");

        let super::Command::Migrate(command) = cli.command else {
            panic!("migrate subcommand should parse");
        };

        assert_eq!(cli.config, Some("lfs-cloud.test.yml".into()));
        assert_eq!(command.server, "http://127.0.0.1:8080");
        assert!(command.allow_insecure_http);
        assert!(command.dry_run);
        assert!(command.all_refs);
        assert!(command.purge_source_lfs);
        assert!(command.refs.is_empty());
        assert_eq!(command.cache_root, Some("/tmp/lfs-cloud-cache".into()));
    }

    #[test]
    fn migrate_command_rejects_missing_dry_run_and_conflicting_ref_scopes() {
        let missing_dry_run =
            Cli::try_parse_from(["lfs-cloud", "migrate", "--server", "http://127.0.0.1:8080"])
                .expect_err("migrate should require explicit dry-run planning");
        assert!(missing_dry_run.to_string().contains("--dry-run"));

        let purge_without_dry_run = Cli::try_parse_from([
            "lfs-cloud",
            "migrate",
            "--server",
            "http://127.0.0.1:8080",
            "--purge-source-lfs",
        ])
        .expect_err("purge helper should require explicit dry-run planning");
        assert!(purge_without_dry_run.to_string().contains("--dry-run"));

        let conflicting_scopes = Cli::try_parse_from([
            "lfs-cloud",
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
            "lfs-cloud",
            "--config",
            "custom-lfs-cloud.yml",
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
                Some("custom-lfs-cloud.yml".into()),
                Some("0.0.0.0".to_owned()),
                Some(9000),
            )
        );
    }

    #[test]
    fn serve_command_accepts_global_config_after_subcommand() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "serve",
            "--config",
            "custom-lfs-cloud.yml",
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
                Some("custom-lfs-cloud.yml".into()),
                Some("0.0.0.0".to_owned()),
                Some(9000),
            )
        );
    }

    #[test]
    fn default_tracing_config_uses_rust_log_env_override() {
        let cli = Cli::try_parse_from(["lfs-cloud", "serve"]).expect("serve command should parse");
        let config = tracing_config(&cli);

        assert_eq!(config.default_filter, DEFAULT_LOG_FILTER);
        assert_eq!(config.env_filter_var.as_deref(), Some(DEFAULT_LOG_ENV_VAR));
    }

    #[test]
    fn explicit_log_level_overrides_rust_log_env() {
        let cli =
            Cli::try_parse_from(["lfs-cloud", "--log-level", "warn,lfs_cloud=debug", "serve"])
                .expect("serve command should parse");
        let config = tracing_config(&cli);

        assert_eq!(config.default_filter, "warn,lfs_cloud=debug");
        assert!(config.env_filter_var.is_none());
    }

    #[tokio::test]
    async fn dispatches_serve_with_global_config_and_overrides() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "--config",
            "lfs-cloud.test.yml",
            "serve",
            "--host",
            "127.0.0.2",
            "--port",
            "8088",
        ])
        .expect("serve command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            move |options| {
                let captured = Arc::clone(&captured_for_runner);
                async move {
                    *captured.lock().expect("capture mutex should lock") = Some(options);
                    Ok(())
                }
            },
            |_| unreachable!("init runner must not be called for serve command"),
            |_| unreachable!("login runner must not be called for serve command"),
            |_| unreachable!("logout runner must not be called for serve command"),
            |_, _| unreachable!("status runner must not be called for serve command"),
            |_| unreachable!("pull runner must not be called for serve command"),
            |_| unreachable!("hydrate runner must not be called for serve command"),
            |_| unreachable!("dehydrate runner must not be called for serve command"),
            |_| unreachable!("gc runner must not be called for serve command"),
            |_, _| unreachable!("migrate runner must not be called for serve command"),
        )
        .await
        .expect("serve dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some(ServeOptions::new(
                Some("lfs-cloud.test.yml".into()),
                Some("127.0.0.2".to_owned()),
                Some(8088),
            ))
        );
    }

    #[tokio::test]
    async fn dispatches_init_with_server_url() {
        let cli = Cli::try_parse_from(["lfs-cloud", "init", "--server", "http://127.0.0.1:8080"])
            .expect("init command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for init command") },
            move |command| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(command.server);
                Ok(())
            },
            |_| unreachable!("login runner must not be called for init command"),
            |_| unreachable!("logout runner must not be called for init command"),
            |_, _| unreachable!("status runner must not be called for init command"),
            |_| unreachable!("pull runner must not be called for init command"),
            |_| unreachable!("hydrate runner must not be called for init command"),
            |_| unreachable!("dehydrate runner must not be called for init command"),
            |_| unreachable!("gc runner must not be called for init command"),
            |_, _| unreachable!("migrate runner must not be called for init command"),
        )
        .await
        .expect("init dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some("http://127.0.0.1:8080".to_owned())
        );
    }

    #[tokio::test]
    async fn dispatches_login_with_server_url() {
        let cli = Cli::try_parse_from(["lfs-cloud", "login", "--server", "http://127.0.0.1:8080"])
            .expect("login command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for login command") },
            |_| unreachable!("init runner must not be called for login command"),
            move |command| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(command.server);
                Ok(())
            },
            |_| unreachable!("logout runner must not be called for login command"),
            |_, _| unreachable!("status runner must not be called for login command"),
            |_| unreachable!("pull runner must not be called for login command"),
            |_| unreachable!("hydrate runner must not be called for login command"),
            |_| unreachable!("dehydrate runner must not be called for login command"),
            |_| unreachable!("gc runner must not be called for login command"),
            |_, _| unreachable!("migrate runner must not be called for login command"),
        )
        .await
        .expect("login dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some("http://127.0.0.1:8080".to_owned())
        );
    }

    #[tokio::test]
    async fn dispatches_logout_with_server_url() {
        let cli = Cli::try_parse_from(["lfs-cloud", "logout", "--server", "http://127.0.0.1:8080"])
            .expect("logout command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for logout command") },
            |_| unreachable!("init runner must not be called for logout command"),
            |_| unreachable!("login runner must not be called for logout command"),
            move |command| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(command.server);
                Ok(())
            },
            |_, _| unreachable!("status runner must not be called for logout command"),
            |_| unreachable!("pull runner must not be called for logout command"),
            |_| unreachable!("hydrate runner must not be called for logout command"),
            |_| unreachable!("dehydrate runner must not be called for logout command"),
            |_| unreachable!("gc runner must not be called for logout command"),
            |_, _| unreachable!("migrate runner must not be called for logout command"),
        )
        .await
        .expect("logout dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some("http://127.0.0.1:8080".to_owned())
        );
    }

    #[tokio::test]
    async fn dispatches_status_with_global_config() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "--config",
            "lfs-cloud.test.yml",
            "status",
            "--server",
            "http://127.0.0.1:8080",
        ])
        .expect("status command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for status command") },
            |_| unreachable!("init runner must not be called for status command"),
            |_| unreachable!("login runner must not be called for status command"),
            |_| unreachable!("logout runner must not be called for status command"),
            move |command, config_path| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some((command.server, config_path));
                Ok(())
            },
            |_| unreachable!("pull runner must not be called for status command"),
            |_| unreachable!("hydrate runner must not be called for status command"),
            |_| unreachable!("dehydrate runner must not be called for status command"),
            |_| unreachable!("gc runner must not be called for status command"),
            |_, _| unreachable!("migrate runner must not be called for status command"),
        )
        .await
        .expect("status dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some((
                Some("http://127.0.0.1:8080".to_owned()),
                Some("lfs-cloud.test.yml".into())
            ))
        );
    }

    #[tokio::test]
    async fn dispatches_pull_with_cache_root() {
        let cli =
            Cli::try_parse_from(["lfs-cloud", "pull", "--cache-root", "/tmp/lfs-cloud-cache"])
                .expect("pull command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for pull command") },
            |_| unreachable!("init runner must not be called for pull command"),
            |_| unreachable!("login runner must not be called for pull command"),
            |_| unreachable!("logout runner must not be called for pull command"),
            |_, _| unreachable!("status runner must not be called for pull command"),
            move |command| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(command.cache_root);
                Ok(())
            },
            |_| unreachable!("hydrate runner must not be called for pull command"),
            |_| unreachable!("dehydrate runner must not be called for pull command"),
            |_| unreachable!("gc runner must not be called for pull command"),
            |_, _| unreachable!("migrate runner must not be called for pull command"),
        )
        .await
        .expect("pull dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some(Some(PathBuf::from("/tmp/lfs-cloud-cache")))
        );
    }

    #[tokio::test]
    async fn dispatches_hydrate_with_paths() {
        let cli = Cli::try_parse_from(["lfs-cloud", "hydrate", "asset/model.bin"])
            .expect("hydrate command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for hydrate command") },
            |_| unreachable!("init runner must not be called for hydrate command"),
            |_| unreachable!("login runner must not be called for hydrate command"),
            |_| unreachable!("logout runner must not be called for hydrate command"),
            |_, _| unreachable!("status runner must not be called for hydrate command"),
            |_| unreachable!("pull runner must not be called for hydrate command"),
            move |command| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(command.paths);
                Ok(())
            },
            |_| unreachable!("dehydrate runner must not be called for hydrate command"),
            |_| unreachable!("gc runner must not be called for hydrate command"),
            |_, _| unreachable!("migrate runner must not be called for hydrate command"),
        )
        .await
        .expect("hydrate dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some(vec![PathBuf::from("asset/model.bin")])
        );
    }

    #[tokio::test]
    async fn dispatches_dehydrate_with_paths() {
        let cli = Cli::try_parse_from(["lfs-cloud", "dehydrate", "asset/model.bin"])
            .expect("dehydrate command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for dehydrate command") },
            |_| unreachable!("init runner must not be called for dehydrate command"),
            |_| unreachable!("login runner must not be called for dehydrate command"),
            |_| unreachable!("logout runner must not be called for dehydrate command"),
            |_, _| unreachable!("status runner must not be called for dehydrate command"),
            |_| unreachable!("pull runner must not be called for dehydrate command"),
            |_| unreachable!("hydrate runner must not be called for dehydrate command"),
            move |command| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(command.paths);
                Ok(())
            },
            |_| unreachable!("gc runner must not be called for dehydrate command"),
            |_, _| unreachable!("migrate runner must not be called for dehydrate command"),
        )
        .await
        .expect("dehydrate dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some(vec![PathBuf::from("asset/model.bin")])
        );
    }

    #[tokio::test]
    async fn dispatches_gc_with_dry_run() {
        let cli =
            Cli::try_parse_from(["lfs-cloud", "gc", "--dry-run"]).expect("gc command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for gc command") },
            |_| unreachable!("init runner must not be called for gc command"),
            |_| unreachable!("login runner must not be called for gc command"),
            |_| unreachable!("logout runner must not be called for gc command"),
            |_, _| unreachable!("status runner must not be called for gc command"),
            |_| unreachable!("pull runner must not be called for gc command"),
            |_| unreachable!("hydrate runner must not be called for gc command"),
            |_| unreachable!("dehydrate runner must not be called for gc command"),
            move |command| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(command.dry_run);
                Ok(())
            },
            |_, _| unreachable!("migrate runner must not be called for gc command"),
        )
        .await
        .expect("gc dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some(true)
        );
    }

    #[tokio::test]
    async fn dispatches_migrate_with_global_config() {
        let cli = Cli::try_parse_from([
            "lfs-cloud",
            "--config",
            "lfs-cloud.test.yml",
            "migrate",
            "--server",
            "http://127.0.0.1:8080",
            "--dry-run",
        ])
        .expect("migrate command should parse");
        let captured = Arc::new(Mutex::new(None));
        let captured_for_runner = Arc::clone(&captured);

        dispatch(
            cli,
            |_| async { unreachable!("serve runner must not be called for migrate command") },
            |_| unreachable!("init runner must not be called for migrate command"),
            |_| unreachable!("login runner must not be called for migrate command"),
            |_| unreachable!("logout runner must not be called for migrate command"),
            |_, _| unreachable!("status runner must not be called for migrate command"),
            |_| unreachable!("pull runner must not be called for migrate command"),
            |_| unreachable!("hydrate runner must not be called for migrate command"),
            |_| unreachable!("dehydrate runner must not be called for migrate command"),
            |_| unreachable!("gc runner must not be called for migrate command"),
            move |command, config_path| {
                *captured_for_runner
                    .lock()
                    .expect("capture mutex should lock") =
                    Some((command.server, command.dry_run, config_path));
                Ok(())
            },
        )
        .await
        .expect("migrate dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some((
                "http://127.0.0.1:8080".to_owned(),
                true,
                Some("lfs-cloud.test.yml".into())
            ))
        );
    }

    #[test]
    fn init_writes_lfsconfig_from_current_repo_origin() {
        if !git_is_available() {
            return;
        }

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
                repo.path()
                    .canonicalize()
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
        if !git_is_available() {
            return;
        }

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
                repo.path()
                    .canonicalize()
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
        if !git_is_available() {
            return;
        }

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
                repo.path()
                    .canonicalize()
                    .expect("repo path should canonicalize")
                    .join(".git/config")
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
        if !git_is_available() {
            return;
        }

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
        let config_path = temp.path().join("lfs-cloud.yml");
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
        assert!(rendered.contains("lfs-cloud status"));
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
        if !git_is_available() {
            return;
        }

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repository directory should be created");
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let config_path = temp.path().join("lfs-cloud.yml");
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
        if !git_is_available() {
            return;
        }

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        fs::create_dir_all(&repo).expect("repository directory should be created");
        run_git(&repo, &["init"]);
        run_git(
            &repo,
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let config_path = temp.path().join("lfs-cloud.yml");
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

    #[test]
    fn status_storage_validation_uses_generic_credential_error() {
        let storage = StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig {
            id: "drive-user-a".to_owned(),
            credential_ref: "definitely-missing-status-test-env".to_owned(),
            root_folder_id: "root-folder".to_owned(),
            display_name: None,
        });

        let error = validate_status_storage(&storage)
            .expect_err("missing storage credential should fail validation");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
        let rendered = error.to_string();
        assert!(rendered.contains("drive-user-a"));
        assert!(rendered.contains("credentials_ref"));
        assert!(!rendered.contains("LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL"));
        assert!(!rendered.contains("definitely-missing-status-test-env"));
    }

    #[test]
    fn migrate_dry_run_reports_current_checkout_plan_without_writes() {
        if !git_is_available() {
            return;
        }

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
        let object = object_for_bytes(b"migration object already local");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        write_git_lfs_source_object(&repo, &object, b"migration object already local");
        let config_path = temp.path().join("lfs-cloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
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
        assert!(rendered.contains("lfs-cloud migrate dry-run"));
        assert!(rendered.contains("mode: current-checkout"));
        assert!(rendered.contains("refs scanned: 1"));
        assert!(rendered.contains("current checkout"));
        assert!(rendered.contains("files touched: 1 would update"));
        assert!(rendered.contains(".lfsconfig"));
        assert!(rendered.contains("pointer files: 1"));
        assert!(rendered.contains("objects discovered: 1"));
        assert!(rendered.contains("objects fetched: 0 would fetch, 1 already local"));
        assert!(rendered.contains("objects uploaded: 1 ready to upload, 0 after fetch"));
        assert!(rendered.contains("access checks:"));
        assert!(rendered.contains("target     ok"));
        assert!(rendered.contains("auth       ok"));
        assert!(rendered.contains("storage    ok"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn migrate_dry_run_reports_missing_objects_as_would_fetch_without_fetching() {
        if !git_is_available() {
            return;
        }

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"migration object missing locally");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        let config_path = temp.path().join("lfs-cloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
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
        assert!(rendered.contains("objects uploaded: 0 ready to upload, 1 after fetch"));
        assert!(rendered.contains("target     warning"));
    }

    #[test]
    fn migrate_dry_run_reports_github_purge_support_flow_when_requested() {
        if !git_is_available() {
            return;
        }

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
        let config_path = temp.path().join("lfs-cloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
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
        assert!(rendered.contains("purge manifest: complete object list for GitHub Support"));
        assert!(rendered.contains(object.oid.as_hex()));
        assert!(rendered.contains(&format!("{} bytes", object.size.bytes())));
    }

    #[test]
    fn migrate_dry_run_reports_custom_source_as_unsupported_purge_provider() {
        if !git_is_available() {
            return;
        }

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
        let config_path = temp.path().join("lfs-cloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
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
        if !git_is_available() {
            return;
        }

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
        let config_path = temp.path().join("lfs-cloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
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
        assert!(rendered.contains("objects uploaded: 0 ready to upload, 101 after fetch"));
    }

    #[test]
    fn migrate_dry_run_purge_manifest_lists_all_objects() {
        if !git_is_available() {
            return;
        }

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
        let config_path = temp.path().join("lfs-cloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
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
        let manifest_count = rendered
            .lines()
            .filter(|line| line.starts_with("      sha256:"))
            .count();
        assert!(rendered.contains("... 1 more objects omitted"));
        assert_eq!(main_listing_count, super::MIGRATION_OBJECT_REPORT_LIMIT);
        assert!(rendered.contains("purge manifest: complete object list for GitHub Support"));
        assert_eq!(manifest_count, super::MIGRATION_OBJECT_REPORT_LIMIT + 1);
    }

    #[test]
    fn migrate_without_dry_run_is_rejected_before_writes() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let mut output = Vec::new();

        let error = run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(temp.path().join("cache")),
                refs: Vec::new(),
                all_refs: false,
                dry_run: false,
                purge_source_lfs: false,
            },
            None,
            temp.path(),
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect_err("non-dry-run migrate should be rejected");

        assert!(
            matches!(error, CliError::InvalidArguments { message } if message.contains("--dry-run"))
        );
        assert!(output.is_empty());
    }

    #[test]
    fn pull_fetches_ingests_and_hydrates_current_checkout_pointers() {
        if !git_is_available() {
            return;
        }

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
            Some(repo.canonicalize().expect("repo path should canonicalize"))
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
        assert!(rendered.contains("lfs-cloud pull"));
        assert!(rendered.contains("fetched Git LFS objects"));
        assert!(rendered.contains("tracked paths: 1"));
        assert!(rendered.contains("pointers: 1"));
        assert!(rendered.contains("pulled"));
        assert!(rendered.contains("cached"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn pull_ingests_from_configured_git_lfs_storage_dir() {
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        init_git_repo_with_origin(&repo);
        run_git(
            &repo,
            &["config", "user.email", "lfs-cloud@example.invalid"],
        );
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
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

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
            } if path == missing_file
                .canonicalize()
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
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"tracked pointer object");
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/pointer.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        write_file(&repo.join("asset/hydrated.bin"), b"already hydrated bytes");
        run_git(
            &repo,
            &[
                "add",
                ".gitattributes",
                "asset/pointer.bin",
                "asset/hydrated.bin",
            ],
        );

        let scan = current_checkout_lfs_pointer_scan(&repo)
            .expect("pointer scan should inspect tracked files");

        assert_eq!(scan.tracked_path_count, 2);
        assert_eq!(scan.pointer_files.len(), 1);
        assert_eq!(scan.pointer_files[0].object, object);
    }

    #[cfg(unix)]
    #[test]
    fn current_checkout_pointer_scan_accepts_non_utf8_tracked_paths() {
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

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
        assert!(rendered.contains("copied") || rendered.contains("copy-on-write-attempted"));
        assert!(rendered.contains("asset/model.bin"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn dehydrate_caches_clean_file_and_writes_pointer() {
        if !git_is_available() {
            return;
        }

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"hydrated model bytes";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
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
        assert!(rendered.contains("asset/model.bin"));
        assert!(rendered.contains(object.oid.as_hex()));

        let mut gc_output = Vec::new();
        run_gc_from_dir(
            GcCommand {
                cache_root: Some(layout.root().to_path_buf()),
                dry_run: false,
            },
            &repo,
            &mut gc_output,
        )
        .expect("gc should retain the dehydrated pointer's cached bytes");
        assert!(layout.object_path(&object).exists());
    }

    #[test]
    fn dehydrate_accepts_existing_pointer_as_idempotent() {
        if !git_is_available() {
            return;
        }

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let object = object_for_bytes(b"already dehydrated bytes");
        let pointer = LfsPointer::new(object.clone()).to_pointer_file();
        write_file(&worktree_file, pointer.as_bytes());
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
    fn hydrate_rejects_non_pointer_worktree_content_with_local_cache_error() {
        if !git_is_available() {
            return;
        }

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
            } if path == worktree_file
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn hydrate_reports_missing_cache_object_as_local_cache_error() {
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

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
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(keep_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        let mut output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: false,
            },
            &repo,
            &mut output,
        )
        .expect("gc should remove unreferenced cache objects");

        assert!(layout.object_path(&keep_object).exists());
        assert!(!layout.object_path(&remove_object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfs-cloud gc"));
        assert!(rendered.contains("worktrees: 1 active, 0 pruned"));
        assert!(rendered.contains("objects: 1 retained, 1 removed, 0 skipped"));
        assert!(rendered.contains(remove_object.oid.as_hex()));
    }

    #[test]
    fn gc_dry_run_reports_without_removing_cache_objects() {
        if !git_is_available() {
            return;
        }

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
            },
            &repo,
            &mut output,
        )
        .expect("gc dry-run should report unreferenced cache objects");

        assert!(layout.object_path(&object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("objects: 0 retained, 1 would remove, 0 skipped"));
        assert!(rendered.contains("would remove"));
        assert!(rendered.contains(object.oid.as_hex()));
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
            },
            &start_dir,
            &mut output,
        )
        .expect("gc should run without a current Git worktree");

        assert!(!layout.object_path(&object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("worktrees: 0 active, 0 pruned"));
        assert!(rendered.contains("objects: 0 retained, 1 removed, 0 skipped"));
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
    fn login_url_preserves_server_base_path() {
        assert_eq!(
            login_url_for_server("https://lfs.example.com/custom/base")
                .expect("login URL should resolve"),
            "https://lfs.example.com/custom/base/auth/github/login"
        );
    }

    #[test]
    fn login_url_rejects_unsafe_server_url_components() {
        for server_url in [
            "https://lfs.example.com/custom/base/",
            "https://user:secret@lfs.example.com/custom/base",
            "https://lfs.example.com/custom/base?token=secret",
            "https://lfs.example.com/custom/base#fragment",
        ] {
            let error =
                login_url_for_server(server_url).expect_err("unsafe server URL should be rejected");
            assert!(
                matches!(error, CliError::InvalidArguments { .. }),
                "unexpected error for {server_url}: {error}"
            );
        }
    }

    #[test]
    fn login_opens_browser_and_stores_local_lfs_token_for_current_repo() {
        if !git_is_available() {
            return;
        }

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let opened_url = Arc::new(Mutex::new(None));
        let approved = Arc::new(Mutex::new(None));
        let opened_url_for_runner = Arc::clone(&opened_url);
        let approved_for_runner = Arc::clone(&approved);
        let mut input = io::Cursor::new(b" \tlocal-lfs-token \r\n".to_vec());
        let mut output = Vec::new();

        run_login_from_dir(
            LoginCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                no_open: false,
            },
            repo.path(),
            &mut input,
            &mut output,
            move |url| {
                *opened_url_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(url.to_owned());
                Ok(())
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
            *opened_url.lock().expect("capture mutex should lock"),
            Some("http://127.0.0.1:8080/auth/github/login".to_owned())
        );
        assert_eq!(
            *approved.lock().expect("capture mutex should lock"),
            Some((
                "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs".to_owned(),
                "lfs-cloud".to_owned(),
                "local-lfs-token".to_owned(),
            ))
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("authorize LFS Cloud with GitHub:"));
        assert!(rendered.contains("opened browser for GitHub OAuth"));
        assert!(rendered.contains("stored local LFS credential"));
        assert!(rendered.contains("username: lfs-cloud"));
        assert!(!rendered.contains("local-lfs-token"));
    }

    #[test]
    fn login_no_open_skips_browser_but_stores_token() {
        if !git_is_available() {
            return;
        }

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        run_git(
            repo.path(),
            &["remote", "add", "origin", "git@github.com:owner/repo.git"],
        );
        let approved = Arc::new(Mutex::new(None));
        let approved_for_runner = Arc::clone(&approved);
        let mut input = io::Cursor::new(b"local-lfs-token\n".to_vec());
        let mut output = Vec::new();

        run_login_from_dir(
            LoginCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                no_open: true,
            },
            repo.path(),
            &mut input,
            &mut output,
            |_| panic!("browser opener should not be called with --no-open"),
            move |approval: GitCredentialApproval| {
                *approved_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(approval.lfs_url().to_string());
                Ok(())
            },
        )
        .expect("login should complete");

        assert_eq!(
            *approved.lock().expect("capture mutex should lock"),
            Some("http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs".to_owned())
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("browser open skipped"));
    }

    #[test]
    fn logout_revokes_remote_session_before_erasing_local_credential() {
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

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
        if !git_is_available() {
            return;
        }

        let repo = TempDir::new().expect("temporary repository should be created");
        run_git(repo.path(), &["init"]);
        let mut input = io::Cursor::new(b"local-lfs-token\n".to_vec());
        let mut output = Vec::new();

        let error = run_login_from_dir(
            LoginCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                no_open: true,
            },
            repo.path(),
            &mut input,
            &mut output,
            |_| panic!("browser opener should not be called without a remote"),
            |_| panic!("credential approval should not run without a remote"),
        )
        .expect_err("missing origin remote should fail before login");

        assert!(matches!(
            error,
            CliError::InvalidArguments { message }
                if message.contains("requires an origin remote")
        ));
    }

    #[test]
    fn browser_stderr_sanitizer_cleans_multiline_output() {
        assert_eq!(
            sanitize_browser_stderr(b"first line\nsecond line\r\n").as_str(),
            "first line second line"
        );
        assert_eq!(sanitize_browser_stderr(b"\n").as_str(), "<no stderr>");
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
    oauth_client_id: client-id
    oauth_client_secret: client-secret

storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: drive-user-a
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

    fn git_is_available() -> bool {
        ProcessCommand::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|output| output.status.success())
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
