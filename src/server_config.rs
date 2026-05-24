//! Typed loading and validation for private `lfs-cloud.yml` server config.
//!
//! The config file is server-owned state. Repository `.lfsconfig` files only
//! point Git LFS clients at an LFS Cloud route; this module decides which
//! configured repository provider and storage provider handle that route.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use config::{Config, File, FileFormat};
use serde::Deserialize;

use crate::{ServerError, ServerResult};

/// Default server config path used when no explicit path is supplied.
pub const DEFAULT_CONFIG_PATH: &str = "lfs-cloud.yml";

const DEFAULT_BIND_HOST: &str = "127.0.0.1";
const DEFAULT_BIND_PORT: u16 = 8080;

/// Loaded and validated server configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerConfig {
    /// Server listener and public URL settings.
    pub server: ServerSettings,
    /// Repository providers keyed by configured provider ID.
    pub repository_providers: BTreeMap<String, RepositoryProviderConfig>,
    /// Storage providers keyed by configured storage ID.
    pub storage_providers: BTreeMap<String, StorageProviderConfig>,
    /// Explicit repository-to-storage mappings served by this instance.
    pub repositories: Vec<RepositoryMapping>,
}

impl ServerConfig {
    /// Returns the relative default config path.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{ServerConfig, DEFAULT_CONFIG_PATH};
    ///
    /// assert_eq!(ServerConfig::default_path(), std::path::Path::new(DEFAULT_CONFIG_PATH));
    /// ```
    #[must_use]
    pub fn default_path() -> &'static Path {
        Path::new(DEFAULT_CONFIG_PATH)
    }

    /// Loads `lfs-cloud.yml` from the default path.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the file cannot be read, parsed, or
    /// validated.
    pub fn load_default() -> ServerResult<Self> {
        Self::load_from_path(Self::default_path())
    }

    /// Loads config from an explicit YAML file path.
    ///
    /// Environment references of the form `${NAME}` are resolved after YAML
    /// parsing so validation errors can name the exact config key.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the file cannot be read, parsed, or
    /// validated.
    pub fn load_from_path(path: impl AsRef<Path>) -> ServerResult<Self> {
        let path = path.as_ref();
        let contents = fs::read_to_string(path).map_err(|source| ServerError::ConfigRead {
            path: path.to_path_buf(),
            source,
        })?;

        Self::load_from_str_with_env(&contents, path.display().to_string(), |name| {
            std::env::var(name).ok()
        })
    }

    /// Parses config from a YAML string.
    ///
    /// This is primarily useful for tests and tooling that has already loaded
    /// the config contents.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the string cannot be parsed or validated.
    pub fn load_from_str(contents: &str) -> ServerResult<Self> {
        Self::load_from_str_with_env(contents, "<memory>", |name| std::env::var(name).ok())
    }

    fn load_from_str_with_env(
        contents: &str,
        path: impl Into<String>,
        env: impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        let path = path.into();
        let config = Config::builder()
            .add_source(File::from_str(contents, FileFormat::Yaml))
            .build()
            .map_err(|source| ServerError::ConfigParse {
                path: path.clone(),
                source,
            })?;
        let raw = config
            .try_deserialize::<RawServerConfig>()
            .map_err(|source| ServerError::ConfigParse { path, source })?;

        ServerConfig::from_raw(raw, env)
    }

    fn from_raw(
        raw: RawServerConfig,
        mut env: impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        let server = ServerSettings::from_raw(raw.server, &mut env)?;
        let repository_providers = raw
            .repository_providers
            .into_iter()
            .map(|(id, provider)| {
                RepositoryProviderConfig::from_raw(id, provider, &mut env)
                    .map(|provider| (provider.id().to_owned(), provider))
            })
            .collect::<ServerResult<BTreeMap<_, _>>>()?;
        let storage_providers = raw
            .storage_providers
            .into_iter()
            .map(|(id, provider)| {
                StorageProviderConfig::from_raw(id, provider, &mut env)
                    .map(|provider| (provider.id().to_owned(), provider))
            })
            .collect::<ServerResult<BTreeMap<_, _>>>()?;
        let repositories = raw
            .repositories
            .into_iter()
            .enumerate()
            .map(|(index, repository)| RepositoryMapping::from_raw(index, repository, &mut env))
            .collect::<ServerResult<Vec<_>>>()?;

        let config = Self {
            server,
            repository_providers,
            storage_providers,
            repositories,
        };
        config.validate_references()?;
        Ok(config)
    }

    fn validate_references(&self) -> ServerResult<()> {
        let mut repo_ids = BTreeSet::new();
        let mut route_paths = BTreeSet::new();

        for (index, repository) in self.repositories.iter().enumerate() {
            let repo_path = format!("repositories[{index}]");

            if !self
                .repository_providers
                .contains_key(&repository.repo_provider)
            {
                return invalid_config(
                    format!("{repo_path}.repo_provider"),
                    format!(
                        "references unknown repository provider {:?}",
                        repository.repo_provider
                    ),
                );
            }
            if !self
                .storage_providers
                .contains_key(&repository.storage_provider)
            {
                return invalid_config(
                    format!("{repo_path}.storage_provider"),
                    format!(
                        "references unknown storage provider {:?}",
                        repository.storage_provider
                    ),
                );
            }
            if !repo_ids.insert(repository.id.clone()) {
                return invalid_config(
                    format!("{repo_path}.id"),
                    format!("duplicates repository id {:?}", repository.id),
                );
            }

            let route_path = repository.route_path();
            if !route_paths.insert(route_path.clone()) {
                return invalid_config(
                    format!("{repo_path}.route_path"),
                    format!("duplicates configured route path {route_path:?}"),
                );
            }
        }

        Ok(())
    }
}

