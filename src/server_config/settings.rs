//! Server listener, request-limit, and metadata-path settings.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::{
    resolution::{resolve_metadata_path, resolve_optional, resolve_required},
    validation::{invalid_config, validate_config_http_url},
};
use crate::ServerResult;

const DEFAULT_BIND_HOST: &str = "0.0.0.0";
const DEFAULT_BIND_PORT: u16 = 15_370;
const DEFAULT_MAX_BATCH_OBJECTS: usize = 100;
const DEFAULT_MAX_PROVIDER_CALLS: usize = 16;
const DEFAULT_MAX_CONCURRENT_REQUESTS: usize = 64;
const DEFAULT_MAX_CONCURRENT_UPLOADS: usize = 8;
const DEFAULT_MAX_CONCURRENT_UPLOADS_PER_USER: usize = 2;

/// Server listener and advertised URL settings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServerSettings {
    /// Host or interface address the server should bind to.
    pub host: String,
    /// TCP port the server should bind to.
    pub port: u16,
    /// Optional public base URL override used for Git LFS action URLs.
    ///
    /// When omitted, the server derives the direct URL from the local address
    /// of each accepted connection. Configure this for hostnames, reverse
    /// proxies, TLS termination, or path prefixes that the listening socket
    /// cannot infer.
    pub public_url: Option<String>,
    /// Whether explicitly configured non-loopback HTTP URLs are allowed.
    ///
    /// This development-only escape hatch affects an explicit public URL and
    /// repository-provider API overrides. Direct URLs inferred from accepted
    /// HTTP connections do not require this config switch. Plaintext LAN
    /// traffic still exposes credentials and object content to observers.
    pub allow_insecure_http: bool,
    /// Stable server-owned secret used to encrypt durable user sessions.
    ///
    /// This is independent of every user's GitHub credential and is redacted
    /// from debug output.
    pub session_encryption_secret: Option<ServerSessionEncryptionSecret>,
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
    /// Maximum number of requests actively handled by this server process.
    ///
    /// Admission is process-wide and rejects excess requests immediately,
    /// preventing slow request bodies from creating an unbounded waiter queue.
    pub max_concurrent_requests: usize,
    /// Maximum number of uploads holding local staging resources.
    ///
    /// The process-wide limit is separate from general HTTP admission because
    /// staged uploads retain temporary disk capacity while provider I/O
    /// completes.
    pub max_concurrent_uploads: usize,
    /// Maximum staged uploads retained by one repository-provider user.
    ///
    /// Stable provider user IDs define this boundary when available, keeping
    /// one principal from consuming every process-wide upload slot.
    pub max_concurrent_uploads_per_user: usize,
    /// Local SQLite database file path for server-owned metadata.
    ///
    /// Relative configuration values are resolved against the server config
    /// file directory, while absolute paths are preserved. The server expects
    /// to create or open this SQLite database with normal filesystem write
    /// permissions when metadata storage is initialized.
    pub metadata_path: PathBuf,
}

impl ServerSettings {
    pub(super) fn from_raw(
        raw: RawServerSettings,
        env: &mut impl FnMut(&str) -> Option<String>,
        metadata_base_dir: &Path,
    ) -> ServerResult<Self> {
        let host = resolve_required(raw.host, "server.host", env)?;
        let public_url = resolve_optional(raw.public_url, "server.public_url", env)?;
        if let Some(public_url) = &public_url {
            validate_config_http_url(public_url, "server.public_url", raw.allow_insecure_http)?;
        }
        let metadata_path = resolve_metadata_path(raw.metadata_path, metadata_base_dir, env)?;
        let session_encryption_secret = resolve_optional(
            raw.session_encryption_secret,
            "server.session_encryption_secret",
            env,
        )?
        .map(ServerSessionEncryptionSecret::new)
        .transpose()?;

        if raw.port == 0 {
            return invalid_config("server.port", "must be greater than zero");
        }
        if raw.max_batch_objects == 0 {
            return invalid_config("server.max_batch_objects", "must be greater than zero");
        }
        if raw.max_provider_calls == 0 {
            return invalid_config("server.max_provider_calls", "must be greater than zero");
        }
        if raw.max_concurrent_requests == 0 {
            return invalid_config(
                "server.max_concurrent_requests",
                "must be greater than zero",
            );
        }
        if raw.max_concurrent_uploads == 0 {
            return invalid_config("server.max_concurrent_uploads", "must be greater than zero");
        }
        if raw.max_concurrent_uploads_per_user == 0 {
            return invalid_config(
                "server.max_concurrent_uploads_per_user",
                "must be greater than zero",
            );
        }
        if raw.max_concurrent_uploads_per_user > raw.max_concurrent_uploads {
            return invalid_config(
                "server.max_concurrent_uploads_per_user",
                "must not exceed server.max_concurrent_uploads",
            );
        }

        Ok(Self {
            host,
            port: raw.port,
            public_url,
            allow_insecure_http: raw.allow_insecure_http,
            session_encryption_secret,
            max_batch_objects: raw.max_batch_objects,
            max_provider_calls: raw.max_provider_calls,
            max_concurrent_requests: raw.max_concurrent_requests,
            max_concurrent_uploads: raw.max_concurrent_uploads,
            max_concurrent_uploads_per_user: raw.max_concurrent_uploads_per_user,
            metadata_path,
        })
    }

