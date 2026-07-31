//! Top-level server configuration loading and cross-reference validation.

use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
};

use config::{Config, File, FileFormat};
use serde::Deserialize;

use super::{
    DEFAULT_CONFIG_PATH, RepositoryMapping, RepositoryProviderConfig, ServerSettings,
    StorageProviderConfig,
    providers::{RawRepositoryProviderConfig, RawStorageProviderConfig},
    repository::RawRepositoryMapping,
    settings::RawServerSettings,
    validation::{invalid_config, invalid_config_error},
};
use crate::{ServerError, ServerResult};

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
    /// use lfscloud::{ServerConfig, DEFAULT_CONFIG_PATH};
    ///
    /// assert_eq!(ServerConfig::default_path(), std::path::Path::new(DEFAULT_CONFIG_PATH));
    /// ```
    #[must_use]
    pub fn default_path() -> &'static Path {
        Path::new(DEFAULT_CONFIG_PATH)
    }

    /// Loads `lfscloud.yml` from the default path.
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
                StorageProviderConfig::from_raw(id, provider, &mut env, metadata_base_dir)
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

            let repository_provider = self
                .repository_providers
                .get(&repository.repo_provider)
                .ok_or_else(|| {
                    invalid_config_error(
                        format!("{repo_path}.repo_provider"),
                        format!(
                            "references unknown repository provider {:?}",
                            repository.repo_provider
                        ),
                    )
                })?;
            repository_provider.validate_mapping(repository, &repo_path)?;
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
            let route_comparison_key = if repository_provider.route_identity_is_case_insensitive() {
                route_path.to_ascii_lowercase()
            } else {
                route_path.clone()
            };
            if !route_paths.insert(route_comparison_key) {
                return invalid_config(
                    format!("{repo_path}.route_path"),
                    format!("duplicates configured route path {route_path:?}"),
                );
            }
        }

        Ok(())
    }

    pub(crate) fn repository_mapping_for_identity(
        &self,
        host: &str,
        owner: &str,
        name: &str,
    ) -> Option<&RepositoryMapping> {
        self.repositories.iter().find(|repository| {
            repository.host.eq_ignore_ascii_case(host)
                && if self.repository_mapping_is_case_insensitive(repository) {
                    repository.owner.eq_ignore_ascii_case(owner)
                        && repository.name.eq_ignore_ascii_case(name)
                } else {
                    repository.owner == owner && repository.name == name
                }
        })
    }

    pub(crate) fn repository_mapping_is_case_insensitive(
        &self,
        repository: &RepositoryMapping,
    ) -> bool {
        self.repository_providers
            .get(&repository.repo_provider)
            .is_some_and(RepositoryProviderConfig::route_identity_is_case_insensitive)
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

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        fs,
        path::{Path, PathBuf},
    };

    use super::{
        super::{
            DEFAULT_METADATA_DB_FILE, DEFAULT_METADATA_DIR, GitHubProviderConfig,
            GoogleDriveGcloudCredentialsConfig, GoogleDriveStorageConfig, RepositoryProviderConfig,
            StorageProviderConfig,
            providers::{DEFAULT_GCLOUD_EXECUTABLE, RawRepositoryProviderConfig},
            validation::validate_config_http_url,
        },
        ServerConfig,
    };
    use crate::ServerError;

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
    personal_access_token: ${GITHUB_PAT}

storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
        BTreeMap::from([("GITHUB_PAT", "github_pat_secret")])
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
        assert_eq!(config.server.max_concurrent_requests, 64);
        assert_eq!(config.server.max_concurrent_uploads, 8);
        assert_eq!(config.server.max_concurrent_uploads_per_user, 2);
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
                authentication,
                ..
            }) => {
                assert_eq!(api_url, "https://api.github.com");
                assert_eq!(authentication.personal_access_token(), "github_pat_secret");
            }
        }

        let StorageProviderConfig::GoogleDrive(GoogleDriveStorageConfig {
            credentials,
            root_folder_id,
            display_name,
            ..
        }) = &config.storage_providers["drive-user-a"];
        assert_eq!(
            credentials,
            &GoogleDriveGcloudCredentialsConfig {
                config_dir: PathBuf::from(".gcloud-drive"),
                executable: PathBuf::from(DEFAULT_GCLOUD_EXECUTABLE),
            }
        );
        assert_eq!(root_folder_id, "drive-root-folder");
        assert_eq!(display_name.as_deref(), Some("Main Drive"));
    }

    #[test]
    fn parses_github_personal_access_token_authentication() {
        let config = load_with_test_env(valid_yaml());
        let RepositoryProviderConfig::GitHub(provider) =
            &config.repository_providers["github-main"];

        assert_eq!(
            provider.authentication.personal_access_token(),
            "github_pat_secret"
        );
    }

    #[test]
    fn provider_factories_preserve_configured_identity_and_metadata() {
        let config = load_with_test_env(valid_yaml());
        let repository_provider = &config.repository_providers["github-main"];
        let storage_provider = &config.storage_providers["drive-user-a"];

        assert_eq!(
            repository_provider.build_provider().provider_id(),
            "github-main"
        );
        assert_eq!(repository_provider.provider_type(), "github");
        assert_eq!(storage_provider.provider_type(), "google_drive");
        assert_eq!(storage_provider.backend_root_id(), "drive-root-folder");
        assert_eq!(storage_provider.display_name(), Some("Main Drive"));
    }

    #[test]
    fn single_github_pat_provider_centralizes_authentication_composition() {
        let mut config = load_with_test_env(valid_yaml());
        let provider = config
            .single_github_pat_provider("the test PAT consumer")
            .expect("one GitHub provider should compose")
            .expect("one GitHub provider should be selected");
        assert_eq!(provider.id, "github-main");

        let second = match &config.repository_providers["github-main"] {
            RepositoryProviderConfig::GitHub(provider) => {
                let mut provider = provider.clone();
                provider.id = "github-secondary".to_owned();
                RepositoryProviderConfig::GitHub(provider)
            }
        };
        config
            .repository_providers
            .insert("github-secondary".to_owned(), second);

        let error = config
            .single_github_pat_provider("the test PAT consumer")
            .expect_err("single-account PAT composition must reject two GitHub providers");
        assert_error_contains(
            &error,
            "multiple GitHub repository providers are not yet supported by the test PAT consumer",
        );
    }

    #[test]
    fn rejects_removed_github_oauth_fields() {
        for (key, value) in [
            ("oauth_client_id", "client-id"),
            ("oauth_client_secret", "client-secret"),
        ] {
            let contents = valid_yaml().replace(
                "    personal_access_token: ${GITHUB_PAT}",
                &format!("    personal_access_token: ${{GITHUB_PAT}}\n    {key}: {value}"),
            );
            let error = ServerConfig::load_from_str_with_env(contents.as_str(), "<test>", test_env)
                .expect_err("removed GitHub OAuth field should be rejected");

            assert!(matches!(error, ServerError::ConfigParse { .. }));
            assert!(
                error.to_string().contains(key),
                "unexpected diagnostic for {key}: {error}"
            );
        }
    }

    #[test]
    fn requires_github_personal_access_token() {
        let contents = valid_yaml().replace("    personal_access_token: ${GITHUB_PAT}\n", "");
        let error = ServerConfig::load_from_str_with_env(contents.as_str(), "<test>", test_env)
            .expect_err("missing GitHub PAT should be rejected");

        assert_error_contains(
            &error,
            "repository_providers.github-main.personal_access_token is required",
        );
    }

    #[test]
    fn parses_google_drive_gcloud_credentials() {
        let config = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ${GCLOUD_CONFIG_DIR}
      executable: /opt/google-cloud-sdk/bin/gcloud
    root_folder_id: drive-root
"#,
            "<test>",
            |name| match name {
                "GCLOUD_CONFIG_DIR" => Some("/srv/lfscloud/gcloud-drive".to_owned()),
                _ => test_env(name),
            },
        )
        .expect("gcloud ADC config should load");

        let StorageProviderConfig::GoogleDrive(storage) = &config.storage_providers["drive-user-a"];
        let credentials = &storage.credentials;
        assert_eq!(
            credentials.config_dir,
            PathBuf::from("/srv/lfscloud/gcloud-drive")
        );
        assert_eq!(
            credentials.executable,
            PathBuf::from("/opt/google-cloud-sdk/bin/gcloud")
        );
    }

    #[test]
    fn google_drive_gcloud_defaults_to_gcloud_executable() {
        let config = load_with_test_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
    root_folder_id: drive-root
"#,
        );

        let StorageProviderConfig::GoogleDrive(storage) = &config.storage_providers["drive-user-a"];
        let credentials = &storage.credentials;
        assert_eq!(credentials.config_dir, PathBuf::from(".gcloud-drive"));
        let expected = if cfg!(windows) {
            PathBuf::from("gcloud.cmd")
        } else {
            PathBuf::from("gcloud")
        };
        assert_eq!(credentials.executable, expected);
    }

    #[test]
    fn google_drive_rejects_legacy_credentials_ref() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: drive-credential
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
    root_folder_id: drive-root
