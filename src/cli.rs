//! Command-line parsing and dispatch for LFS Cloud.
//!
//! This module keeps the binary target small while making CLI behavior
//! testable without binding sockets. The process entry point owns global
//! tracing initialization, while parser and dispatch helpers stay side-effect
//! free for focused tests.

use std::{
    future::Future,
    io::{self, Write},
    path::{Path, PathBuf},
};

use anyhow::Context;
use clap::{Args, Parser, Subcommand};

use crate::git::redacted_url_for_display;
use crate::{
    GitLfsConfigChange, GitLfsConfigTarget, GitRepository, LfsInitRoute, ServeOptions,
    TracingConfig, init_tracing,
};

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
    /// Resolve the Git LFS URL for the current repository.
    Init(InitCommand),
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
struct InitCommand {
    /// Base URL of the running LFS Cloud server.
    #[arg(long, value_name = "URL")]
    server: String,

    /// Write lfs.url to local Git config instead of committed .lfsconfig.
    #[arg(long)]
    local: bool,
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
    dispatch(cli, crate::serve, run_init_to_stdout).await
}

async fn dispatch<F, Fut, I>(cli: Cli, serve: F, init: I) -> anyhow::Result<()>
where
    F: FnOnce(ServeOptions) -> Fut,
    Fut: Future<Output = crate::ServerResult<()>>,
    I: FnOnce(InitCommand) -> anyhow::Result<()>,
{
    // Keep command execution injectable only at the command boundary; each new
    // subcommand should add its own runner here rather than hiding side effects
    // in parser code.
    match cli.command {
        Command::Serve(command) => serve(command.serve_options(cli.config))
            .await
            .context("failed to run lfs-cloud server"),
        Command::Init(command) => init(command).context("failed to resolve lfs-cloud init route"),
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
    let route = LfsInitRoute::resolve(&command.server, &repository.remote)
        .context("failed to build Git LFS URL")?;
    let change = repository
        .write_lfs_url(command.target(), &route.lfs_url)
        .context("failed to write Git LFS config")?;

    write_init_change(output, &change).context("failed to write init summary")
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
    use std::{
        fs,
        path::Path,
        process::Command as ProcessCommand,
        sync::{Arc, Mutex},
    };

    use clap::{CommandFactory, Parser};
    use tempfile::TempDir;

    use super::{Cli, InitCommand, dispatch, run_init_from_dir, tracing_config, write_init_change};
    use crate::{
        DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, GitLfsConfigChange, GitLfsConfigTarget,
        ServeOptions,
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
        ])
        .expect("init command should parse");

        let super::Command::Init(command) = cli.command else {
            panic!("init subcommand should parse");
        };

        assert_eq!(cli.config, Some("custom-lfs-cloud.yml".into()));
        assert_eq!(command.server, "http://127.0.0.1:8080");
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
        )
        .await
        .expect("init dispatch should succeed");

        assert_eq!(
            *captured.lock().expect("capture mutex should lock"),
            Some("http://127.0.0.1:8080".to_owned())
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
