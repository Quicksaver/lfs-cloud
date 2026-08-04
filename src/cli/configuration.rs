//! Editing and listing the server's provider and repository configuration.

use super::*;

const MAX_CONFIG_PROMPT_INPUT_BYTES: usize = 16 * 1024;

pub(super) fn run_configuration_to_stdio(
    command: ConfigurationCommand,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();

    if stdin.is_terminal() {
        run_configuration_with_input(command, config_path, &mut input, &mut output, |_| {
            read_hidden_config_value_from_terminal()
        })
    } else {
        run_configuration_with_input(
            command,
            config_path,
            &mut input,
            &mut output,
            read_config_prompt_value,
        )
    }
    .map_err(anyhow::Error::from)
}

fn run_configuration_with_input<R, W, S>(
    command: ConfigurationCommand,
    config_path: Option<PathBuf>,
    input: &mut R,
    output: &mut W,
    mut read_secret: S,
) -> CliResult<()>
where
    R: BufRead,
    W: Write,
    S: FnMut(&mut R) -> CliResult<String>,
{
    let config_path = match config_path {
        Some(config_path) => config_path,
        None => ServerConfig::default_path()?,
    };
    let mut config = EditableServerConfig::load(config_path)?;

    match command {
        ConfigurationCommand::Config(ConfigCommand {
            resource: ConfigResourceCommand::Repository(ConfigRepositoryCommand { action }),
        }) => match action {
            ConfigRepositoryAction::Add(command) => {
                let values = if command.is_interactive() {
                    prompt_repository_provider(&config, input, output, &mut read_secret)?
                } else {
                    command.into_values()?
                };
                let id = values.id.clone();
                let outcome = config.upsert_repository_provider(values)?;
                save_and_report_edit(&config, outcome, "repository provider", &id, output)
            }
            ConfigRepositoryAction::Remove(command) => {
                let outcome = config.remove_repository_provider(&command.id)?;
                save_and_report_remove(&config, outcome, "repository provider", &command.id, output)
            }
            ConfigRepositoryAction::List => write_repository_provider_list(&config, output),
        },
        ConfigurationCommand::Config(ConfigCommand {
            resource: ConfigResourceCommand::Storage(ConfigStorageCommand { action }),
        }) => match action {
            ConfigStorageAction::Add(command) => {
                let values = if command.is_interactive() {
                    prompt_storage_provider(&config, input, output)?
                } else {
                    command.into_values()?
                };
                let id = values.id.clone();
                let outcome = config.upsert_storage_provider(values)?;
                save_and_report_edit(&config, outcome, "storage provider", &id, output)
            }
            ConfigStorageAction::Remove(command) => {
                let outcome = config.remove_storage_provider(&command.id)?;
                save_and_report_remove(&config, outcome, "storage provider", &command.id, output)
            }
            ConfigStorageAction::List => write_storage_provider_list(&config, output),
        },
        ConfigurationCommand::Repository(RepositoryCommand { action }) => match action {
            RepositoryAction::Add(command) => {
                let values = if command.is_interactive() {
                    prompt_repository(&config, input, output)?
                } else {
                    command.into_values()?
                };
                let id = values.id.clone();
                let outcome = config.upsert_repository(values)?;
                save_and_report_edit(&config, outcome, "repository", &id, output)
            }
            RepositoryAction::Remove(command) => {
                let outcome = config.remove_repository(&command.id)?;
                save_and_report_remove(&config, outcome, "repository", &command.id, output)
            }
            RepositoryAction::List => write_repository_list(&config, output),
        },
    }
}

impl RepositoryProviderAddCommand {
    fn is_interactive(&self) -> bool {
        self.id.is_none()
            && self.provider_type.is_none()
            && self.api_url.is_none()
            && !self.clear_api_url
            && self.personal_access_token.is_none()
    }

