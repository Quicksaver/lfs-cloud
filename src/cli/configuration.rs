//! Editing and listing the server's provider and repository configuration.

use super::*;

const MAX_CONFIG_PROMPT_INPUT_BYTES: usize = 16 * 1024;
const DEFAULT_GOOGLE_DRIVE_PROVIDER_ID: &str = "google_drive";
const DEFAULT_GOOGLE_DRIVE_ROOT_FOLDER_ID: &str = "root";
const GOOGLE_CLOUD_PLATFORM_SCOPE: &str = "https://www.googleapis.com/auth/cloud-platform";
const GOOGLE_DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

#[derive(Debug)]
struct PreparedStorageProvider {
    values: StorageProviderValues,
    client_secret_file: Option<PathBuf>,
}

struct ConfigurationOperations<M, A, G> {
    select: M,
    authorize_drive: A,
    resolve_github_id: G,
}

pub(super) fn run_configuration_to_stdio(
    command: ConfigurationCommand,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();

    if stdin.is_terminal() {
        run_configuration_with_input(
            command,
            config_path,
            &mut input,
            &mut output,
            |_| read_hidden_config_value_from_terminal(),
            ConfigurationOperations {
                select: terminal_select_index,
                authorize_drive: authorize_google_drive_adc,
                resolve_github_id: resolve_github_repository_id,
            },
        )
    } else {
        run_configuration_with_input(
            command,
            config_path,
            &mut input,
            &mut output,
            read_config_prompt_value,
            ConfigurationOperations {
                select: prompt_select_index,
                authorize_drive: authorize_google_drive_adc,
                resolve_github_id: resolve_github_repository_id,
            },
        )
    }
    .map_err(anyhow::Error::from)
}