/// Server listener and advertised URL settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSettings {
    /// Host or interface address the server should bind to.
    pub host: String,
    /// TCP port the server should bind to.
    pub port: u16,
    /// Public base URL used when constructing Git LFS action URLs.
    pub public_url: String,
}

impl ServerSettings {
    fn from_raw(
        raw: RawServerSettings,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        let host = resolve_required(raw.host, "server.host", env)?;
        let public_url = resolve_required(raw.public_url, "server.public_url", env)?;

        if raw.port == 0 {
            return invalid_config("server.port", "must be greater than zero");
        }

        Ok(Self {
            host,
            port: raw.port,
            public_url,
        })
    }
}

/// Configured repository-provider entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryProviderConfig {
    /// GitHub repository provider configuration.
    GitHub(GitHubProviderConfig),
}

impl RepositoryProviderConfig {
    /// Returns the configured provider ID.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::GitHub(config) => &config.id,
        }
    }

    /// Returns the configured provider type.
    #[must_use]
    pub fn provider_type(&self) -> &'static str {
        match self {
            Self::GitHub(_) => "github",
        }
    }

    fn from_raw(
        id: String,
        raw: RawRepositoryProviderConfig,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        validate_key(&id, format!("repository_providers.{id}"))?;
        let base_path = format!("repository_providers.{id}");
        let provider_type = resolve_required(raw.provider_type, format!("{base_path}.type"), env)?;

        match provider_type.as_str() {
            "github" => Ok(Self::GitHub(GitHubProviderConfig {
                id,
                api_url: resolve_required(raw.api_url, format!("{base_path}.api_url"), env)?,
                oauth_client_id: resolve_required(
                    raw.oauth_client_id,
                    format!("{base_path}.oauth_client_id"),
                    env,
                )?,
                oauth_client_secret: resolve_required(
                    raw.oauth_client_secret,
                    format!("{base_path}.oauth_client_secret"),
                    env,
                )?,
            })),
            unsupported => invalid_config(
                format!("{base_path}.type"),
                format!("unsupported repository provider type {unsupported:?}"),
            ),
        }
    }
}

/// GitHub repository-provider configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubProviderConfig {
    /// Configured repository provider ID.
    pub id: String,
    /// GitHub API base URL, such as `https://api.github.com`.
    pub api_url: String,
    /// OAuth client ID used for GitHub login.
    pub oauth_client_id: String,
    /// OAuth client secret used for token exchange.
    pub oauth_client_secret: String,
}

/// Configured storage-provider entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StorageProviderConfig {
    /// Google Drive storage provider configuration.
    GoogleDrive(GoogleDriveStorageConfig),
}