    fn into_values(self) -> CliResult<RepositoryProviderValues> {
        Ok(RepositoryProviderValues {
            id: required_flag(self.id, "--id")?,
            provider_type: self.provider_type.map(|kind| match kind {
                RepositoryProviderKind::GitHub => "github".to_owned(),
            }),
            api_url: self.api_url,
            clear_api_url: self.clear_api_url,
            personal_access_token: self.personal_access_token,
        })
    }
}

impl StorageProviderAddCommand {
    fn is_interactive(&self) -> bool {
        self.id.is_none()
            && self.provider_type.is_none()
            && self.credentials_type.is_none()
            && self.config_dir.is_none()
            && self.executable.is_none()
            && self.root_folder_id.is_none()
            && self.display_name.is_none()
    }

    fn into_values(self) -> CliResult<StorageProviderValues> {
        Ok(StorageProviderValues {
            id: required_flag(self.id, "--id")?,
            provider_type: self.provider_type.map(|kind| match kind {
                StorageProviderKind::GoogleDrive => "google_drive".to_owned(),
            }),
            credentials_type: self.credentials_type.map(|kind| match kind {
                StorageCredentialKind::Gcloud => "gcloud".to_owned(),
            }),
            config_dir: optional_path_string(self.config_dir, "--config-dir")?,
            executable: optional_path_string(self.executable, "--executable")?,
            root_folder_id: self.root_folder_id,
            display_name: self.display_name,
        })
    }
}

impl RepositoryAddCommand {
    fn is_interactive(&self) -> bool {
        self.id.is_none()
            && self.repo_provider.is_none()
            && self.host.is_none()
            && self.owner.is_none()
            && self.name.is_none()
            && self.provider_repository_id.is_none()
            && self.storage_provider.is_none()
    }

    fn into_values(self) -> CliResult<RepositoryValues> {
        Ok(RepositoryValues {
            id: required_flag(self.id, "--id")?,
            repo_provider: self.repo_provider,
            host: self.host,
            owner: self.owner,
            name: self.name,
            provider_repository_id: self.provider_repository_id,
            storage_provider: self.storage_provider,
        })
    }
}

fn prompt_repository_provider<R, W, S>(
    config: &EditableServerConfig,
    input: &mut R,
    output: &mut W,
    _read_secret: &mut S,
) -> CliResult<RepositoryProviderValues>
where
    R: BufRead,
    W: Write,
    S: FnMut(&mut R) -> CliResult<String>,
{
    let id = prompt_value(input, output, "Repository provider ID", None, true)?
        .expect("required prompt returns a value");
    let existing = config.repository_provider(&id)?.unwrap_or_default();
    let provider_type = prompt_value(
        input,
        output,
        "Type",
        existing.provider_type.as_deref().or(Some("github")),
        true,
    )?;
    if provider_type.as_deref() != Some("github") {
        return Err(CliError::InvalidArguments {
            message: "repository provider type must be github".to_owned(),
        });
    }
    let api_url_label = if existing.api_url.is_some() {
        "GitHub API URL override (blank to retain; enter 'default' to clear)"
    } else {
        "GitHub API URL override (blank for https://api.github.com)"
    };
    let mut api_url = prompt_value(
        input,
        output,
        api_url_label,
        existing.api_url.as_deref(),
        false,
    )?;
    let clear_api_url = api_url.as_deref() == Some("default");
    if clear_api_url {
        api_url = None;
    }
    Ok(RepositoryProviderValues {
        id,
        provider_type,
        api_url,
        clear_api_url,
        personal_access_token: existing.personal_access_token,
    })
}