fn run_configuration_with_input<R, W, S, M, A, G>(
    command: ConfigurationCommand,
    config_path: Option<PathBuf>,
    input: &mut R,
    output: &mut W,
    mut read_secret: S,
    mut operations: ConfigurationOperations<M, A, G>,
) -> CliResult<()>
where
    R: BufRead,
    W: Write,
    S: FnMut(&mut R) -> CliResult<String>,
    M: FnMut(&mut R, &mut W, &str, &[String], usize) -> CliResult<usize>,
    A: FnMut(&PreparedStorageProvider) -> CliResult<()>,
    G: FnMut(&str, &str, &str) -> CliResult<String>,
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
                    prompt_repository_provider(
                        &config,
                        input,
                        output,
                        &mut read_secret,
                        &mut operations.select,
                    )?
                } else {
                    command.into_values()?
                };
                let id = values.id.clone();
                let outcome = config.upsert_repository_provider(values)?;
                save_and_report_edit(&config, outcome, "repository provider", &id, output)
            }
            ConfigRepositoryAction::Remove(command) => {
                let id = select_remove_id(
                    command.id,
                    config
                        .repository_providers()?
                        .into_iter()
                        .map(|provider| provider.id)
                        .collect(),
                    "repository provider",
                    input,
                    output,
                    &mut operations.select,
                )?;
                let outcome = config.remove_repository_provider(&id)?;
                save_and_report_remove(&config, outcome, "repository provider", &id, output)
            }
            ConfigRepositoryAction::List => write_repository_provider_list(&config, output),
        },
        ConfigurationCommand::Config(ConfigCommand {
            resource: ConfigResourceCommand::Storage(ConfigStorageCommand { action }),
        }) => match action {
            ConfigStorageAction::Add(command) => {
                let prepared = if command.is_interactive() {
                    prompt_storage_provider(&config, input, output, &mut operations.select)?
                } else {
                    command.into_prepared()?
                };
                if prepared.client_secret_file.is_some() {
                    (operations.authorize_drive)(&prepared)?;
                }
                let id = prepared.values.id.clone();
                let outcome = config.upsert_storage_provider(prepared.values)?;
                save_and_report_edit(&config, outcome, "storage provider", &id, output)
            }
            ConfigStorageAction::Remove(command) => {
                let id = select_remove_id(
                    command.id,
                    config
                        .storage_providers()?
                        .into_iter()
                        .map(|provider| provider.id)
                        .collect(),
                    "storage provider",
                    input,
                    output,
                    &mut operations.select,
                )?;
                let outcome = config.remove_storage_provider(&id)?;
                save_and_report_remove(&config, outcome, "storage provider", &id, output)
            }
            ConfigStorageAction::List => write_storage_provider_list(&config, output),
        },
        ConfigurationCommand::Repository(RepositoryCommand { action }) => match action {
            RepositoryAction::Add(command) => {
                let values = if command.is_interactive() {
                    prompt_repository(
                        &config,
                        input,
                        output,
                        &mut operations.select,
                        &mut operations.resolve_github_id,
                    )?
                } else {
                    command.into_values()?
                };
                let id = values.id.clone();
                let outcome = config.upsert_repository(values)?;
                save_and_report_edit(&config, outcome, "repository", &id, output)
            }
            RepositoryAction::Remove(command) => {
                let id = select_remove_id(
                    command.id,
                    config
                        .repositories()?
                        .into_iter()
                        .map(|repository| repository.id)
                        .collect(),
                    "repository",
                    input,
                    output,
                    &mut operations.select,
                )?;
                let outcome = config.remove_repository(&id)?;
                save_and_report_remove(&config, outcome, "repository", &id, output)
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
            && self.client_secret_file.is_none()
            && self.executable.is_none()
            && self.root_folder_id.is_none()
            && self.display_name.is_none()
    }

    fn into_prepared(self) -> CliResult<PreparedStorageProvider> {
        let applies_google_drive_defaults = self.client_secret_file.is_some();
        let id = if applies_google_drive_defaults {
            self.id
                .unwrap_or_else(|| DEFAULT_GOOGLE_DRIVE_PROVIDER_ID.to_owned())
        } else {
            required_flag(self.id, "--id")?
        };
        let provider_type = self.provider_type.map(|kind| match kind {
            StorageProviderKind::GoogleDrive => "google_drive".to_owned(),
        });
        let credentials_type = self.credentials_type.map(|kind| match kind {
            StorageCredentialKind::Gcloud => "gcloud".to_owned(),
        });
        Ok(PreparedStorageProvider {
            values: StorageProviderValues {
                id,
                provider_type: provider_type
                    .or_else(|| applies_google_drive_defaults.then(|| "google_drive".to_owned())),
                credentials_type: credentials_type
                    .or_else(|| applies_google_drive_defaults.then(|| "gcloud".to_owned())),
                config_dir: optional_path_string(self.config_dir, "--config-dir")?.or_else(|| {
                    applies_google_drive_defaults.then(default_gcloud_config_reference)
                }),
                executable: optional_path_string(self.executable, "--executable")?,
                root_folder_id: self.root_folder_id.or_else(|| {
                    applies_google_drive_defaults
                        .then(|| DEFAULT_GOOGLE_DRIVE_ROOT_FOLDER_ID.to_owned())
                }),
                display_name: self.display_name,
            },
            client_secret_file: self.client_secret_file,
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

fn prompt_repository_provider<R, W, S, M>(
    config: &EditableServerConfig,
    input: &mut R,
    output: &mut W,
    _read_secret: &mut S,
    select: &mut M,
) -> CliResult<RepositoryProviderValues>
where
    R: BufRead,
    W: Write,
    S: FnMut(&mut R) -> CliResult<String>,
    M: FnMut(&mut R, &mut W, &str, &[String], usize) -> CliResult<usize>,
{
    let provider_kinds = vec!["GitHub".to_owned()];
    let selected = select(
        input,
        output,
        "Repository provider type",
        &provider_kinds,
        0,
    )?;
    debug_assert_eq!(selected, 0);
    let default_id = config
        .repository_provider("github")?
        .is_none()
        .then_some("github");
    let id = prompt_value(input, output, "Repository provider ID", default_id, true)?
        .expect("required prompt returns a value");
    let existing = config.repository_provider(&id)?.unwrap_or_default();
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
        provider_type: Some("github".to_owned()),
        api_url,
        clear_api_url,
        personal_access_token: existing.personal_access_token,
    })
}

fn prompt_storage_provider<R, W, M>(
    config: &EditableServerConfig,
    input: &mut R,
    output: &mut W,
    select: &mut M,
) -> CliResult<PreparedStorageProvider>
where
    R: BufRead,
    W: Write,
    M: FnMut(&mut R, &mut W, &str, &[String], usize) -> CliResult<usize>,
{
    let provider_kinds = vec!["Google Drive".to_owned()];
    let selected = select(input, output, "Storage provider type", &provider_kinds, 0)?;
    debug_assert_eq!(selected, 0);
    let default_id = config
        .storage_provider(DEFAULT_GOOGLE_DRIVE_PROVIDER_ID)?
        .is_none()
        .then_some(DEFAULT_GOOGLE_DRIVE_PROVIDER_ID);
    let id = prompt_value(input, output, "Storage provider ID", default_id, true)?
        .expect("required prompt returns a value");
    let existing = config.storage_provider(&id)?.unwrap_or_default();
    let client_secret_file = prompt_value(
        input,
        output,
        if existing.provider_type.is_some() {
            "Desktop OAuth client JSON (blank to keep existing ADC)"
        } else {
            "Desktop OAuth client JSON"
        },
        None,
        existing.provider_type.is_none(),
    )?;
    let default_config_dir = default_gcloud_config_reference();
    let config_dir = prompt_value(
        input,
        output,
        "gcloud config directory",
        existing.config_dir.as_deref().or(Some(&default_config_dir)),
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
        existing
            .root_folder_id
            .as_deref()
            .or(Some(DEFAULT_GOOGLE_DRIVE_ROOT_FOLDER_ID)),
        true,
    )?;
    let display_name = prompt_value(
        input,
        output,
        "Display name",
        existing.display_name.as_deref(),
        false,
    )?;

    Ok(PreparedStorageProvider {
        values: StorageProviderValues {
            id,
            provider_type: Some("google_drive".to_owned()),
            credentials_type: Some("gcloud".to_owned()),
            config_dir,
            executable,
            root_folder_id,
            display_name,
        },
        client_secret_file: client_secret_file.map(PathBuf::from),
    })
}

fn prompt_repository<R, W, M, G>(
    config: &EditableServerConfig,
    input: &mut R,
    output: &mut W,
    select: &mut M,
    resolve_github_id: &mut G,
) -> CliResult<RepositoryValues>
where
    R: BufRead,
    W: Write,
    M: FnMut(&mut R, &mut W, &str, &[String], usize) -> CliResult<usize>,
    G: FnMut(&str, &str, &str) -> CliResult<String>,
{
    let repository_providers = config.repository_providers()?;
    let repository_provider_ids = repository_providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    let repo_provider_index = select_required_configured_value(
        input,
        output,
        "Repository provider",
        "repository providers",
        &repository_provider_ids,
        select,
    )?;
    let repo_provider = repository_provider_ids[repo_provider_index].clone();
    let storage_provider_ids = config
        .storage_providers()?
        .into_iter()
        .map(|provider| provider.id)
        .collect::<Vec<_>>();
    let storage_provider_index = select_required_configured_value(
        input,
        output,
        "Storage provider",
        "storage providers",
        &storage_provider_ids,
        select,
    )?;
    let storage_provider = storage_provider_ids[storage_provider_index].clone();
    let owner = prompt_value(input, output, "Repository owner", None, true)?
        .expect("required prompt returns a value");
    let name = prompt_value(input, output, "Repository name", None, true)?
        .expect("required prompt returns a value");
    let host = prompt_value(input, output, "Repository host", Some("github.com"), true)?
        .expect("required prompt returns a value");
    let provider = &repository_providers[repo_provider_index];
    if provider.provider_type.as_deref() != Some("github") {
        return Err(CliError::InvalidArguments {
            message: format!(
                "interactive repository setup does not support repository provider type {:?}",
                provider.provider_type.as_deref().unwrap_or("unknown")
            ),
        });
    }
    let provider_repository_id = resolve_github_id(&host, &owner, &name)?;
    let id = format!("{repo_provider}:{owner}/{name}");
    Ok(RepositoryValues {
        id,
        repo_provider: Some(repo_provider),
        host: Some(host),
        owner: Some(owner),
        name: Some(name),
        provider_repository_id: Some(provider_repository_id),
        storage_provider: Some(storage_provider),
    })
}

fn select_required_configured_value<R, W, M>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
    kind: &str,
    items: &[String],
    select: &mut M,
) -> CliResult<usize>
where
    R: BufRead,
    W: Write,
    M: FnMut(&mut R, &mut W, &str, &[String], usize) -> CliResult<usize>,
{
    if items.is_empty() {
        return Err(CliError::InvalidArguments {
            message: format!("no {kind} are configured; add one before configuring a repository"),
        });
    }
    select(input, output, prompt, items, 0)
}

fn select_remove_id<R, W, M>(
    id: Option<String>,
    items: Vec<String>,
    kind: &str,
    input: &mut R,
    output: &mut W,
    select: &mut M,
) -> CliResult<String>
where
    R: BufRead,
    W: Write,
    M: FnMut(&mut R, &mut W, &str, &[String], usize) -> CliResult<usize>,
{
    if let Some(id) = id {
        return Ok(id);
    }
    if items.is_empty() {
        return Err(CliError::InvalidArguments {
            message: format!("no {kind}s are configured to remove"),
        });
    }
    let index = select(input, output, &format!("{kind} to remove"), &items, 0)?;
    Ok(items[index].clone())
}

fn terminal_select_index<R, W>(
    _input: &mut R,
    _output: &mut W,
    prompt: &str,
    items: &[String],
    default: usize,
) -> CliResult<usize>
where
    R: BufRead,
    W: Write,
{
    Select::new()
        .with_prompt(prompt)
        .items(items)
        .default(default)
        .interact_on(&Term::stderr())
        .map_err(|error| CliError::Io {
            context: format!("failed to read {prompt} selection from the terminal"),
            source: io::Error::other(error.to_string()),
        })
}

fn prompt_select_index<R, W>(
    input: &mut R,
    output: &mut W,
    prompt: &str,
    items: &[String],
    default: usize,
) -> CliResult<usize>
where
    R: BufRead,
    W: Write,
{
    writeln!(output, "{prompt}:").map_err(output_error)?;
    for (index, item) in items.iter().enumerate() {
        writeln!(output, "  {}) {item}", index + 1).map_err(output_error)?;
    }
    write!(output, "Selection [{}]: ", default + 1).map_err(output_error)?;
    output.flush().map_err(output_error)?;
    let value = read_config_prompt_value(input)?;
    if value.is_empty() {
        return Ok(default);
    }
    if let Some(index) = items.iter().position(|item| item == &value) {
        return Ok(index);
    }
    let index = value
        .parse::<usize>()
        .ok()
        .and_then(|number| number.checked_sub(1))
        .filter(|index| *index < items.len())
        .ok_or_else(|| CliError::InvalidArguments {
            message: format!(
                "{prompt} selection must name an item or be between 1 and {}",
                items.len()
            ),
        })?;
    Ok(index)
}

#[cfg(not(windows))]
fn default_gcloud_config_reference() -> String {
    "${HOME}/.config/lfscloud/gcloud-drive".to_owned()
}

#[cfg(windows)]
fn default_gcloud_config_reference() -> String {
    "${USERPROFILE}/.config/lfscloud/gcloud-drive".to_owned()
}

fn authorize_google_drive_adc(prepared: &PreparedStorageProvider) -> CliResult<()> {
    let client_secret_file = prepared
        .client_secret_file
        .as_deref()
        .expect("authorization is called only with a client secret file");
    if !client_secret_file.is_file() {
        return Err(CliError::InvalidArguments {
            message: format!(
                "--client-secret-file must identify an existing file: {}",
                client_secret_file.display()
            ),
        });
    }
    let config_dir =
        prepared
            .values
            .config_dir
            .as_deref()
            .ok_or_else(|| CliError::InvalidArguments {
                message: "--config-dir is required for Google Drive authorization".to_owned(),
            })?;
    let config_dir = resolve_setup_path(config_dir)?;
    fs::create_dir_all(&config_dir).map_err(|source| CliError::Io {
        context: format!(
            "failed to create isolated gcloud config directory {}",
            config_dir.display()
        ),
        source,
    })?;
    set_private_directory_permissions(&config_dir)?;

    let executable = prepared
        .values
        .executable
        .as_deref()
        .unwrap_or_else(|| default_gcloud_executable());
    let scopes = format!("{GOOGLE_CLOUD_PLATFORM_SCOPE},{GOOGLE_DRIVE_FILE_SCOPE}");
    let mut client_secret_argument = OsString::from("--client-id-file=");
    client_secret_argument.push(client_secret_file);
    let status = ProcessCommand::new(executable)
        .args(["auth", "application-default", "login"])
        .arg(client_secret_argument)
        .arg(format!("--scopes={scopes}"))
        .env("CLOUDSDK_CONFIG", &config_dir)
        .env_remove("GOOGLE_APPLICATION_CREDENTIALS")
        .env_remove("CLOUDSDK_AUTH_ACCESS_TOKEN")
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|source| CliError::Io {
            context: format!(
                "failed to start {executable} for Google Drive Application Default Credentials"
            ),
            source,
        })?;
    if !status.success() {
        return Err(CliError::ExternalCommand {
            command: format!("{executable} auth application-default login"),
            status: command_status_text(status),
            stderr: SanitizedMessage::new(
                "Google Drive Application Default Credentials were not created",
            ),
        });
    }

    let adc_path = config_dir.join("application_default_credentials.json");
    if !adc_path.is_file() {
        return Err(CliError::InvalidArguments {
            message: format!("gcloud completed without creating {}", adc_path.display()),
        });
    }
    set_private_file_permissions(&adc_path)
}