"#,
            "<test>",
            test_env,
        )
        .expect_err("legacy credential references must be rejected");

        assert!(matches!(error, ServerError::ConfigParse { .. }));
        assert!(error.to_string().contains("credentials_ref"));
    }

    #[test]
    fn google_drive_gcloud_requires_config_dir() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
    root_folder_id: drive-root
"#,
            "<test>",
            test_env,
        )
        .expect_err("gcloud ADC config directory should be required");

        assert_error_contains(
            &error,
            "storage_providers.drive-user-a.credentials.config_dir is required",
        );
    }

    #[test]
    fn google_drive_rejects_old_gcloud_adc_type_name() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud_adc
      config_dir: .gcloud-drive
    root_folder_id: drive-root
"#,
            "<test>",
            test_env,
        )
        .expect_err("only the gcloud credential type should be accepted");

        assert_error_contains(
            &error,
            "unsupported Google Drive credential type \"gcloud_adc\"",
        );
    }

    #[test]
    fn parses_server_provider_work_limits() {
        let config = load_with_test_env(&valid_yaml().replace(
            "  public_url: http://127.0.0.1:8081",
            "  public_url: http://127.0.0.1:8081\n  max_batch_objects: 25\n  max_provider_calls: 4\n  max_concurrent_requests: 8\n  max_concurrent_uploads: 6\n  max_concurrent_uploads_per_user: 3",
        ));

        assert_eq!(config.server.max_batch_objects, 25);
        assert_eq!(config.server.max_provider_calls, 4);
        assert_eq!(config.server.max_concurrent_requests, 8);
        assert_eq!(config.server.max_concurrent_uploads, 6);
        assert_eq!(config.server.max_concurrent_uploads_per_user, 3);
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
            (
                "max_concurrent_requests",
                "server.max_concurrent_requests must be greater than zero",
            ),
            (
                "max_concurrent_uploads",
                "server.max_concurrent_uploads must be greater than zero",
            ),
            (
                "max_concurrent_uploads_per_user",
                "server.max_concurrent_uploads_per_user must be greater than zero",
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
    fn rejects_per_user_upload_limit_above_global_limit() {
        let contents = valid_yaml().replace(
            "  public_url: http://127.0.0.1:8081",
            "  public_url: http://127.0.0.1:8081\n  max_concurrent_uploads: 2\n  max_concurrent_uploads_per_user: 3",
        );
        let error = ServerConfig::load_from_str_with_env(contents.as_str(), "<test>", test_env)
            .expect_err("per-user upload limit above global limit should be rejected");

        assert_error_contains(
            &error,
            "server.max_concurrent_uploads_per_user must not exceed server.max_concurrent_uploads",
        );
    }

    #[test]
    fn server_config_debug_redacts_github_personal_access_token() {
        let config = load_with_test_env(valid_yaml());
        let RepositoryProviderConfig::GitHub(provider) =
            &config.repository_providers["github-main"];
        let configured_secret = provider.authentication.personal_access_token();
        let rendered = format!("{config:?}");

        assert!(rendered.contains("GitHubProviderConfig"));
        assert!(rendered.contains("PersonalAccessToken"));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(configured_secret));
    }

    #[test]
    fn literal_github_personal_access_token_is_redacted() {
        let contents = valid_yaml().replace("${GITHUB_PAT}", "github_pat_configured_secret");
        let config = ServerConfig::load_from_str_with_env(contents.as_str(), "<test>", |_| None)
            .expect("PAT config should load");
        let rendered = format!("{config:?}");

        assert!(rendered.contains("PersonalAccessToken"));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("github_pat_configured_secret"));
    }

    #[test]
    fn raw_repository_provider_debug_redacts_github_personal_access_token() {
        let raw = RawRepositoryProviderConfig {
            provider_type: Some("github".to_owned()),
            api_url: Some("https://api.github.com".to_owned()),
            personal_access_token: Some("raw-github-pat".to_owned()),
        };
        let configured_secret = raw
            .personal_access_token
            .as_deref()
            .expect("fixture should include a secret");
        let rendered = format!("{raw:?}");

        assert!(rendered.contains("RawRepositoryProviderConfig"));
        assert!(rendered.contains("personal_access_token"));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(configured_secret));
    }

    #[test]
    fn explicit_path_loading_reads_yaml_file() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let config_path = directory.path().join("custom-lfscloud.yml");
        fs::write(
            &config_path,
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    fn relative_gcloud_directory_resolves_from_config_directory() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let config_path = directory.path().join("custom-lfscloud.yml");
        fs::write(
            &config_path,
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: secrets/gcloud-drive
    root_folder_id: drive-root
"#,
        )
        .expect("config fixture should be written");

        let config = ServerConfig::load_from_path(&config_path).expect("config should load");
        let StorageProviderConfig::GoogleDrive(storage) = &config.storage_providers["drive-user-a"];
        let credentials = &storage.credentials;

        assert_eq!(
            credentials.config_dir,
            directory.path().join("secrets/gcloud-drive")
        );
    }

    #[test]
    fn default_path_is_lfscloud_yml() {
        assert_eq!(
            ServerConfig::default_path(),
            std::path::Path::new("lfscloud.yml")
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
        let config_path = directory.path().join("config").join("lfscloud.yml");
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
            "./lfscloud.yml",
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
            "repository_providers.github-main.personal_access_token references unset environment variable GITHUB_PAT",
        );
    }

    #[test]
    fn gcloud_config_directory_environment_reference_must_not_be_empty() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: ${GCLOUD_CONFIG_DIR}
    root_folder_id: drive-root
