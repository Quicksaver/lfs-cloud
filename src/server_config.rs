//! Typed loading and validation for private `lfs-cloud.yml` server config.
//!
//! The config file is server-owned state. Repository `.lfsconfig` files only
//! point Git LFS clients at an LFS Cloud route; this module decides which
//! configured repository provider and storage provider handle that route.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt, fs,
    path::{Path, PathBuf},
};

use config::{Config, File, FileFormat};
use serde::Deserialize;
use url::Url;

use crate::{ServerError, ServerResult, http_transport::uses_protected_http_transport};

/// Default server config path used when no explicit path is supplied.
pub const DEFAULT_CONFIG_PATH: &str = "lfs-cloud.yml";
/// Default metadata state directory relative to the config file.
pub const DEFAULT_METADATA_DIR: &str = ".lfs-cloud";
/// Default SQLite metadata database filename.
pub const DEFAULT_METADATA_DB_FILE: &str = "metadata.sqlite3";

const DEFAULT_BIND_HOST: &str = "127.0.0.1";
const DEFAULT_BIND_PORT: u16 = 8080;
const DEFAULT_MAX_BATCH_OBJECTS: usize = 100;
const DEFAULT_MAX_PROVIDER_CALLS: usize = 16;

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

        let metadata_base_dir = path.parent().unwrap_or_else(|| Path::new("."));

        Self::load_from_str_with_env_and_base_dir(
            &contents,
            path.display().to_string(),
            metadata_base_dir,
            |name| std::env::var(name).ok(),
        )
    }

    /// Parses config from a YAML string.
    ///
    /// This is primarily useful for tests and tooling that has already loaded
    /// the config contents. Environment references are resolved against the
    /// current process environment; use `load_from_str_with_env` in tests that
    /// need deterministic environment values.
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
        Self::load_from_str_with_env_and_base_dir(contents, path, Path::new(""), env)
    }

    fn load_from_str_with_env_and_base_dir(
        contents: &str,
        path: impl Into<String>,
        metadata_base_dir: &Path,
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

        ServerConfig::from_raw(raw, env, metadata_base_dir)
    }

    fn from_raw(
        raw: RawServerConfig,
        mut env: impl FnMut(&str) -> Option<String>,
        metadata_base_dir: &Path,
    ) -> ServerResult<Self> {
        let server = ServerSettings::from_raw(raw.server, &mut env, metadata_base_dir)?;
        let repository_providers = raw
            .repository_providers
            .into_iter()
            .map(|(id, provider)| {
                RepositoryProviderConfig::from_raw(
                    id,
                    provider,
                    server.allow_insecure_http,
                    &mut env,
                )
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
            if matches!(
                self.repository_providers.get(&repository.repo_provider),
                Some(RepositoryProviderConfig::GitHub(_))
            ) && repository
                .provider_repository_id
                .parse::<u64>()
                .ok()
                .filter(|id| *id > 0)
                .is_none()
            {
                return invalid_config(
                    format!("{repo_path}.provider_repository_id"),
                    "must be a positive GitHub numeric repository ID",
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
    /// Whether non-loopback plaintext HTTP is explicitly allowed.
    ///
    /// This development-only escape hatch affects the public server URL and
    /// repository-provider API URLs. It should never be enabled on an
    /// untrusted network because those endpoints carry credentials and LFS
    /// object content.
    pub allow_insecure_http: bool,
    /// Maximum number of object entries accepted in one Git LFS batch.
    ///
    /// Duplicate entries count toward this limit even though storage lookups
    /// are deduplicated later, keeping both request work and response size
    /// bounded.
    pub max_batch_objects: usize,
    /// Maximum number of concurrent repository or storage provider calls.
    ///
    /// The limit is shared across all repositories handled by this server
    /// process so one client cannot monopolize upstream provider capacity.
    pub max_provider_calls: usize,
    /// Local SQLite database file path for server-owned metadata.
    ///
    /// Relative configuration values are resolved against the server config
    /// file directory, while absolute paths are preserved. The server expects
    /// to create or open this SQLite database with normal filesystem write
    /// permissions when metadata storage is initialized.
    pub metadata_path: PathBuf,
}

impl ServerSettings {
    fn from_raw(
        raw: RawServerSettings,
        env: &mut impl FnMut(&str) -> Option<String>,
        metadata_base_dir: &Path,
    ) -> ServerResult<Self> {
        let host = resolve_required(raw.host, "server.host", env)?;
        let public_url = resolve_required(raw.public_url, "server.public_url", env)?;
        validate_http_url(
            &public_url,
            "server.public_url",
            false,
            raw.allow_insecure_http,
        )?;
        let metadata_path = resolve_metadata_path(raw.metadata_path, metadata_base_dir, env)?;

        if raw.port == 0 {
            return invalid_config("server.port", "must be greater than zero");
        }
        if raw.max_batch_objects == 0 {
            return invalid_config("server.max_batch_objects", "must be greater than zero");
        }
        if raw.max_provider_calls == 0 {
            return invalid_config("server.max_provider_calls", "must be greater than zero");
        }

        Ok(Self {
            host,
            port: raw.port,
            public_url,
            allow_insecure_http: raw.allow_insecure_http,
            max_batch_objects: raw.max_batch_objects,
            max_provider_calls: raw.max_provider_calls,
            metadata_path,
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
        allow_insecure_http: bool,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        validate_key(&id, format!("repository_providers.{id}"))?;
        let base_path = format!("repository_providers.{id}");
        let provider_type = resolve_required(raw.provider_type, format!("{base_path}.type"), env)?;

        match provider_type.as_str() {
            "github" => Ok(Self::GitHub(GitHubProviderConfig {
                id,
                api_url: resolve_http_url(
                    raw.api_url,
                    format!("{base_path}.api_url"),
                    allow_insecure_http,
                    env,
                )?,
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
                allow_insecure_http,
            })),
            unsupported => invalid_config(
                format!("{base_path}.type"),
                format!("unsupported repository provider type {unsupported:?}"),
            ),
        }
    }
}

/// GitHub repository-provider configuration.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubProviderConfig {
    /// Configured repository provider ID.
    pub id: String,
    /// GitHub API base URL, such as `https://api.github.com`.
    pub api_url: String,
    /// OAuth client ID used for GitHub login.
    pub oauth_client_id: String,
    /// OAuth client secret used for token exchange.
    pub oauth_client_secret: String,
    /// Whether non-loopback plaintext HTTP was explicitly enabled.
    pub allow_insecure_http: bool,
}

impl fmt::Debug for GitHubProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubProviderConfig")
            .field("id", &self.id)
            .field("api_url", &self.api_url)
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_client_secret", &RedactedSecret)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

struct RedactedSecret;

impl fmt::Debug for RedactedSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("\"<redacted>\"")
    }
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
    /// Provider-stable repository ID used to detect rename and name reuse.
    pub provider_repository_id: String,
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
    ///     provider_repository_id: "123456789".to_owned(),
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
        let id = resolve_required(raw.id, format!("{base_path}.id"), env)?;
        let repo_provider =
            resolve_required(raw.repo_provider, format!("{base_path}.repo_provider"), env)?;
        let host = resolve_required(raw.host, format!("{base_path}.host"), env)?;
        validate_route_host(&host, format!("{base_path}.host"))?;
        let owner = resolve_required(raw.owner, format!("{base_path}.owner"), env)?;
        validate_route_component(&owner, format!("{base_path}.owner"))?;
        let name = resolve_required(raw.name, format!("{base_path}.name"), env)?;
        validate_route_component(&name, format!("{base_path}.name"))?;
        if name.ends_with(".git") {
            return invalid_config(
                format!("{base_path}.name"),
                "must omit the .git suffix because the route adds it",
            );
        }
        let provider_repository_id = resolve_required(
            raw.provider_repository_id,
            format!("{base_path}.provider_repository_id"),
            env,
        )?;
        let storage_provider = resolve_required(
            raw.storage_provider,
            format!("{base_path}.storage_provider"),
            env,
        )?;

        Ok(Self {
            id,
            repo_provider,
            host,
            owner,
            name,
            provider_repository_id,
            storage_provider,
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
    #[serde(default)]
    allow_insecure_http: bool,
    #[serde(default = "default_max_batch_objects")]
    max_batch_objects: usize,
    #[serde(default = "default_max_provider_calls")]
    max_provider_calls: usize,
    #[serde(default)]
    metadata_path: Option<String>,
}

impl Default for RawServerSettings {
    fn default() -> Self {
        Self {
            host: default_bind_host(),
            port: default_bind_port(),
            public_url: None,
            allow_insecure_http: false,
            max_batch_objects: default_max_batch_objects(),
            max_provider_calls: default_max_provider_calls(),
            metadata_path: None,
        }
    }
}

#[derive(Deserialize)]
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

impl fmt::Debug for RawRepositoryProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawRepositoryProviderConfig")
            .field("provider_type", &self.provider_type)
            .field("api_url", &self.api_url)
            .field("oauth_client_id", &self.oauth_client_id)
            .field("oauth_client_secret", &RedactedSecret)
            .finish()
    }
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
    provider_repository_id: Option<String>,
    #[serde(default)]
    storage_provider: Option<String>,
}

fn default_bind_host() -> Option<String> {
    Some(DEFAULT_BIND_HOST.to_owned())
}

const fn default_bind_port() -> u16 {
    DEFAULT_BIND_PORT
}

const fn default_max_batch_objects() -> usize {
    DEFAULT_MAX_BATCH_OBJECTS
}

const fn default_max_provider_calls() -> usize {
    DEFAULT_MAX_PROVIDER_CALLS
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

fn resolve_http_url(
    value: Option<String>,
    path: impl Into<String>,
    allow_insecure_http: bool,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<String> {
    let path = path.into();
    let value = resolve_required(value, &path, env)?;
    validate_http_url(&value, &path, false, allow_insecure_http)?;
    Ok(value)
}

fn resolve_metadata_path(
    value: Option<String>,
    metadata_base_dir: &Path,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<PathBuf> {
    let path = match value {
        Some(value) => {
            if value.trim().is_empty() {
                return invalid_config("server.metadata_path", "must not be empty");
            }
            let value = interpolate_env(&value, "server.metadata_path", env)?;
            if value.trim().is_empty() {
                return invalid_config(
                    "server.metadata_path",
                    "must not resolve to an empty value",
                );
            }
            if value.trim() != value {
                return invalid_config(
                    "server.metadata_path",
                    "must not include leading or trailing whitespace",
                );
            }
            if has_trailing_path_separator(&value) {
                return invalid_config(
                    "server.metadata_path",
                    "must include a metadata database file name",
                );
            }
            PathBuf::from(value)
        }
        None => PathBuf::from(DEFAULT_METADATA_DIR).join(DEFAULT_METADATA_DB_FILE),
    };

    if path.as_os_str().is_empty() {
        return invalid_config("server.metadata_path", "must not be empty");
    }

    if path.is_absolute() || metadata_base_dir.as_os_str().is_empty() {
        Ok(path)
    } else {
        Ok(metadata_base_dir.join(path))
    }
}

fn has_trailing_path_separator(value: &str) -> bool {
    value.ends_with('/') || value.ends_with('\\')
}

fn resolve_optional(
    value: Option<String>,
    path: impl Into<String>,
    env: &mut impl FnMut(&str) -> Option<String>,
) -> ServerResult<Option<String>> {
    let path = path.into();
    let Some(value) = value.filter(|value| !value.trim().is_empty()) else {
        return Ok(None);
    };
    let value = interpolate_env(&value, &path, env)?;
    if value.trim().is_empty() {
        return Ok(None);
    }

    Ok(Some(value))
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
    let path = path.into();
    if key.trim().is_empty() {
        return invalid_config(path, "must not be empty");
    }
    if key != key.trim() {
        return invalid_config(path, "must not have leading or trailing whitespace");
    }
    if !key
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric())
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return invalid_config(
            path,
            "must contain only ASCII letters, digits, '_' or '-' and start with a letter or digit",
        );
    }

    Ok(())
}

fn validate_route_host(host: &str, path: impl Into<String>) -> ServerResult<()> {
    let path = path.into();
    validate_no_outer_whitespace(host, &path)?;
    if host.split('.').any(|label| {
        label.is_empty()
            || label.starts_with('-')
            || label.ends_with('-')
            || !label
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    }) {
        return invalid_config(
            path,
            "must be a route-safe host made of ASCII domain labels",
        );
    }

    Ok(())
}

fn validate_route_component(component: &str, path: impl Into<String>) -> ServerResult<()> {
    let path = path.into();
    validate_no_outer_whitespace(component, &path)?;
    if matches!(component, "." | "..")
        || component.contains("..")
        || !component
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return invalid_config(
            path,
            "must be a route-safe repository component without separators, percent escapes, or traversal segments",
        );
    }

    Ok(())
}

fn validate_http_url(
    url: &str,
    path: impl Into<String>,
    allow_trailing_slash: bool,
    allow_insecure_http: bool,
) -> ServerResult<()> {
    let path = path.into();
    validate_no_outer_whitespace(url, &path)?;
    if !allow_trailing_slash && url.ends_with('/') {
        return invalid_config(path, "must not end with a trailing slash");
    }
    let parsed = Url::parse(url)
        .map_err(|_| invalid_config_error(&path, "must be a valid http or https URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return invalid_config(path, "must be a valid http or https URL");
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return invalid_config(path, "must not include a query string or fragment");
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return invalid_config(path, "must not include credentials");
    }
    if !allow_insecure_http && !uses_protected_http_transport(&parsed) {
        return invalid_config(
            path,
            "must use HTTPS unless it targets an exact loopback IP; set server.allow_insecure_http to true only for a trusted development network",
        );
    }

    Ok(())
}

fn validate_no_outer_whitespace(value: &str, path: &str) -> ServerResult<()> {
    if value != value.trim() {
        return invalid_config(path, "must not have leading or trailing whitespace");
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
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
    };

    use super::{
        DEFAULT_METADATA_DB_FILE, DEFAULT_METADATA_DIR, GitHubProviderConfig,
        GoogleDriveStorageConfig, RawRepositoryProviderConfig, RepositoryProviderConfig,
        ServerConfig, ServerError, StorageProviderConfig,
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
    provider_repository_id: "8675309"
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
        assert!(!config.server.allow_insecure_http);
        assert_eq!(config.server.max_batch_objects, 100);
        assert_eq!(config.server.max_provider_calls, 16);
        assert_eq!(
            config.server.metadata_path,
            PathBuf::from(DEFAULT_METADATA_DIR).join(DEFAULT_METADATA_DB_FILE)
        );
        assert_eq!(
            config.repositories[0].route_path(),
            "/github.com/owner/repo.git/info/lfs"
        );

        match &config.repository_providers["github-main"] {
            RepositoryProviderConfig::GitHub(GitHubProviderConfig {
                api_url,
                oauth_client_id,
                oauth_client_secret,
                ..
            }) => {
                assert_eq!(api_url, "https://api.github.com");
                assert_eq!(oauth_client_id, "client-id");
                assert_eq!(oauth_client_secret, "client-secret");
            }
        }

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
    fn parses_server_provider_work_limits() {
        let config = load_with_test_env(&valid_yaml().replace(
            "  public_url: http://127.0.0.1:8081",
            "  public_url: http://127.0.0.1:8081\n  max_batch_objects: 25\n  max_provider_calls: 4",
        ));

        assert_eq!(config.server.max_batch_objects, 25);
        assert_eq!(config.server.max_provider_calls, 4);
    }

    #[test]
    fn rejects_zero_server_provider_work_limits() {
        for (key, expected) in [
            (
                "max_batch_objects",
                "server.max_batch_objects must be greater than zero",
            ),
            (
                "max_provider_calls",
                "server.max_provider_calls must be greater than zero",
            ),
        ] {
            let contents = valid_yaml().replace(
                "  public_url: http://127.0.0.1:8081",
                &format!("  public_url: http://127.0.0.1:8081\n  {key}: 0"),
            );
            let error = ServerConfig::load_from_str_with_env(contents.as_str(), "<test>", test_env)
                .expect_err("zero provider work limits should be rejected");

            assert_error_contains(&error, expected);
        }
    }

    #[test]
    fn server_config_debug_redacts_github_oauth_client_secret() {
        let config = load_with_test_env(valid_yaml());
        let RepositoryProviderConfig::GitHub(provider) =
            &config.repository_providers["github-main"];
        let configured_secret = provider.oauth_client_secret.as_str();
        let rendered = format!("{config:?}");

        assert!(rendered.contains("GitHubProviderConfig"));
        assert!(rendered.contains("oauth_client_secret"));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(configured_secret));
    }

    #[test]
    fn raw_repository_provider_debug_redacts_github_oauth_client_secret() {
        let raw = RawRepositoryProviderConfig {
            provider_type: Some("github".to_owned()),
            api_url: Some("https://api.github.com".to_owned()),
            oauth_client_id: Some("client-id".to_owned()),
            oauth_client_secret: Some("raw-client-secret".to_owned()),
        };
        let configured_secret = raw
            .oauth_client_secret
            .as_deref()
            .expect("fixture should include a secret");
        let rendered = format!("{raw:?}");

        assert!(rendered.contains("RawRepositoryProviderConfig"));
        assert!(rendered.contains("oauth_client_secret"));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(configured_secret));
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
        assert_eq!(
            config.server.metadata_path,
            directory
                .path()
                .join(DEFAULT_METADATA_DIR)
                .join(DEFAULT_METADATA_DB_FILE)
        );
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
    fn explicit_relative_metadata_path_resolves_from_config_directory() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let config_path = directory.path().join("config").join("lfs-cloud.yml");
        fs::create_dir_all(
            config_path
                .parent()
                .expect("config path should have parent"),
        )
        .expect("config directory should be created");
        fs::write(
            &config_path,
            r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: state/metadata.sqlite3
"#,
        )
        .expect("config fixture should be written");

        let config = ServerConfig::load_from_path(&config_path).expect("config should load");

        assert_eq!(
            config.server.metadata_path,
            directory
                .path()
                .join("config")
                .join("state")
                .join("metadata.sqlite3")
        );
    }

    #[test]
    fn explicit_absolute_metadata_path_is_preserved() {
        let path = tempfile::tempdir()
            .expect("tempdir should be created")
            .path()
            .join("metadata.sqlite3");
        let config = ServerConfig::load_from_str_with_env(
            &format!(
                r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: {}
"#,
                path.display()
            ),
            "<test>",
            test_env,
        )
        .expect("config should load");

        assert_eq!(config.server.metadata_path, path);
    }

    #[test]
    fn metadata_path_supports_environment_references() {
        let config = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: ${METADATA_PATH}
"#,
            "<test>",
            |name| match name {
                "METADATA_PATH" => Some("state/metadata.sqlite3".to_owned()),
                _ => test_env(name),
            },
        )
        .expect("config should load");

        assert_eq!(
            config.server.metadata_path,
            PathBuf::from("state").join("metadata.sqlite3")
        );
    }

    #[test]
    fn metadata_path_rejects_whitespace_only_values() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: "   "
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(&error, "server.metadata_path must not be empty");
    }

    #[test]
    fn metadata_path_rejects_environment_references_resolving_to_empty_values() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: ${METADATA_PATH}
"#,
            "<test>",
            |name| match name {
                "METADATA_PATH" => Some(String::new()),
                _ => test_env(name),
            },
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "server.metadata_path must not resolve to an empty value",
        );
    }

    #[test]
    fn metadata_path_rejects_environment_references_resolving_to_outer_whitespace() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: ${METADATA_PATH}
