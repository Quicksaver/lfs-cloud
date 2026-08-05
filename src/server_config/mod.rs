//! Typed loading and validation for private `lfscloud.yml` server config.
//!
//! The config file is server-owned state. Repository `.lfsconfig` files only
//! point Git LFS clients at an LFS Cloud route; this module decides which
//! configured repository provider and storage provider handle that route.

mod config;
mod providers;
mod repository;
mod resolution;
mod settings;
mod validation;

pub use config::ServerConfig;
pub use providers::{
    DEFAULT_GITHUB_API_URL, GitHubAuthenticationConfig, GitHubProviderConfig,
    GoogleDriveGcloudCredentialsConfig, GoogleDriveStorageConfig, RepositoryProviderConfig,
    StorageProviderConfig,
};
pub use repository::RepositoryMapping;
pub(crate) use resolution::{resolve_config_directory, resolve_optional};
pub use settings::{ServerSessionEncryptionSecret, ServerSettings};

/// Default server config filename appended to the user's home directory.
pub const DEFAULT_CONFIG_PATH: &str = "lfscloud.yml";
/// Default metadata state directory relative to the config file.
pub const DEFAULT_METADATA_DIR: &str = ".lfscloud";
/// Default SQLite metadata database filename.
pub const DEFAULT_METADATA_DB_FILE: &str = "metadata.sqlite3";