"#,
            "<test>",
            |name| (name == "GCLOUD_CONFIG_DIR").then(String::new),
        )
        .unwrap_err();

        assert_error_contains(
            &error,
            "storage_providers.drive-user-a.credentials.config_dir is required",
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
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    fn rejects_legacy_singular_storage_credential_ref_key() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
storage_providers:
  drive-user-a:
    type: google_drive
    credential_ref: drive-credential
    root_folder_id: drive-root
"#,
            "<test>",
            test_env,
        )
        .expect_err("legacy singular credential reference must be rejected");

        assert!(matches!(error, ServerError::ConfigParse { .. }));
        assert!(error.to_string().contains("credential_ref"));
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
    personal_access_token: github-pat
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
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    personal_access_token: github-pat
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
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    fn rejects_github_route_paths_that_differ_only_by_case() {
        let error = ServerConfig::load_from_str_with_env(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
    root_folder_id: drive-root
repositories:
  - id: one
    repo_provider: github-main
    host: github.com
    owner: Owner
    name: Repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
  - id: two
    repo_provider: github-main
    host: GITHUB.COM
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
            "repositories[1].route_path duplicates configured route path \"/GITHUB.COM/owner/repo.git/info/lfs\"",
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
    personal_access_token: github-pat
  github-main:
    type: github
    api_url: https://api.github.com
    personal_access_token: github-pat
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
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
    root_folder_id: drive-root
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    personal_access_token: github-pat
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
    fn rejects_route_unsafe_http_url_forms_consistently() {
        for url in [
            "https://lfs.example.com/base path",
            "https://lfs.example.com/base\npath",
            "https://lfs.example.com\\base",
            "https://lfs.example.com/../base",
            "https://lfs.example.com/./base",
            "https://lfs.example.com/%2e%2e/base",
        ] {
            let error = validate_config_http_url(url, "server.public_url", false)
                .expect_err("route-unsafe URL should be rejected");

            assert!(
                matches!(error, ServerError::InvalidConfiguration { .. }),
                "{url:?}"
            );
        }
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
    personal_access_token: github-pat
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
    personal_access_token: github-pat
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
    personal_access_token: github-pat
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
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
    personal_access_token: github-pat
storage_providers:
  drive-user-a:
    type: google_drive
    credentials:
      type: gcloud
      config_dir: .gcloud-drive
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