fn prompt_storage_provider<R, W>(
    config: &EditableServerConfig,
    input: &mut R,
    output: &mut W,
) -> CliResult<StorageProviderValues>
where
    R: BufRead,
    W: Write,
{
    let id = prompt_value(input, output, "Storage provider ID", None, true)?
        .expect("required prompt returns a value");
    let existing = config.storage_provider(&id)?.unwrap_or_default();
    let provider_type = prompt_value(
        input,
        output,
        "Type",
        existing.provider_type.as_deref().or(Some("google_drive")),
        true,
    )?;
    if provider_type.as_deref() != Some("google_drive") {
        return Err(CliError::InvalidArguments {
            message: "storage provider type must be google_drive".to_owned(),
        });
    }
    let credentials_type = prompt_value(
        input,
        output,
        "Credentials type",
        existing.credentials_type.as_deref().or(Some("gcloud")),
        true,
    )?;
    if credentials_type.as_deref() != Some("gcloud") {
        return Err(CliError::InvalidArguments {
            message: "storage credentials type must be gcloud".to_owned(),
        });
    }
    let config_dir = prompt_value(
        input,
        output,
        "gcloud config directory",
        existing.config_dir.as_deref(),
        true,
    )?;
    let executable = prompt_value(
        input,
        output,
        "gcloud executable",
        existing
            .executable
            .as_deref()
            .or(Some(default_gcloud_executable())),
        true,
    )?;
    let root_folder_id = prompt_value(
        input,
        output,
        "Google Drive root folder ID",
        existing.root_folder_id.as_deref(),
        true,
    )?;
    let display_name = prompt_value(
        input,
        output,
        "Display name",
        existing.display_name.as_deref(),
        false,
    )?;

    Ok(StorageProviderValues {
        id,
        provider_type,
        credentials_type,
        config_dir,
        executable,
        root_folder_id,
        display_name,
    })
}

fn prompt_repository<R, W>(
    config: &EditableServerConfig,
    input: &mut R,
    output: &mut W,
) -> CliResult<RepositoryValues>
where
    R: BufRead,
    W: Write,
{
    let id = prompt_value(input, output, "Repository ID", None, true)?
        .expect("required prompt returns a value");
    let existing = config.repository(&id)?.unwrap_or_default();

    Ok(RepositoryValues {
        id,
        repo_provider: prompt_value(
            input,
            output,
            "Repository provider ID",
            existing.repo_provider.as_deref(),
            true,
        )?,
        host: prompt_value(
            input,
            output,
            "Repository host",
            existing.host.as_deref().or(Some("github.com")),
            true,
        )?,
        owner: prompt_value(
            input,
            output,
            "Repository owner",
            existing.owner.as_deref(),
            true,
        )?,
        name: prompt_value(
            input,
            output,
            "Repository name",
            existing.name.as_deref(),
            true,
        )?,
        provider_repository_id: prompt_value(
            input,
            output,
            "Provider repository ID",
            existing.provider_repository_id.as_deref(),
            true,
        )?,
        storage_provider: prompt_value(
            input,
            output,
            "Storage provider ID",
            existing.storage_provider.as_deref(),
            true,
        )?,
    })
}

fn prompt_value<R, W>(
    input: &mut R,
    output: &mut W,
    label: &str,
    default: Option<&str>,
    required: bool,
) -> CliResult<Option<String>>
where
    R: BufRead,
    W: Write,
{
    write!(output, "{label}").map_err(output_error)?;
    if let Some(default) = default {
        write!(output, " [{default}]").map_err(output_error)?;
    } else if !required {
        write!(output, " [optional]").map_err(output_error)?;
    }
    write!(output, ": ").map_err(output_error)?;
    output.flush().map_err(output_error)?;

    let value = read_config_prompt_value(input)?;
    if value.is_empty() {
        if let Some(default) = default {
            return Ok(Some(default.to_owned()));
        }
        if required {
            return Err(CliError::InvalidArguments {
                message: format!("{label} is required"),
            });
        }
        return Ok(None);
    }
    Ok(Some(value))
}

fn read_config_prompt_value<R>(input: &mut R) -> CliResult<String>
where
    R: BufRead + ?Sized,
{
    let maximum_line_bytes = MAX_CONFIG_PROMPT_INPUT_BYTES + 2;
    let mut bytes = Vec::with_capacity(maximum_line_bytes + 1);
    input
        .take((maximum_line_bytes + 1) as u64)
        .read_until(b'\n', &mut bytes)
        .map_err(|source| CliError::Io {
            context: "failed to read interactive configuration input".to_owned(),
            source,
        })?;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_CONFIG_PROMPT_INPUT_BYTES {
        return Err(CliError::InvalidArguments {
            message: format!(
                "interactive configuration input must not exceed {MAX_CONFIG_PROMPT_INPUT_BYTES} bytes"
            ),
        });
    }
    String::from_utf8(bytes)
        .map(|value| value.trim().to_owned())
        .map_err(|_| CliError::InvalidArguments {
            message: "interactive configuration input must be valid UTF-8".to_owned(),
        })
}

