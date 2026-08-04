//! Repository and storage provider configuration types.

use std::{
    fmt,
    path::{Path, PathBuf},
};

use serde::Deserialize;

use super::{
    resolution::{resolve_config_directory, resolve_optional, resolve_required},
    validation::{invalid_config, validate_config_http_url, validate_key},
};
use crate::ServerResult;

#[cfg(not(windows))]
pub(super) const DEFAULT_GCLOUD_EXECUTABLE: &str = "gcloud";
#[cfg(windows)]
pub(super) const DEFAULT_GCLOUD_EXECUTABLE: &str = "gcloud.cmd";

/// Default REST API base for the public GitHub service.
pub const DEFAULT_GITHUB_API_URL: &str = "https://api.github.com";

/// Configured repository-provider entry.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RepositoryProviderConfig {
    /// GitHub repository provider configuration.
    GitHub(GitHubProviderConfig),
}

impl RepositoryProviderConfig {
    pub(super) fn from_raw(
        id: String,
        raw: RawRepositoryProviderConfig,
        allow_insecure_http: bool,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        validate_key(&id, format!("repository_providers.{id}"))?;
        let base_path = format!("repository_providers.{id}");
        let provider_type = resolve_required(raw.provider_type, format!("{base_path}.type"), env)?;

        match provider_type.as_str() {
            "github" => {
                let api_path = format!("{base_path}.api_url");
                let api_url = resolve_optional(raw.api_url, &api_path, env)?
                    .unwrap_or_else(|| DEFAULT_GITHUB_API_URL.to_owned());
                validate_config_http_url(&api_url, &api_path, allow_insecure_http)?;
                Ok(Self::GitHub(GitHubProviderConfig {
                    id,
                    api_url,
                    authentication: GitHubAuthenticationConfig::from_raw(
                        raw.personal_access_token,
                        &base_path,
                        env,
                    )?,
                    allow_insecure_http,
                }))
            }
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
    /// Deprecated configured authentication retained for config compatibility.
    pub authentication: GitHubAuthenticationConfig,
    /// Whether non-loopback plaintext HTTP was explicitly enabled.
    pub allow_insecure_http: bool,
}

impl fmt::Debug for GitHubProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubProviderConfig")
            .field("id", &self.id)
            .field("api_url", &self.api_url)
            .field("authentication", &self.authentication)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

/// Deprecated provider PAT retained only as a session-secret fallback.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubAuthenticationConfig {
    /// Legacy configured token retained only as a session-encryption fallback.
    token: Option<String>,
}

impl GitHubAuthenticationConfig {
    /// Creates compatibility configuration from one personal access token.
    ///
    /// This constructor is retained for programmatic compatibility and tests;
    /// login authentication uses the PAT presented by each user.
    #[must_use]
    pub fn new(personal_access_token: impl Into<String>) -> Self {
        Self {
            token: Some(personal_access_token.into()),
        }
    }

    fn from_raw(
        personal_access_token: Option<String>,
        base_path: &str,
        env: &mut impl FnMut(&str) -> Option<String>,
    ) -> ServerResult<Self> {
        let token = resolve_optional(
            personal_access_token,
            format!("{base_path}.personal_access_token"),
            env,
        )?;
        Ok(Self { token })
    }

    /// Returns the deprecated configured token, when retained for compatibility.
    #[must_use]
    pub fn personal_access_token(&self) -> Option<&str> {
        self.token.as_deref()
    }
}

impl fmt::Debug for GitHubAuthenticationConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PersonalAccessToken")
            .field("token", &RedactedSecret)
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
    pub(super) fn from_raw(
        id: String,
        raw: RawStorageProviderConfig,
        env: &mut impl FnMut(&str) -> Option<String>,
        config_base_dir: &Path,
    ) -> ServerResult<Self> {
        validate_key(&id, format!("storage_providers.{id}"))?;
        let base_path = format!("storage_providers.{id}");
        let provider_type = resolve_required(raw.provider_type, format!("{base_path}.type"), env)?;

        match provider_type.as_str() {
            "google_drive" => {
                let credentials = GoogleDriveGcloudCredentialsConfig::from_raw(
                    raw.credentials,
                    &base_path,
                    env,
                    config_base_dir,
                )?;
                Ok(Self::GoogleDrive(GoogleDriveStorageConfig {
                    id,
                    credentials,
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
                }))
            }
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
    /// Google Cloud CLI ADC settings used to obtain short-lived access tokens.
    pub credentials: GoogleDriveGcloudCredentialsConfig,
    /// Google Drive folder ID that contains this provider's LFS objects.
    pub root_folder_id: String,
    /// Optional operator-facing label for this Drive backend.
    pub display_name: Option<String>,
}

/// Google Cloud CLI settings for Google Drive Application Default Credentials.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDriveGcloudCredentialsConfig {
    /// Isolated `CLOUDSDK_CONFIG` directory containing generated ADC state.
    pub config_dir: PathBuf,
    /// Google Cloud CLI executable name or path.
    pub executable: PathBuf,
}

impl GoogleDriveGcloudCredentialsConfig {
    fn from_raw(
        credentials: Option<RawStorageCredentialsConfig>,
        storage_path: &str,
        env: &mut impl FnMut(&str) -> Option<String>,
        config_base_dir: &Path,
    ) -> ServerResult<Self> {
        match credentials {
            Some(credentials) => {
                let credentials_path = format!("{storage_path}.credentials");
                let credential_type = resolve_required(
                    credentials.credential_type,
                    format!("{credentials_path}.type"),
                    env,
                )?;
                match credential_type.as_str() {
                    "gcloud" => Ok(Self {
                        config_dir: resolve_config_directory(
                            credentials.config_dir,
                            format!("{credentials_path}.config_dir"),
                            env,
                            config_base_dir,
                        )?,
                        executable: PathBuf::from(
                            resolve_optional(
                                credentials.executable,
                                format!("{credentials_path}.executable"),
                                env,
                            )?
                            .unwrap_or_else(|| DEFAULT_GCLOUD_EXECUTABLE.to_owned()),
                        ),
                    }),
                    unsupported => invalid_config(
                        format!("{credentials_path}.type"),
                        format!("unsupported Google Drive credential type {unsupported:?}"),
                    ),
                }
            }
            None => invalid_config(format!("{storage_path}.credentials"), "is required"),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawRepositoryProviderConfig {
    #[serde(default, rename = "type")]
    pub(super) provider_type: Option<String>,
    #[serde(default)]
    pub(super) api_url: Option<String>,
    #[serde(default)]
    pub(super) personal_access_token: Option<String>,
}

impl fmt::Debug for RawRepositoryProviderConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RawRepositoryProviderConfig")
            .field("provider_type", &self.provider_type)
            .field("api_url", &self.api_url)
            .field("personal_access_token", &RedactedSecret)
            .finish()
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawStorageProviderConfig {
    #[serde(default, rename = "type")]
    pub(super) provider_type: Option<String>,
    #[serde(default)]
    pub(super) credentials: Option<RawStorageCredentialsConfig>,
    #[serde(default)]
    pub(super) root_folder_id: Option<String>,
    #[serde(default)]
    pub(super) display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawStorageCredentialsConfig {
    #[serde(default, rename = "type")]
    pub(super) credential_type: Option<String>,
    #[serde(default)]
    pub(super) config_dir: Option<String>,
    #[serde(default)]
    pub(super) executable: Option<String>,
}