"#,
            "<test>",
            |name| match name {
                "METADATA_PATH" => Some(" state/metadata.sqlite3 ".to_owned()),
                _ => test_env(name),
            },
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "server.metadata_path must not include leading or trailing whitespace",
        );
    }

    #[test]
    fn metadata_path_rejects_values_ending_in_path_separators() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: state/
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "server.metadata_path must include a metadata database file name",
        );
    }

    #[test]
    fn explicit_relative_metadata_path_joins_current_directory_base() {
        let config = ServerConfig::load_from_str_with_env_and_base_dir(
            r#"
server:
  public_url: http://127.0.0.1:8080
  metadata_path: state/metadata.sqlite3
"#,
            "./lfs-cloud.yml",
            Path::new("."),
            test_env,
        )
        .expect("config should load");

        assert_eq!(
            config.server.metadata_path,
            PathBuf::from(".").join("state").join("metadata.sqlite3")
        );
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
    fn optional_environment_references_empty_strings_to_none() {
        let config = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: drive-credential
    root_folder_id: drive-root
    display_name: ${EMPTY_DISPLAY_NAME}
"#,
            "<test>",
            |name| match name {
                "EMPTY_DISPLAY_NAME" => Some(String::new()),
                _ => test_env(name),
            },
        )
        .expect("config should load");

        let StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig { display_name, .. }) =
            &config.storage_providers["drive-user-a"];
        assert_eq!(display_name, &None);
    }

    #[test]
    fn interpolates_multiple_environment_references() {
        let config = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://${LFS_HOST}:${LFS_PORT}
"#,
            "<test>",
            |name| match name {
                "LFS_HOST" => Some("127.0.0.1".to_owned()),
                "LFS_PORT" => Some("8080".to_owned()),
                _ => None,
            },
        )
        .expect("config should load");

        assert_eq!(config.server.public_url, "http://127.0.0.1:8080");
    }

    #[test]
    fn invalid_environment_reference_names_exact_key_path() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://${LFS-HOST}:8080
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "server.public_url contains invalid environment variable reference \"LFS-HOST\"",
        );
    }

    #[test]
    fn unterminated_environment_reference_names_exact_key_path() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://${LFS_HOST
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "server.public_url contains an unterminated environment reference",
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
    fn requires_a_persisted_provider_repository_id() {
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
    credentials_ref: drive-credential
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

        assert_error_contains(&error, "repositories[0].provider_repository_id is required");
    }

    #[test]
    fn rejects_non_numeric_github_repository_ids() {
        let error = ServerConfig::load_from_str_with_env(
            &valid_yaml().replace(
                "provider_repository_id: \"8675309\"",
                "provider_repository_id: not-a-github-id",
            ),
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repositories[0].provider_repository_id must be a positive GitHub numeric repository ID",
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
    provider_repository_id: "8675309"
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
    provider_repository_id: "8675309"
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
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
  - id: duplicate
    repo_provider: github-main
    host: github.com
    owner: owner-b
    name: repo-b
    provider_repository_id: "8675309"
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
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
  - id: two
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
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
    fn rejects_invalid_provider_ids() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  'bad id':
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repository_providers.bad id must contain only ASCII letters, digits, '_' or '-' and start with a letter or digit",
        );
    }

    #[test]
    fn validates_public_url_shape() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: 127.0.0.1:8080
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "server.public_url must be a valid http or https URL",
        );
    }

    #[test]
    fn rejects_trailing_public_url_slashes() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080/
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "server.public_url must not end with a trailing slash",
        );
    }

    #[test]
    fn validates_repository_provider_api_url_shape() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: ftp://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repository_providers.github-main.api_url must be a valid http or https URL",
        );
    }

    #[test]
    fn requires_https_for_non_loopback_server_and_github_urls() {
        let public_url_error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://192.168.1.25:8080
"#,
            "<test>",
            test_env,
        )
        .expect_err("LAN HTTP should require an explicit unsafe opt-in");
        assert_error_contains(&public_url_error, "server.public_url must use HTTPS");

        let api_url_error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: http://github.example.test/api/v3
    oauth_client_id: client-id
    oauth_client_secret: client-secret
