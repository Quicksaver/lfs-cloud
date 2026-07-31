//! Repository Git LFS endpoint initialization.

use super::*;

fn run_init<W>(command: InitCommand, output: &mut W) -> anyhow::Result<()>
where
    W: Write,
{
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;

    run_init_from_dir(command, &current_dir, output)
}

pub(super) fn run_init_to_stdout(command: InitCommand) -> anyhow::Result<()> {
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
}