impl StorageProviderConfig {
    /// Returns the configured storage provider ID.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::GoogleDrive(config) => &config.id,
        }
    }

    /// Returns the configured storage provider type.
    #[must_use]
    pub fn provider_type(&self) -> &'static str {
        match self {
            Self::GoogleDrive(_) => "google_drive",
        }
    }

    fn from_raw(
        id: String,
        raw: RawStorageProviderConfig,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        validate_key(&id, format!("storage_providers.{id}"))?;
        let base_path = format!("storage_providers.{id}");
        let provider_type = resolve_required(raw.provider_type, format!("{base_path}.type"), env)?;

        match provider_type.as_str() {
            "google_drive" => Ok(Self::GoogleDrive(GoogleDriveStorageConfig {
                id,
                credential_ref: resolve_required(
                    raw.credential_ref,
                    format!("{base_path}.credentials_ref"),
                    env,
                )?,
                root_folder_id: resolve_required(
                    raw.root_folder_id,
                    format!("{base_path}.root_folder_id"),
                    env,
                )?,
                display_name: resolve_optional(
                    raw.display_name,
                    format!("{base_path}.display_name"),
                    env,
                )?,
            })),
            unsupported => invalid_config(
                format!("{base_path}.type"),
                format!("unsupported storage provider type {unsupported:?}"),
            ),
        }
    }
}

/// Google Drive storage-provider configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDriveStorageConfig {
    /// Configured storage provider ID.
    pub id: String,
    /// Credential reference used to locate the server-side Drive credential.
    pub credential_ref: String,
    /// Google Drive folder ID that contains this provider's LFS objects.
    pub root_folder_id: String,
    /// Optional operator-facing label for this Drive backend.
    pub display_name: Option<String>,
}

/// Explicit repository-to-storage mapping served by this instance.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RepositoryMapping {
    /// Stable mapping ID used in server config and metadata records.
    pub id: String,
    /// Configured repository-provider ID.
    pub repo_provider: String,
    /// Repository host, such as `github.com`.
    pub host: String,
    /// Repository owner or namespace.
    pub owner: String,
    /// Repository name without the `.git` suffix.
    pub name: String,
    /// Configured storage-provider ID.
    pub storage_provider: String,
}

impl RepositoryMapping {
    /// Returns the Git LFS route path for this repository mapping.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::RepositoryMapping;
    ///
    /// let mapping = RepositoryMapping {
    ///     id: "github-main:owner/repo".to_owned(),
    ///     repo_provider: "github-main".to_owned(),
    ///     host: "github.com".to_owned(),
    ///     owner: "owner".to_owned(),
    ///     name: "repo".to_owned(),
    ///     storage_provider: "drive-user-a".to_owned(),
    /// };
    ///
    /// assert_eq!(mapping.route_path(), "/github.com/owner/repo.git/info/lfs");
    /// ```
    #[must_use]
    pub fn route_path(&self) -> String {
        format!("/{}/{}/{}.git/info/lfs", self.host, self.owner, self.name)
    }