fn resolve_setup_path(value: &str) -> CliResult<PathBuf> {
    for variable in ["HOME", "USERPROFILE"] {
        let prefix = format!("${{{variable}}}");
        if let Some(suffix) = value.strip_prefix(&prefix) {
            if suffix.contains("${") {
                return Err(unresolved_setup_path_environment_reference());
            }
            let root = std::env::var_os(variable).ok_or_else(|| CliError::InvalidArguments {
                message: format!(
                    "gcloud config directory references unset environment variable {variable}"
                ),
            })?;
            return Ok(PathBuf::from(root).join(suffix.trim_start_matches(['/', '\\'])));
        }
    }
    if value.contains("${") {
        return Err(unresolved_setup_path_environment_reference());
    }
    Ok(PathBuf::from(value))
}

fn unresolved_setup_path_environment_reference() -> CliError {
    CliError::InvalidArguments {
        message:
            "gcloud config directory contains an environment reference that setup cannot resolve"
                .to_owned(),
    }
}

#[cfg(unix)]
fn set_private_directory_permissions(path: &Path) -> CliResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).map_err(|source| CliError::Io {
        context: format!(
            "failed to secure gcloud config directory {}",
            path.display()
        ),
        source,
    })
}

#[cfg(not(unix))]
fn set_private_directory_permissions(_path: &Path) -> CliResult<()> {
    Ok(())
}