"#,
            "<test>",
            test_env,
        )
        .expect_err("remote GitHub HTTP should require an explicit unsafe opt-in");
        assert_error_contains(
            &api_url_error,
            "repository_providers.github-main.api_url must use HTTPS",
        );
    }

    #[test]
    fn explicit_unsafe_http_opt_in_allows_trusted_lan_development() {
        let config = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://192.168.1.25:8080
  allow_insecure_http: true
repository_providers:
  github-main:
    type: github
    api_url: http://192.168.1.30:8081/api/v3
    oauth_client_id: client-id
    oauth_client_secret: client-secret
"#,
            "<test>",
            test_env,
        )
        .expect("explicit unsafe opt-in should allow trusted LAN development URLs");

        assert!(config.server.allow_insecure_http);
        let RepositoryProviderConfig::GitHub(provider) =
            &config.repository_providers["github-main"];
        assert!(provider.allow_insecure_http);
    }

    #[test]
    fn rejects_route_unsafe_repository_components() {
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
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner/team
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repositories[0].owner must be a route-safe repository component without separators, percent escapes, or traversal segments",
        );
    }

    #[test]
    fn rejects_repository_names_with_git_suffix() {
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
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo.git
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#,
            "<test>",
            test_env,
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "repositories[0].name must omit the .git suffix because the route adds it",
        );
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