    fn from_raw(
        index: usize,
        raw: RawRepositoryMapping,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        let base_path = format!("repositories[{index}]");
        Ok(Self {
            id: resolve_required(raw.id, format!("{base_path}.id"), env)?,
            repo_provider: resolve_required(
                raw.repo_provider,
                format!("{base_path}.repo_provider"),
                env,
            )?,
            host: resolve_required(raw.host, format!("{base_path}.host"), env)?,
            owner: resolve_required(raw.owner, format!("{base_path}.owner"), env)?,
            name: resolve_required(raw.name, format!("{base_path}.name"), env)?,
            storage_provider: resolve_required(
                raw.storage_provider,
                format!("{base_path}.storage_provider"),
                env,
            )?,
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerConfig {
    #[serde(default)]
    server: RawServerSettings,
    #[serde(default)]
    repository_providers: BTreeMap<String, RawRepositoryProviderConfig>,
    #[serde(default)]
    storage_providers: BTreeMap<String, RawStorageProviderConfig>,
    #[serde(default)]
    repositories: Vec<RawRepositoryMapping>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawServerSettings {
    #[serde(default = "default_bind_host")]
    host: Option<String>,
    #[serde(default = "default_bind_port")]
    port: u16,
    #[serde(default)]
    public_url: Option<String>,
}

impl Default for RawServerSettings {
    fn default() -> Self {
        Self {
            host: default_bind_host(),
            port: default_bind_port(),
            public_url: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepositoryProviderConfig {
    #[serde(default, rename = "type")]
    provider_type: Option<String>,
    #[serde(default)]
    api_url: Option<String>,
    #[serde(default)]
    oauth_client_id: Option<String>,
    #[serde(default)]
    oauth_client_secret: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawStorageProviderConfig {
    #[serde(default, rename = "type")]
    provider_type: Option<String>,
    #[serde(default, rename = "credentials_ref", alias = "credential_ref")]
    credential_ref: Option<String>,
    #[serde(default)]
    root_folder_id: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRepositoryMapping {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    repo_provider: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    storage_provider: Option<String>,
}

fn default_bind_host() -> Option<String> {
    Some(DEFAULT_BIND_HOST.to_owned())
}

const fn default_bind_port() -> u16 {
    DEFAULT_BIND_PORT
}

fn resolve_required(
    value: Option<String>,
    path: impl Into<String>,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<String> {
    let path = path.into();
    let value = value
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| invalid_config_error(&path, "is required"))?;
    let value = interpolate_env(&value, &path, env)?;
    if value.trim().is_empty() {
        return invalid_config(path, "is required");
    }

    Ok(value)
}

fn resolve_optional(
    value: Option<String>,
    path: impl Into<String>,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<Option<String>> {
    value
        .filter(|value| !value.trim().is_empty())
        .map(|value| interpolate_env(&value, &path.into(), env))
        .transpose()
}

fn interpolate_env(
    value: &str,
    path: &str,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<String> {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;

    while let Some(start) = rest.find("${") {
        output.push_str(&rest[..start]);
        let after_start = &rest[start + 2..];
        let Some(end) = after_start.find('}') else {
            return invalid_config(path, "contains an unterminated environment reference");
        };
        let name = &after_start[..end];
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        {
            return invalid_config(
                path,
                format!("contains invalid environment variable reference {name:?}"),
            );
        }
        let resolved = env(name).ok_or_else(|| {
            invalid_config_error(
                path,
                format!("references unset environment variable {name}"),
            )
        })?;
        output.push_str(&resolved);
        rest = &after_start[end + 1..];
    }

    output.push_str(rest);
    Ok(output)
}

fn validate_key(key: &str, path: impl Into<String>) -> ServerResult<()> {
    if key.trim().is_empty() {
        return invalid_config(path, "must not be empty");
    }

    Ok(())
}

fn invalid_config<T>(path: impl Into<String>, message: impl Into<String>) -> ServerResult<T> {
    Err(invalid_config_error(path, message))
}

fn invalid_config_error(path: impl Into<String>, message: impl Into<String>) -> ServerError {
    ServerError::InvalidConfiguration {
        message: format!("{} {}", path.into(), message.into()),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::{
        GitHubProviderConfig, GoogleDriveStorageConfig, RepositoryProviderConfig, ServerConfig,
        ServerError, StorageProviderConfig,
    };

    fn valid_yaml() -> &'static str {
        r#"
server:
  host: 127.0.0.1
  port: 8081
  public_url: http://127.0.0.1:8081

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: ${GITHUB_CLIENT_ID}
    oauth_client_secret: ${GITHUB_CLIENT_SECRET}

storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: ${DRIVE_CREDENTIAL_REF}
    root_folder_id: drive-root-folder
    display_name: Main Drive

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    storage_provider: drive-user-a
"#
    }

    fn test_env(name: &str) -> Option<String> {
        BTreeMap::from([
            ("GITHUB_CLIENT_ID", "client-id"),
            ("GITHUB_CLIENT_SECRET", "client-secret"),
            ("DRIVE_CREDENTIAL_REF", "drive-credential"),
        ])
        .get(name)
        .map(ToString::to_string)
    }

    fn load_with_test_env(contents: &str) -> ServerConfig {
        ServerConfig::load_from_str_with_env(contents, "<test>", test_env)
            .expect("config should load")
    }

    #[test]
    fn parses_github_and_drive_server_config() {
        let config = load_with_test_env(valid_yaml());

        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8081);
        assert_eq!(config.server.public_url, "http://127.0.0.1:8081");
        assert_eq!(
            config.repositories[0].route_path(),
            "/github.com/owner/repo.git/info/lfs"
        );

        let RepositoryProviderConfig::GitHub(GitHubProviderConfig {
            api_url,
            oauth_client_id,
            oauth_client_secret,
            ..
        }) = &config.repository_providers["github-main"];
        assert_eq!(api_url, "https://api.github.com");
        assert_eq!(oauth_client_id, "client-id");
        assert_eq!(oauth_client_secret, "client-secret");

        let StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig {
            credential_ref,
            root_folder_id,
            display_name,
            ..
        }) = &config.storage_providers["drive-user-a"];
        assert_eq!(credential_ref, "drive-credential");
        assert_eq!(root_folder_id, "drive-root-folder");
        assert_eq!(display_name.as_deref(), Some("Main Drive"));
    }

    #[test]
    fn explicit_path_loading_reads_yaml_file() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let config_path = directory.path().join("custom-lfs-cloud.yml");
        fs::write(
            &config_path,
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
"#,
        )
        .expect("config fixture should be written");

        let config = ServerConfig::load_from_path(&config_path).expect("config should load");

        assert_eq!(config.server.public_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn default_path_is_lfs_cloud_yml() {
        assert_eq!(
            ServerConfig::default_path(),
            std::path::Path::new("lfs-cloud.yml")
        );
    }

    #[test]
    fn server_bind_defaults_to_localhost_when_host_and_port_are_omitted() {
        let config = load_with_test_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
"#,
        );

        assert_eq!(config.server.host, "127.0.0.1");
        assert_eq!(config.server.port, 8080);
    }

    #[test]
    fn missing_environment_reference_names_exact_key_path() {
        let error =
            ServerConfig::load_from_str_with_env(valid_yaml(), "<test>", |_| None).unwrap_err();

        assert_error_contains(
            &error,
            "repository_providers.github-main.oauth_client_id references unset environment variable GITHUB_CLIENT_ID",
        );
    }

    #[test]
    fn required_environment_references_must_not_resolve_to_empty_strings() {
        let error =
            ServerConfig::load_from_str_with_env(valid_yaml(), "<test>", |name| match name {
                "GITHUB_CLIENT_ID" => Some("client-id".to_owned()),
                "GITHUB_CLIENT_SECRET" => Some("client-secret".to_owned()),
                "DRIVE_CREDENTIAL_REF" => Some(String::new()),
                _ => None,
            })
            .unwrap_err();

        assert_error_contains(
            &error,
            "storage_providers.drive-user-a.credentials_ref is required",
        );
    }

    #[test]
    fn accepts_legacy_singular_storage_credential_ref_key() {
        let config = load_with_test_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
"#,
        );

        let StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig { credential_ref, .. }) =
            &config.storage_providers["drive-user-a"];
        assert_eq!(credential_ref, "drive-credential");
    }

    #[test]
    fn validates_required_github_fields_with_exact_paths() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    oauth_client_id: client-id
    oauth_client_secret: client-secret
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repository_providers.github-main.api_url is required",
        );
    }

    #[test]
    fn validates_required_google_drive_fields_with_exact_paths() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "storage_providers.drive-user-a.root_folder_id is required",
        );
    }

    #[test]
    fn validates_repository_provider_references() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    storage_provider: drive-user-a
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repositories[0].repo_provider references unknown repository provider \"github-main\"",
        );
    }

    #[test]
    fn validates_storage_provider_references() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    storage_provider: drive-user-a
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repositories[0].storage_provider references unknown storage provider \"drive-user-a\"",
        );
    }