#[cfg(unix)]
fn set_private_file_permissions(path: &Path) -> CliResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|source| CliError::Io {
        context: format!("failed to secure gcloud ADC file {}", path.display()),
        source,
    })
}

#[cfg(not(unix))]
fn set_private_file_permissions(_path: &Path) -> CliResult<()> {
    Ok(())
}

fn resolve_github_repository_id(host: &str, owner: &str, name: &str) -> CliResult<String> {
    resolve_github_repository_id_with_executable(Path::new("gh"), host, owner, name)
}

fn resolve_github_repository_id_with_executable(
    executable: &Path,
    host: &str,
    owner: &str,
    name: &str,
) -> CliResult<String> {
    validate_github_repository_path_component(owner, "repository owner")?;
    validate_github_repository_path_component(name, "repository name")?;

    let mut command = ProcessCommand::new(executable);
    command.arg("api");
    if host != "github.com" {
        command.args(["--hostname", host]);
    }
    command
        .arg(format!("repos/{owner}/{name}"))
        .args(["--jq", ".id"]);
    let output = command.output().map_err(|source| CliError::Io {
        context: "failed to start gh for GitHub repository ID lookup; install and authenticate GitHub CLI"
            .to_owned(),
        source,
    })?;
    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: format!("gh api repos/{owner}/{name} --jq .id"),
            status: command_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }
    let value = String::from_utf8(output.stdout).map_err(|_| CliError::ExternalCommandOutput {
        command: "gh api repository ID lookup".to_owned(),
        message: SanitizedMessage::new("gh returned a non-UTF-8 repository ID"),
    })?;
    let id = value.trim();
    let parsed = id.parse::<u64>().ok().filter(|id| *id > 0).ok_or_else(|| {
        CliError::ExternalCommandOutput {
            command: "gh api repository ID lookup".to_owned(),
            message: SanitizedMessage::new("gh did not return a positive numeric repository ID"),
        }
    })?;
    Ok(parsed.to_string())
}