    /// Returns the URL used by local CLI commands when no explicit server URL
    /// or public URL override is configured.
    ///
    /// Wildcard binds are intentionally represented as loopback because
    /// unspecified addresses are listener targets, not connectable endpoints.
    #[must_use]
    pub fn local_client_url(&self) -> String {
        if let Some(public_url) = &self.public_url {
            return public_url.clone();
        }

        let host = match self.host.as_str() {
            "0.0.0.0" => "127.0.0.1".to_owned(),
            "::" | "[::]" => "[::1]".to_owned(),
            host if host.contains(':') && !host.starts_with('[') => format!("[{host}]"),
            host => host.to_owned(),
        };
        format!("http://{host}:{}", self.port)
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawServerSettings {
    #[serde(default = "default_bind_host")]
    pub(super) host: Option<String>,
    #[serde(default = "default_bind_port")]
    pub(super) port: u16,
    #[serde(default)]
    pub(super) public_url: Option<String>,
    #[serde(default)]
    pub(super) allow_insecure_http: bool,
    #[serde(default)]
    pub(super) session_encryption_secret: Option<String>,
    #[serde(default = "default_max_batch_objects")]
    pub(super) max_batch_objects: usize,
    #[serde(default = "default_max_provider_calls")]
    pub(super) max_provider_calls: usize,
    #[serde(default = "default_max_concurrent_requests")]
    pub(super) max_concurrent_requests: usize,
    #[serde(default = "default_max_concurrent_uploads")]
    pub(super) max_concurrent_uploads: usize,
    #[serde(default = "default_max_concurrent_uploads_per_user")]
    pub(super) max_concurrent_uploads_per_user: usize,
    #[serde(default)]
    pub(super) metadata_path: Option<String>,
}

impl Default for RawServerSettings {
    fn default() -> Self {
        Self {
            host: default_bind_host(),
            port: default_bind_port(),
            public_url: None,
            allow_insecure_http: false,
            session_encryption_secret: None,
            max_batch_objects: default_max_batch_objects(),
            max_provider_calls: default_max_provider_calls(),
            max_concurrent_requests: default_max_concurrent_requests(),
            max_concurrent_uploads: default_max_concurrent_uploads(),
            max_concurrent_uploads_per_user: default_max_concurrent_uploads_per_user(),
            metadata_path: None,
        }
    }
}

/// Redacted server-owned secret used to protect durable LFS sessions.
#[derive(Clone, Eq, PartialEq)]
pub struct ServerSessionEncryptionSecret(String);

impl ServerSessionEncryptionSecret {
    fn new(value: String) -> ServerResult<Self> {
        if value.chars().count() < 32
            || value.trim().len() != value.len()
            || value.chars().any(|character| character.is_control())
        {
            return invalid_config(
                "server.session_encryption_secret",
                "must contain at least 32 unpadded, non-control characters",
            );
        }
        Ok(Self(value))
    }

    pub(crate) fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for ServerSessionEncryptionSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("<redacted>")
    }
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

const fn default_max_concurrent_requests() -> usize {
    DEFAULT_MAX_CONCURRENT_REQUESTS
}

const fn default_max_concurrent_uploads() -> usize {
    DEFAULT_MAX_CONCURRENT_UPLOADS
}

const fn default_max_concurrent_uploads_per_user() -> usize {
    DEFAULT_MAX_CONCURRENT_UPLOADS_PER_USER
}