    #[test]
    fn rejects_duplicate_repository_ids() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
repositories:
  - id: duplicate
    repo_provider: github-main
    host: github.com
    owner: owner-a
    name: repo-a
    storage_provider: drive-user-a
  - id: duplicate
    repo_provider: github-main
    host: github.com
    owner: owner-b
    name: repo-b
    storage_provider: drive-user-a
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repositories[1].id duplicates repository id \"duplicate\"",
        );
    }

    #[test]
    fn rejects_duplicate_route_paths() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
repositories:
  - id: one
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    storage_provider: drive-user-a
  - id: two
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    storage_provider: drive-user-a
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repositories[1].route_path duplicates configured route path \"/github.com/owner/repo.git/info/lfs\"",
        );
    }

    #[test]
    fn yaml_parser_rejects_duplicate_provider_ids() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert!(matches!(error, ServerError::ConfigParse { .. }));
        assert_error_contains(&error, "duplicated key in mapping");
    }

    #[test]
    fn yaml_parser_rejects_duplicate_storage_ids() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert!(matches!(error, ServerError::ConfigParse { .. }));
        assert_error_contains(&error, "duplicated key in mapping");
    }

    #[test]
    fn rejects_unsupported_provider_types_with_exact_paths() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  gitlab-main:
    type: gitlab
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repository_providers.gitlab-main.type unsupported repository provider type \"gitlab\"",
        );
    }

    fn assert_error_contains(error: &ServerError, expected: &str) {
        let message = error.to_string();
        assert!(
            message.contains(expected),
            "expected error {message:?} to contain {expected:?}"
        );
    }
}
