//! Repository, server, authentication, storage, and cache readiness reporting.

use super::*;

pub(super) const STATUS_SERVER_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
pub(super) fn run_status_to_stdout(
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
pub(super) enum StatusLevel {
    Ok,
    Warning,
    Error,
}

impl StatusLevel {
    pub(super) fn label(self) -> &'static str {
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

pub(super) fn probe_server_reachable(server_url: &str) -> CliResult<()> {
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

pub(super) fn validate_status_storage(storage: &StorageProviderConfig) -> CliResult<()> {
    storage
        .validate_local_readiness()
        .map_err(|message| CliError::InvalidArguments { message })
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
}