fn read_hidden_config_value_from_terminal() -> CliResult<String> {
    let mut terminal = terminal_prompt::Terminal::open().map_err(|source| CliError::Io {
        context: "failed to open terminal for hidden configuration input".to_owned(),
        source,
    })?;
    let echo_was_enabled = terminal
        .is_echo_enabled()
        .map_err(|source| config_terminal_echo_error("inspect", source))?;
    if echo_was_enabled {
        terminal
            .set_echo_enabled(false)
            .map_err(|source| config_terminal_echo_error("disable", source))?;
    }
    let read_result = read_config_prompt_value(&mut terminal);
    let restore_result = if echo_was_enabled {
        terminal
            .set_echo_enabled(true)
            .map_err(|source| config_terminal_echo_error("restore", source))
    } else {
        Ok(())
    };
    match (read_result, restore_result) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn config_terminal_echo_error(action: &str, source: io::Error) -> CliError {
    CliError::Io {
        context: format!("failed to {action} terminal echo for secret configuration input"),
        source,
    }
}

fn save_and_report_edit<W>(
    config: &EditableServerConfig,
    outcome: EditOutcome,
    kind: &str,
    id: &str,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    if outcome != EditOutcome::Unchanged {
        config.save()?;
    }
    let action = match outcome {
        EditOutcome::Added => "added",
        EditOutcome::Updated => "updated",
        EditOutcome::Unchanged => "unchanged",
    };
    writeln!(
        output,
        "{action} {kind} {id:?} in {}",
        config.path().display()
    )
    .map_err(output_error)
}

fn save_and_report_remove<W>(
    config: &EditableServerConfig,
    outcome: RemoveOutcome,
    kind: &str,
    id: &str,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    if outcome == RemoveOutcome::Removed {
        config.save()?;
    }
    let action = match outcome {
        RemoveOutcome::Removed => "removed",
        RemoveOutcome::NotFound => "not found",
    };
    writeln!(
        output,
        "{action} {kind} {id:?} in {}",
        config.path().display()
    )
    .map_err(output_error)
}

fn write_repository_provider_list<W>(config: &EditableServerConfig, output: &mut W) -> CliResult<()>
where
    W: Write,
{
    writeln!(output, "ID\tTYPE\tAPI URL\tLEGACY SESSION SECRET").map_err(output_error)?;
    for provider in config.repository_providers()? {
        writeln!(
            output,
            "{}\t{}\t{}\t{}",
            one_line(&provider.id),
            optional_cell(provider.provider_type.as_deref()),
            optional_cell(provider.api_url.as_deref().or_else(|| {
                (provider.provider_type.as_deref() == Some("github"))
                    .then_some(crate::DEFAULT_GITHUB_API_URL)
            })),
            if provider.personal_access_token.is_some() {
                "configured"
            } else {
                "-"
            }
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn write_storage_provider_list<W>(config: &EditableServerConfig, output: &mut W) -> CliResult<()>
where
    W: Write,
{
    writeln!(
        output,
        "ID\tTYPE\tCREDENTIALS\tCONFIG DIR\tEXECUTABLE\tROOT FOLDER ID\tDISPLAY NAME"
    )
    .map_err(output_error)?;
    for storage in config.storage_providers()? {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            one_line(&storage.id),
            optional_cell(storage.provider_type.as_deref()),
            optional_cell(storage.credentials_type.as_deref()),
            optional_cell(storage.config_dir.as_deref()),
            optional_cell(storage.executable.as_deref()),
            optional_cell(storage.root_folder_id.as_deref()),
            optional_cell(storage.display_name.as_deref()),
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn write_repository_list<W>(config: &EditableServerConfig, output: &mut W) -> CliResult<()>
where
    W: Write,
{
    writeln!(
        output,
        "ID\tREPOSITORY PROVIDER\tHOST\tOWNER\tNAME\tPROVIDER REPOSITORY ID\tSTORAGE PROVIDER"
    )
    .map_err(output_error)?;
    for repository in config.repositories()? {
        writeln!(
            output,
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            one_line(&repository.id),
            optional_cell(repository.repo_provider.as_deref()),
            optional_cell(repository.host.as_deref()),
            optional_cell(repository.owner.as_deref()),
            optional_cell(repository.name.as_deref()),
            optional_cell(repository.provider_repository_id.as_deref()),
            optional_cell(repository.storage_provider.as_deref()),
        )
        .map_err(output_error)?;
    }
    Ok(())
}

fn one_line(value: &str) -> String {
    value
        .chars()
        .map(|character| match character {
            '\n' | '\r' | '\t' => ' ',
            other => other,
        })
        .collect()
}

fn optional_cell(value: Option<&str>) -> String {
    value.map(one_line).unwrap_or_else(|| "-".to_owned())
}

fn required_flag<T>(value: Option<T>, flag: &str) -> CliResult<T> {
    value.ok_or_else(|| CliError::InvalidArguments {
        message: format!("{flag} is required when using flag-based add"),
    })
}

fn optional_path_string(path: Option<PathBuf>, flag: &str) -> CliResult<Option<String>> {
    path.map(|path| {
        path.into_os_string()
            .into_string()
            .map_err(|_| CliError::InvalidArguments {
                message: format!("{flag} must be valid UTF-8"),
            })
    })
    .transpose()
}

#[cfg(not(windows))]
fn default_gcloud_executable() -> &'static str {
    "gcloud"
}

#[cfg(windows)]
fn default_gcloud_executable() -> &'static str {
    "gcloud.cmd"
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
    fn zero_flag_add_commands_prompt_for_complete_configuration_without_echoing_secrets() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(
            &config_path,
            "server:\n  host: 127.0.0.1\n  port: 8080\n  public_url: http://127.0.0.1:8080\n  session_encryption_secret: interactive-session-secret-at-least-32-characters\n",
        )
        .expect("base config should be written");
        let path = config_path.to_str().expect("test path should be UTF-8");

        let repository_provider_output = run_configuration_test_command(
            &["lfscloud", "--config", path, "config", "repository", "add"],
            "github-main\n\n\n",
        );
        assert!(repository_provider_output.contains("added repository provider"));
        assert!(!repository_provider_output.contains("super-secret-pat"));

        let storage_output = run_configuration_test_command(
            &["lfscloud", "--config", path, "config", "storage", "add"],
            "drive-main\n\n\n./gcloud\n\nroot-folder\nMain Drive\n",
        );
        assert!(storage_output.contains("added storage provider"));

        let repository_output = run_configuration_test_command(
            &["lfscloud", "--config", path, "repository", "add"],
            "github-main:owner/repo\ngithub-main\n\nowner\nrepo\n123456789\ndrive-main\n",
        );
        assert!(repository_output.contains("added repository"));

        let config =
            ServerConfig::load_from_path(&config_path).expect("interactive config should be valid");
        assert!(config.repository_providers.contains_key("github-main"));
        assert!(config.storage_providers.contains_key("drive-main"));
        assert_eq!(config.repositories[0].id, "github-main:owner/repo");
        assert_eq!(config.repositories[0].host, "github.com");
    }

    #[test]
    fn interactive_repository_provider_can_clear_api_override() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(
            &config_path,
            "server: {}\nrepository_providers:\n  github:\n    type: github\n    api_url: https://github.example/api/v3\n",
        )
        .expect("configuration fixture should be written");
        let config = EditableServerConfig::load(config_path).expect("config should load");
        let mut input = "github\n\ndefault\n".as_bytes();
        let mut output = Vec::new();

        let values = prompt_repository_provider(
            &config,
            &mut input,
            &mut output,
            &mut read_config_prompt_value,
        )
        .expect("interactive provider values should parse");

        assert!(values.clear_api_url);
        assert!(values.api_url.is_none());
    }

    #[test]
    fn flag_add_partial_update_list_and_idempotent_remove_work_end_to_end() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(
            &config_path,
            r#"
server:
  host: 127.0.0.1
  port: 8080
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: test-pat
storage_providers:
  drive-main:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ./gcloud
    root_folder_id: root-folder
"#,
        )
        .expect("base config should be written");
        let path = config_path.to_str().expect("test path should be UTF-8");

        let added = run_configuration_test_command(
            &[
                "lfscloud",
                "--config",
                path,
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
            ],
            "",
        );
        assert!(added.contains("added repository"));

        let updated = run_configuration_test_command(
            &[
                "lfscloud",
                "--config",
                path,
                "repository",
                "add",
                "--id",
                "github-main:owner/repo",
                "--name",
                "renamed",
            ],
            "",
        );
        assert!(updated.contains("updated repository"));
        let unchanged = run_configuration_test_command(
            &[
                "lfscloud",
                "--config",
                path,
                "repository",
                "add",
                "--id",
                "github-main:owner/repo",
                "--name",
                "renamed",
            ],
            "",
        );
        assert!(unchanged.contains("unchanged repository"));

        let repositories = run_configuration_test_command(
            &["lfscloud", "--config", path, "repository", "list"],
            "",
        );
        assert!(repositories.contains("PROVIDER REPOSITORY ID"));
        assert!(repositories.contains("github-main:owner/repo"));
        assert!(repositories.contains("renamed"));
        let repository_providers = run_configuration_test_command(
            &["lfscloud", "--config", path, "config", "repository", "list"],
            "",
        );
        assert!(repository_providers.contains("github-main"));
        assert!(repository_providers.contains("configured"));
        assert!(!repository_providers.contains("test-pat"));
        let cleared = run_configuration_test_command(
            &[
                "lfscloud",
                "--config",
                path,
                "config",
                "repository",
                "add",
                "--id",
                "github-main",
                "--clear-api-url",
            ],
            "",
        );
        assert!(cleared.contains("updated repository provider"));
        let cleared_yaml = fs::read_to_string(&config_path).expect("config should be readable");
        assert!(!cleared_yaml.contains("api_url"));
        let storage_providers = run_configuration_test_command(
            &["lfscloud", "--config", path, "config", "storage", "list"],
            "",
        );
        assert!(storage_providers.contains("drive-main"));
        assert!(storage_providers.contains("root-folder"));

        let removed = run_configuration_test_command(
            &[
                "lfscloud",
                "--config",
                path,
                "repository",
                "remove",
                "--id",
                "github-main:owner/repo",
            ],
            "",
        );
        assert!(removed.contains("removed repository"));
        let repeated = run_configuration_test_command(
            &[
                "lfscloud",
                "--config",
                path,
                "repository",
                "remove",
                "--id",
                "github-main:owner/repo",
            ],
            "",
        );
        assert!(repeated.contains("not found repository"));
    }

    fn run_configuration_test_command(args: &[&str], input: &str) -> String {
        let cli = Cli::try_parse_from(args).expect("configuration command should parse");
        let command = match cli.command {
            super::Command::Config(command) => ConfigurationCommand::Config(command),
            super::Command::Repository(command) => ConfigurationCommand::Repository(command),
            _ => panic!("test command should edit configuration"),
        };
        let mut input = io::Cursor::new(input.as_bytes());
        let mut output = Vec::new();
        run_configuration_with_input(
            command,
            cli.config,
            &mut input,
            &mut output,
            read_config_prompt_value,
        )
        .expect("configuration command should succeed");
        String::from_utf8(output).expect("configuration output should be UTF-8")
    }
}