fn validate_github_repository_path_component(value: &str, label: &str) -> CliResult<()> {
    if value.is_empty()
        || matches!(value, "." | "..")
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "{label} must be a route-safe repository component without separators, percent escapes, or traversal segments"
            ),
        });
    }

    Ok(())
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
            "\ngithub-main\n\n",
        );
        assert!(repository_provider_output.contains("added repository provider"));
        assert!(!repository_provider_output.contains("super-secret-pat"));

        let storage_output = run_configuration_test_command(
            &["lfscloud", "--config", path, "config", "storage", "add"],
            "\ndrive-main\n/client_secret.json\n\n\nroot-folder\nMain Drive\n",
        );
        assert!(storage_output.contains("added storage provider"));

        let repository_output = run_configuration_test_command(
            &["lfscloud", "--config", path, "repository", "add"],
            "\n\nowner\nrepo\n\n",
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
            "repository_providers:\n  github:\n    type: github\n    api_url: https://github.example/api/v3\n",
        )
        .expect("configuration fixture should be written");
        let config = EditableServerConfig::load(config_path).expect("config should load");
        let mut input = "\ngithub\ndefault\n".as_bytes();
        let mut output = Vec::new();
        let mut select = prompt_select_index;

        let values = prompt_repository_provider(
            &config,
            &mut input,
            &mut output,
            &mut read_config_prompt_value,
            &mut select,
        )
        .expect("interactive provider values should parse");

        assert!(values.clear_api_url);
        assert!(values.api_url.is_none());
    }

    #[test]
    fn client_secret_flag_applies_google_drive_setup_defaults() {
        let prepared = StorageProviderAddCommand {
            id: None,
            provider_type: None,
            credentials_type: None,
            config_dir: None,
            client_secret_file: Some(PathBuf::from("client_secret.json")),
            executable: None,
            root_folder_id: None,
            display_name: None,
        }
        .into_prepared()
        .expect("client-secret setup defaults should be complete");

        assert_eq!(prepared.values.id, "google_drive");
        assert_eq!(
            prepared.values.provider_type.as_deref(),
            Some("google_drive")
        );
        assert_eq!(prepared.values.credentials_type.as_deref(), Some("gcloud"));
        assert_eq!(
            prepared.values.config_dir.as_deref(),
            Some(default_gcloud_config_reference().as_str())
        );
        assert_eq!(prepared.values.root_folder_id.as_deref(), Some("root"));
    }

    #[test]
    fn setup_path_rejects_additional_environment_references_after_home_prefix() {
        for value in [
            "${HOME}/${OTHER}/gcloud",
            "${USERPROFILE}\\${OTHER}\\gcloud",
        ] {
            let error = resolve_setup_path(value)
                .expect_err("nested environment references must be rejected before expansion");
            assert!(matches!(
                error,
                CliError::InvalidArguments { message }
                    if message.contains("contains an environment reference that setup cannot resolve")
            ));
        }
    }

    #[test]
    fn github_repository_lookup_rejects_invalid_path_components_before_running_gh() {
        for (owner, name) in [
            ("", "assets"),
            (".", "assets"),
            ("..", "assets"),
            ("octo..org", "assets"),
            ("octo/org", "assets"),
            ("octo-org", ""),
            ("octo-org", "assets+archive"),
        ] {
            let error = resolve_github_repository_id_with_executable(
                Path::new("definitely-missing-gh"),
                "github.com",
                owner,
                name,
            )
            .expect_err("invalid repository components must fail before gh starts");
            assert!(matches!(error, CliError::InvalidArguments { .. }));
        }
    }

    #[cfg(unix)]
    #[test]
    fn google_drive_setup_creates_private_adc_directory_and_runs_scoped_login() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temporary directory should be created");
        let client_secret = temp.path().join("client_secret.json");
        fs::write(&client_secret, "{}\n").expect("client secret fixture should be written");
        let config_dir = temp.path().join("gcloud-drive");
        let args_path = temp.path().join("args.txt");
        let fake_gcloud = temp.path().join("gcloud");
        fs::write(
            &fake_gcloud,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nmkdir -p \"$CLOUDSDK_CONFIG\"\nprintf '{{}}\\n' > \"$CLOUDSDK_CONFIG/application_default_credentials.json\"\n",
                args_path.display()
            ),
        )
        .expect("fake gcloud should be written");
        fs::set_permissions(&fake_gcloud, fs::Permissions::from_mode(0o700))
            .expect("fake gcloud should be executable");

        authorize_google_drive_adc(&PreparedStorageProvider {
            values: StorageProviderValues {
                id: "google_drive".to_owned(),
                config_dir: Some(config_dir.display().to_string()),
                executable: Some(fake_gcloud.display().to_string()),
                ..StorageProviderValues::default()
            },
            client_secret_file: Some(client_secret.clone()),
        })
        .expect("Google Drive ADC setup should succeed");

        let arguments = fs::read_to_string(args_path).expect("gcloud arguments should be recorded");
        assert!(arguments.contains("application-default"));
        assert!(arguments.contains("login"));
        assert!(arguments.contains(&format!("--client-id-file={}", client_secret.display())));
        assert!(arguments.contains(GOOGLE_CLOUD_PLATFORM_SCOPE));
        assert!(arguments.contains(GOOGLE_DRIVE_FILE_SCOPE));
        assert_eq!(
            fs::metadata(&config_dir)
                .expect("config directory metadata should load")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(config_dir.join("application_default_credentials.json"))
                .expect("ADC metadata should load")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[cfg(unix)]
    #[test]
    fn github_repository_lookup_uses_host_and_requires_a_numeric_id() {
        use std::os::unix::fs::PermissionsExt;

        let temp = TempDir::new().expect("temporary directory should be created");
        let args_path = temp.path().join("args.txt");
        let fake_gh = temp.path().join("gh");
        fs::write(
            &fake_gh,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nprintf '987654321\\n'\n",
                args_path.display()
            ),
        )
        .expect("fake gh should be written");
        fs::set_permissions(&fake_gh, fs::Permissions::from_mode(0o700))
            .expect("fake gh should be executable");

        let id = resolve_github_repository_id_with_executable(
            &fake_gh,
            "github.example.com",
            "octo-org",
            "assets",
        )
        .expect("GitHub repository ID lookup should succeed");

        assert_eq!(id, "987654321");
        let arguments = fs::read_to_string(args_path).expect("gh arguments should be recorded");
        assert!(arguments.contains("--hostname\ngithub.example.com"));
        assert!(arguments.contains("repos/octo-org/assets"));
        assert!(arguments.contains("--jq\n.id"));
    }

    #[test]
    fn zero_flag_remove_commands_select_existing_entries() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(
            &config_path,
            r#"
repository_providers:
  github:
    type: github
storage_providers:
  google_drive:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ./gcloud
    root_folder_id: root
repositories:
  - id: github:owner/repo
    repo_provider: github
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "123"
    storage_provider: google_drive
"#,
        )
        .expect("configuration fixture should be written");
        let path = config_path.to_str().expect("test path should be UTF-8");

        let repository = run_configuration_test_command(
            &["lfscloud", "--config", path, "repository", "remove"],
            "\n",
        );
        assert!(repository.contains("removed repository"));
        let storage = run_configuration_test_command(
            &["lfscloud", "--config", path, "config", "storage", "remove"],
            "\n",
        );
        assert!(storage.contains("removed storage provider"));
        let provider = run_configuration_test_command(
            &[
                "lfscloud",
                "--config",
                path,
                "config",
                "repository",
                "remove",
            ],
            "\n",
        );
        assert!(provider.contains("removed repository provider"));
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
            ConfigurationOperations {
                select: prompt_select_index,
                authorize_drive: noop_drive_authorization,
                resolve_github_id: fixed_github_repository_id,
            },
        )
        .expect("configuration command should succeed");
        String::from_utf8(output).expect("configuration output should be UTF-8")
    }

    fn noop_drive_authorization(_prepared: &PreparedStorageProvider) -> CliResult<()> {
        Ok(())
    }

    fn fixed_github_repository_id(_host: &str, _owner: &str, _name: &str) -> CliResult<String> {
        Ok("123456789".to_owned())
    }
}
