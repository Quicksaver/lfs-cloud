//! CLI argument definitions, parsing, and command dispatch.

use super::*;

#[derive(Debug, Parser)]
#[command(name = "lfscloud", version, about, propagate_version = true)]
#[command(arg_required_else_help = true)]
pub(super) struct Cli {
    /// Server config path to load (defaults to HOME/lfscloud.yml).
    #[arg(long, global = true, value_name = "PATH")]
    pub(super) config: Option<PathBuf>,

    /// Tracing filter or log level to use instead of RUST_LOG.
    #[arg(long, global = true, value_name = "FILTER")]
    pub(super) log_level: Option<String>,

    #[command(subcommand)]
    pub(super) command: Command,
}

#[derive(Debug, Subcommand)]
pub(super) enum Command {
    /// Manage repository-provider and storage-provider configuration.
    Config(ConfigCommand),
    /// Manage repositories served by this LFS Cloud instance.
    Repository(RepositoryCommand),
    /// Manage server-side LFS sessions and their encryption key.
    Sessions(SessionsCommand),
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

#[derive(Debug, Eq, PartialEq)]
pub(super) enum ConfigurationCommand {
    Config(ConfigCommand),
    Repository(RepositoryCommand),
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct ConfigCommand {
    #[command(subcommand)]
    pub(super) resource: ConfigResourceCommand,
}

#[derive(Debug, Subcommand, Eq, PartialEq)]
pub(super) enum ConfigResourceCommand {
    /// Manage repository-provider configuration.
    Repository(ConfigRepositoryCommand),
    /// Manage storage-provider configuration.
    Storage(ConfigStorageCommand),
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct ConfigRepositoryCommand {
    #[command(subcommand)]
    pub(super) action: ConfigRepositoryAction,
}

#[derive(Debug, Subcommand, Eq, PartialEq)]
pub(super) enum ConfigRepositoryAction {
    /// Add or update a repository provider.
    Add(RepositoryProviderAddCommand),
    /// Remove a repository provider.
    Remove(ConfigEntryRemoveCommand),
    /// List configured repository providers.
    List,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum RepositoryProviderKind {
    /// GitHub repository provider.
    #[value(name = "github")]
    GitHub,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct RepositoryProviderAddCommand {
    /// Configured repository-provider ID.
    #[arg(long)]
    pub(super) id: Option<String>,

    /// Repository-provider type.
    #[arg(long = "type", value_enum)]
    pub(super) provider_type: Option<RepositoryProviderKind>,

    /// GitHub REST API base URL.
    #[arg(long, value_name = "URL")]
    pub(super) api_url: Option<String>,

    /// Remove a configured GitHub REST API override.
    #[arg(long, conflicts_with = "api_url")]
    pub(super) clear_api_url: bool,

    /// Deprecated server-session encryption fallback.
    #[arg(long, value_name = "TOKEN_OR_ENV_REFERENCE", hide = true)]
    pub(super) personal_access_token: Option<String>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct ConfigStorageCommand {
    #[command(subcommand)]
    pub(super) action: ConfigStorageAction,
}

#[derive(Debug, Subcommand, Eq, PartialEq)]
pub(super) enum ConfigStorageAction {
    /// Add or update a storage provider.
    Add(StorageProviderAddCommand),
    /// Remove a storage provider.
    Remove(ConfigEntryRemoveCommand),
    /// List configured storage providers.
    List,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum StorageProviderKind {
    /// Google Drive storage provider.
    #[value(name = "google_drive", alias = "google-drive")]
    GoogleDrive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub(super) enum StorageCredentialKind {
    /// Google Cloud CLI Application Default Credentials.
    #[value(name = "gcloud")]
    Gcloud,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct StorageProviderAddCommand {
    /// Configured storage-provider ID.
    #[arg(long)]
    pub(super) id: Option<String>,

    /// Storage-provider type.
    #[arg(long = "type", value_enum)]
    pub(super) provider_type: Option<StorageProviderKind>,

    /// Storage credential type.
    #[arg(long, value_enum)]
    pub(super) credentials_type: Option<StorageCredentialKind>,

    /// Isolated gcloud configuration directory containing ADC.
    #[arg(long, value_name = "PATH")]
    pub(super) config_dir: Option<PathBuf>,

    /// Desktop OAuth client JSON used to authorize isolated Google Drive ADC.
    #[arg(long, value_name = "PATH")]
    pub(super) client_secret_file: Option<PathBuf>,

    /// Google Cloud CLI executable name or path.
    #[arg(long, value_name = "PATH")]
    pub(super) executable: Option<PathBuf>,

    /// Google Drive root folder ID.
    #[arg(long)]
    pub(super) root_folder_id: Option<String>,

    /// Optional operator-facing storage label.
    #[arg(long)]
    pub(super) display_name: Option<String>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct ConfigEntryRemoveCommand {
    /// Configured entry ID; omit to select an existing entry interactively.
    #[arg(long)]
    pub(super) id: Option<String>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct RepositoryCommand {
    #[command(subcommand)]
    pub(super) action: RepositoryAction,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct SessionsCommand {
    #[command(subcommand)]
    pub(super) action: SessionsAction,
}

#[derive(Debug, Subcommand, Eq, PartialEq)]
pub(super) enum SessionsAction {
    /// Generate a new managed encryption key and invalidate current sessions.
    GenerateKey,
}

#[derive(Debug, Subcommand, Eq, PartialEq)]
pub(super) enum RepositoryAction {
    /// Add or update a served repository mapping.
    Add(RepositoryAddCommand),
    /// Remove a served repository mapping.
    Remove(ConfigEntryRemoveCommand),
    /// List served repository mappings.
    List,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct RepositoryAddCommand {
    /// Stable repository mapping ID.
    #[arg(long)]
    pub(super) id: Option<String>,

    /// Configured repository-provider ID.
    #[arg(long)]
    pub(super) repo_provider: Option<String>,

    /// Repository host, such as github.com.
    #[arg(long)]
    pub(super) host: Option<String>,

    /// Repository owner or namespace.
    #[arg(long)]
    pub(super) owner: Option<String>,

    /// Repository name without the .git suffix.
    #[arg(long)]
    pub(super) name: Option<String>,

    /// Provider-stable repository ID.
    #[arg(long)]
    pub(super) provider_repository_id: Option<String>,

    /// Configured storage-provider ID.
    #[arg(long)]
    pub(super) storage_provider: Option<String>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct ServeCommand {
    /// Host or interface address to bind.
    #[arg(long)]
    pub(super) host: Option<String>,

    /// TCP port to bind.
    #[arg(long)]
    pub(super) port: Option<u16>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct LoginCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    pub(super) server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    pub(super) allow_insecure_http: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct LogoutCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    pub(super) server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    pub(super) allow_insecure_http: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct InitCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    pub(super) server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    pub(super) allow_insecure_http: bool,

    /// Write lfs.url to local Git config instead of committed .lfsconfig.
    #[arg(long)]
    pub(super) local: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct StatusCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    pub(super) server: Option<String>,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    pub(super) allow_insecure_http: bool,

    /// Local cache root to inspect instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    pub(super) cache_root: Option<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct PullCommand {
    /// Local cache root to use instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    pub(super) cache_root: Option<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct HydrateCommand {
    /// Local cache root to use instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    pub(super) cache_root: Option<PathBuf>,

    /// Git LFS pointer files to replace with cached object bytes.
    #[arg(value_name = "PATH", required = true)]
    pub(super) paths: Vec<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct DehydrateCommand {
    /// Local cache root to use instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    pub(super) cache_root: Option<PathBuf>,

    /// Clean hydrated files to replace with Git LFS pointers.
    #[arg(value_name = "PATH", required = true)]
    pub(super) paths: Vec<PathBuf>,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct GcCommand {
    /// Local cache root to clean instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    pub(super) cache_root: Option<PathBuf>,

    /// Report objects and worktree registrations that would be removed.
    #[arg(long)]
    pub(super) dry_run: bool,

    /// Permanently forget unavailable worktrees before removing objects.
    #[arg(long)]
    pub(super) prune_unavailable_worktrees: bool,
}

#[derive(Debug, Args, Eq, PartialEq)]
pub(super) struct MigrateCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    pub(super) server: String,

    /// Allow plaintext HTTP to a non-loopback server on a trusted network.
    #[arg(long)]
    pub(super) allow_insecure_http: bool,

    /// Local cache root to inspect instead of ~/.lfscloud.
    #[arg(long, value_name = "PATH")]
    pub(super) cache_root: Option<PathBuf>,

    /// Git remote that owns the source repository and source LFS objects.
    #[arg(long, value_name = "REMOTE", default_value = "origin")]
    pub(super) source_remote: String,

    /// Confirm migration between different source and target repositories.
    #[arg(long)]
    pub(super) allow_cross_remote: bool,

    /// Scan one selected branch, tag, or ref from a non-shallow repository.
    /// Can be repeated.
    #[arg(long = "ref", value_name = "REF", conflicts_with = "all_refs")]
    pub(super) refs: Vec<String>,

    /// Scan local branches, tags, and source refs from a non-shallow repository.
    #[arg(long, conflicts_with = "refs")]
    pub(super) all_refs: bool,

    /// Report the migration plan without fetching, uploading, or writing config.
    #[arg(long)]
    pub(super) dry_run: bool,

    /// Include source-LFS purge guidance in the migration report.
    ///
    /// GitHub does not expose a normal self-service API for arbitrary LFS
    /// object deletion, so this flag never mutates the source. Dry runs report
    /// planning guidance; completed executions point at the durable verified
    /// receipt for the provider's supported cleanup process.
    #[arg(long)]
    pub(super) purge_source_lfs: bool,
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
        run_configuration_to_stdio,
        run_sessions_to_stdio,
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
pub(super) async fn dispatch<F, Fut, C, X, I, L, O, S, P, H, D, G, M>(
    cli: Cli,
    serve: F,
    configure: C,
    sessions: X,
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
    C: FnOnce(ConfigurationCommand, Option<PathBuf>) -> anyhow::Result<()>,
    X: FnOnce(SessionsCommand, Option<PathBuf>) -> anyhow::Result<()>,
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
        Command::Config(command) => configure(ConfigurationCommand::Config(command), cli.config)
            .context("failed to edit lfscloud configuration"),
        Command::Repository(command) => {
            configure(ConfigurationCommand::Repository(command), cli.config)
                .context("failed to edit lfscloud repository mappings")
        }
        Command::Sessions(command) => {
            sessions(command, cli.config).context("failed to manage lfscloud sessions")
        }
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

pub(super) fn tracing_config(cli: &Cli) -> TracingConfig {
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
        assert!(
            config
                .get_help()
                .expect("config flag should have help text")
                .to_string()
                .contains("HOME/lfscloud.yml")
        );
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
    fn config_repository_add_accepts_every_config_field() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "--config",
            "custom-lfscloud.yml",
            "config",
            "repository",
            "add",
            "--id",
            "github-main",
            "--type",
            "github",
            "--api-url",
            "https://api.github.com",
            "--personal-access-token",
            "${LFS_CLOUD_GITHUB_PAT}",
        ])
        .expect("repository-provider add command should parse");

        let super::Command::Config(ConfigCommand {
            resource:
                ConfigResourceCommand::Repository(ConfigRepositoryCommand {
                    action: ConfigRepositoryAction::Add(command),
                }),
        }) = cli.command
        else {
            panic!("config repository add command should parse");
        };

        assert_eq!(cli.config, Some("custom-lfscloud.yml".into()));
        assert_eq!(command.id.as_deref(), Some("github-main"));
        assert_eq!(command.provider_type, Some(RepositoryProviderKind::GitHub));
        assert_eq!(command.api_url.as_deref(), Some("https://api.github.com"));
        assert!(!command.clear_api_url);
        assert_eq!(
            command.personal_access_token.as_deref(),
            Some("${LFS_CLOUD_GITHUB_PAT}")
        );
    }

    #[test]
    fn config_storage_add_accepts_every_config_field() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "config",
            "storage",
            "add",
            "--id",
            "drive-main",
            "--type",
            "google-drive",
            "--credentials-type",
            "gcloud",
            "--config-dir",
            "/var/lib/lfscloud/gcloud",
            "--client-secret-file",
            "/tmp/client_secret.json",
            "--executable",
            "/usr/local/bin/gcloud",
            "--root-folder-id",
            "drive-root",
            "--display-name",
            "Main Drive",
        ])
        .expect("storage-provider add command should parse");

        let super::Command::Config(ConfigCommand {
            resource:
                ConfigResourceCommand::Storage(ConfigStorageCommand {
                    action: ConfigStorageAction::Add(command),
                }),
        }) = cli.command
        else {
            panic!("config storage add command should parse");
        };

        assert_eq!(command.id.as_deref(), Some("drive-main"));
        assert_eq!(
            command.provider_type,
            Some(StorageProviderKind::GoogleDrive)
        );
        assert_eq!(
            command.credentials_type,
            Some(StorageCredentialKind::Gcloud)
        );
        assert_eq!(
            command.config_dir.as_deref(),
            Some(Path::new("/var/lib/lfscloud/gcloud"))
        );
        assert_eq!(
            command.client_secret_file.as_deref(),
            Some(Path::new("/tmp/client_secret.json"))
        );
        assert_eq!(
            command.executable.as_deref(),
            Some(Path::new("/usr/local/bin/gcloud"))
        );
        assert_eq!(command.root_folder_id.as_deref(), Some("drive-root"));
        assert_eq!(command.display_name.as_deref(), Some("Main Drive"));
    }

    #[test]
    fn repository_add_and_all_remove_and_list_commands_parse() {
        let repository = Cli::try_parse_from([
            "lfscloud",
            "repository",
            "add",
            "--id",
            "github-main:owner/repo",
            "--repo-provider",
            "github-main",
            "--host",
            "github.com",
            "--owner",
            "owner",
            "--name",
            "repo",
            "--provider-repository-id",
            "123456789",
            "--storage-provider",
            "drive-main",
        ])
        .expect("repository add command should parse");
        let super::Command::Repository(RepositoryCommand {
            action: RepositoryAction::Add(command),
        }) = repository.command
        else {
            panic!("repository add command should parse");
        };
        assert_eq!(command.id.as_deref(), Some("github-main:owner/repo"));
        assert_eq!(command.repo_provider.as_deref(), Some("github-main"));
        assert_eq!(command.host.as_deref(), Some("github.com"));
        assert_eq!(command.owner.as_deref(), Some("owner"));
        assert_eq!(command.name.as_deref(), Some("repo"));
        assert_eq!(command.provider_repository_id.as_deref(), Some("123456789"));
        assert_eq!(command.storage_provider.as_deref(), Some("drive-main"));

        for args in [
            vec![
                "lfscloud",
                "config",
                "repository",
                "remove",
                "--id",
                "github-main",
            ],
            vec!["lfscloud", "config", "repository", "list"],
            vec![
                "lfscloud",
                "config",
                "storage",
                "remove",
                "--id",
                "drive-main",
            ],
            vec!["lfscloud", "config", "storage", "list"],
            vec![
                "lfscloud",
                "repository",
                "remove",
                "--id",
                "github-main:owner/repo",
            ],
            vec!["lfscloud", "repository", "list"],
        ] {
            Cli::try_parse_from(&args)
                .unwrap_or_else(|error| panic!("{args:?} should parse: {error}"));
        }
    }

    #[test]
    fn add_commands_accept_zero_flags_for_interactive_mode() {
        for args in [
            vec!["lfscloud", "config", "repository", "add"],
            vec!["lfscloud", "config", "storage", "add"],
            vec!["lfscloud", "repository", "add"],
        ] {
            Cli::try_parse_from(&args)
                .unwrap_or_else(|error| panic!("{args:?} should parse: {error}"));
        }
    }

    #[test]
    fn remove_commands_accept_zero_flags_for_interactive_mode() {
        for args in [
            vec!["lfscloud", "config", "repository", "remove"],
            vec!["lfscloud", "config", "storage", "remove"],
            vec!["lfscloud", "repository", "remove"],
        ] {
            Cli::try_parse_from(&args)
                .unwrap_or_else(|error| panic!("{args:?} should parse: {error}"));
        }
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
    fn sessions_generate_key_command_parses_with_global_config() {
        let cli = Cli::try_parse_from([
            "lfscloud",
            "--config",
            "custom-lfscloud.yml",
            "sessions",
            "generate-key",
        ])
        .expect("sessions generate-key command should parse");

        assert_eq!(cli.config, Some("custom-lfscloud.yml".into()));
        assert!(matches!(
            cli.command,
            super::Command::Sessions(SessionsCommand {
                action: SessionsAction::GenerateKey
            })
        ));
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
        Configuration(ConfigurationCommand, Option<PathBuf>),
        Sessions(SessionsCommand, Option<PathBuf>),
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
                    "config",
                    "repository",
                    "list",
                ],
                Invoked::Configuration(
                    ConfigurationCommand::Config(ConfigCommand {
                        resource: ConfigResourceCommand::Repository(ConfigRepositoryCommand {
                            action: ConfigRepositoryAction::List,
                        }),
                    }),
                    Some("lfscloud.test.yml".into()),
                ),
            ),
            (
                vec!["lfscloud", "repository", "list"],
                Invoked::Configuration(
                    ConfigurationCommand::Repository(RepositoryCommand {
                        action: RepositoryAction::List,
                    }),
                    None,
                ),
            ),
            (
                vec![
                    "lfscloud",
                    "--config",
                    "lfscloud.test.yml",
                    "sessions",
                    "generate-key",
                ],
                Invoked::Sessions(
                    SessionsCommand {
                        action: SessionsAction::GenerateKey,
                    },
                    Some("lfscloud.test.yml".into()),
                ),
            ),
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
                |command, config| {
                    record_invocation(recorder, Invoked::Configuration(command, config));
                    Ok(())
                },
                |command, config| {
                    record_invocation(recorder, Invoked::Sessions(command, config));
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
}
