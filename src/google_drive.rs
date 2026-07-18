//! Google Drive storage-provider authentication helpers.
//!
//! This module loads server-owned Google Drive OAuth credentials from
//! configuration references and exchanges refresh tokens for short-lived
//! access tokens. It does not expose Drive credentials to Git LFS clients.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom},
    net::IpAddr,
    path::Path,
    sync::OnceLock,
    time::Duration,
};

use axum::{
    body::{Body as AxumBody, Bytes},
    response::Response as AxumResponse,
};
use futures_util::StreamExt;
use reqwest::{
    Body as ReqwestBody, Client, StatusCode,
    header::{
        ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
        HeaderMap, HeaderValue, LOCATION, RANGE,
    },
    redirect::Policy,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::sync::watch;
use url::Url;

use crate::{
    GoogleDriveStorageConfig, LfsObject, ProviderFuture, SanitizedMessage, StorageDeleteOutcome,
    StorageError, StorageProvider, StorageResult, StoredObject,
};

const GOOGLE_DRIVE_TOKEN_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_ROOT_VALIDATION_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_OBJECT_METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GOOGLE_DRIVE_TRANSFER_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE: usize = 256 * 1024;
const GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS: u32 = 4;
const GOOGLE_DRIVE_RESUMABLE_UPLOAD_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const DEFAULT_GOOGLE_DRIVE_CREDENTIAL_ENV_PREFIX: &str = "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_";
const MAX_GOOGLE_ERROR_BODY_LEN: usize = 16 * 1024;
const MIN_REDACTED_SECRET_FRAGMENT_LEN: usize = 6;
const MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES: usize = 124;
const GOOGLE_DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
const GOOGLE_DRIVE_OBJECT_CONTENT_TYPE: &str = "application/octet-stream";
const GOOGLE_DRIVE_OBJECT_VERSION: &str = "1";
const GOOGLE_DRIVE_OBJECT_VERSION_PROPERTY: &str = "lfsCloudVersion";
const GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY: &str = "lfsCloudRepoNamespace";
const GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY: &str = "lfsCloudRepoNamespaceFormat";
const GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT: &str = "sha256";
const GOOGLE_DRIVE_OBJECT_OID_PROPERTY: &str = "lfsCloudOid";
const GOOGLE_DRIVE_OBJECT_SIZE_PROPERTY: &str = "lfsCloudSize";

static DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_OBJECT_METADATA_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

/// Google OAuth token endpoint used by Drive refresh-token exchange.
pub const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google Drive API root used for storage-provider metadata and transfer calls.
pub const GOOGLE_DRIVE_API_BASE_URL: &str = "https://www.googleapis.com";

/// MVP Google Drive OAuth scope for app-accessible LFS object storage.
pub const GOOGLE_DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

/// Loads Google Drive OAuth credentials from server-side references.
///
/// Bare credential references are resolved to environment variables by
/// uppercasing ASCII letters and replacing `-` with `_`, using the prefix
/// `LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_`. For example, `google-drive-user-a`
/// resolves to `LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_GOOGLE_DRIVE_USER_A`.
///
/// References may also use `env:NAME` to name the environment variable
/// explicitly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDriveCredentialLoader {
    env_prefix: String,
}

impl GoogleDriveCredentialLoader {
    /// Creates a loader using the default `lfs-cloud` Google Drive env prefix.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::GoogleDriveCredentialLoader;
    ///
    /// let loader = GoogleDriveCredentialLoader::new();
    /// assert!(format!("{loader:?}").contains("GoogleDriveCredentialLoader"));
    /// ```
    #[must_use]
    pub fn new() -> Self {
        Self {
            env_prefix: DEFAULT_GOOGLE_DRIVE_CREDENTIAL_ENV_PREFIX.to_owned(),
        }
    }

    /// Creates a loader with an explicit environment-variable prefix.
    ///
    /// This is primarily useful for tests and embedded server runtimes that
    /// need to keep their secret namespace separate.
    #[must_use]
    pub fn with_env_prefix(prefix: impl Into<String>) -> Self {
        Self {
            env_prefix: prefix.into(),
        }
    }

    /// Loads credentials for a configured Google Drive storage provider.
    ///
    /// The loaded environment value must be a JSON object containing
    /// `client_id`, `client_secret`, and `refresh_token`. `token_uri` is
    /// optional and defaults to Google's OAuth token endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the credential reference cannot be mapped,
    /// the environment variable is unset, or the JSON credential is invalid.
    pub fn load_from_environment(
        &self,
        storage: &GoogleDriveStorageConfig,
    ) -> StorageResult<GoogleDriveCredential> {
        self.load_from_env_with(storage, |name| std::env::var(name).ok())
    }

    fn load_from_env_with(
        &self,
        storage: &GoogleDriveStorageConfig,
        mut env: impl FnMut(&str) -> Option<String>,
    ) -> StorageResult<GoogleDriveCredential> {
        let env_var = self.env_var_for_ref(&storage.id, &storage.credential_ref)?;
        let contents = env(&env_var).ok_or_else(|| StorageError::CredentialLoad {
            provider: storage.id.clone(),
            reference: storage.credential_ref.clone(),
            message: SanitizedMessage::new(format!("environment variable {env_var} is not set")),
        })?;

        GoogleDriveCredential::from_json(&storage.id, &storage.credential_ref, &contents)
    }

    fn env_var_for_ref(&self, provider: &str, reference: &str) -> StorageResult<String> {
        if let Some(name) = reference.strip_prefix("env:") {
            validate_env_var_name(provider, reference, name)?;
            return Ok(name.to_owned());
        }

        let mut name = String::with_capacity(self.env_prefix.len() + reference.len());
        name.push_str(&self.env_prefix);
        for byte in reference.bytes() {
            match byte {
                b'a'..=b'z' => name.push((byte as char).to_ascii_uppercase()),
                b'A'..=b'Z' | b'0'..=b'9' | b'_' => name.push(byte as char),
                b'-' => name.push('_'),
                _ => {
                    return Err(StorageError::CredentialLoad {
                        provider: provider.to_owned(),
                        reference: reference.to_owned(),
                        message: SanitizedMessage::new(
                            "bare credential references must contain only ASCII letters, digits, '_' or '-'",
                        ),
                    });
                }
            }
        }
        validate_env_var_name(provider, reference, &name)?;

        Ok(name)
    }
}

impl Default for GoogleDriveCredentialLoader {
    fn default() -> Self {
        Self::new()
    }
}

/// Server-owned Google Drive OAuth credential.
#[derive(Clone, Eq, PartialEq)]
pub struct GoogleDriveCredential {
    provider_id: String,
    credential_ref: String,
    client_id: String,
    client_secret: String,
    refresh_token: String,
    token_url: Url,
}

impl GoogleDriveCredential {
    /// Parses a flat JSON Google Drive OAuth credential.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the JSON is malformed, required fields are
    /// missing, or `token_uri` is not an absolute HTTPS URL. HTTP is accepted
    /// only for loopback hosts used by local tests and development tools.
    pub fn from_json(
        provider_id: impl Into<String>,
        credential_ref: impl Into<String>,
        contents: &str,
    ) -> StorageResult<Self> {
        let provider_id = provider_id.into();
        let credential_ref = credential_ref.into();
        let raw = serde_json::from_str::<RawGoogleDriveCredential>(contents).map_err(|source| {
            credential_load_error(
                &provider_id,
                &credential_ref,
                format!("credential JSON is invalid: {source}"),
            )
        })?;

        let token_uri = raw
            .token_uri
            .unwrap_or_else(|| GOOGLE_OAUTH_TOKEN_URL.to_owned());
        let token_url = validate_token_url(&provider_id, &credential_ref, &token_uri)?;

        Ok(Self {
            client_id: required_credential_field(
                &raw.client_id,
                "client_id",
                &provider_id,
                &credential_ref,
            )?,
            client_secret: required_credential_field(
                &raw.client_secret,
                "client_secret",
                &provider_id,
                &credential_ref,
            )?,
            refresh_token: required_credential_field(
                &raw.refresh_token,
                "refresh_token",
                &provider_id,
                &credential_ref,
            )?,
            provider_id,
            credential_ref,
            token_url,
        })
    }

    /// Returns the configured storage provider ID this credential belongs to.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.provider_id
    }

    /// Returns the non-secret credential reference from server configuration.
    #[must_use]
    pub fn credential_ref(&self) -> &str {
        &self.credential_ref
    }

    /// Returns the OAuth token endpoint used for refresh-token exchange.
    #[must_use]
    pub fn token_url(&self) -> &Url {
        &self.token_url
    }
}

impl fmt::Debug for GoogleDriveCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveCredential")
            .field("provider_id", &self.provider_id)
            .field("credential_ref", &self.credential_ref)
            .field("client_id", &"<redacted>")
            .field("client_secret", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("token_url", &self.token_url)
            .finish()
    }
}

/// Short-lived Google Drive OAuth access token.
#[derive(Clone, Eq, PartialEq)]
pub struct GoogleDriveAccessToken {
    access_token: String,
    token_type: String,
    expires_in_seconds: Option<u64>,
    scope: Vec<String>,
}

impl GoogleDriveAccessToken {
    /// Returns the raw bearer token secret for provider HTTP requests.
    ///
    /// Callers must not log this value or return it to Git LFS clients.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.access_token
    }

    /// Returns an HTTP `Authorization` header value for this bearer token.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the token cannot be represented as an HTTP
    /// header value.
    pub fn authorization_header_value(&self, provider: &str) -> StorageResult<HeaderValue> {
        HeaderValue::from_str(&format!("Bearer {}", self.access_token)).map_err(|_| {
            StorageError::Upstream {
                provider: provider.to_owned(),
                status: None,
                message: SanitizedMessage::new(
                    "Google OAuth access token could not be encoded as an HTTP header",
                ),
            }
        })
    }

    /// Returns the token lifetime reported by Google, when present.
    #[must_use]
    pub fn expires_in_seconds(&self) -> Option<u64> {
        self.expires_in_seconds
    }

    /// Returns the OAuth scopes reported by Google for this access token.
    #[must_use]
    pub fn scope(&self) -> &[String] {
        &self.scope
    }
}

impl fmt::Debug for GoogleDriveAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveAccessToken")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in_seconds", &self.expires_in_seconds)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Exchanges Google Drive refresh tokens for access tokens.
#[derive(Clone)]
pub struct GoogleDriveTokenRefresher {
    client: Client,
}

impl GoogleDriveTokenRefresher {
    /// Creates a token refresher using the default HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the default HTTP client cannot be built.
    pub fn new() -> StorageResult<Self> {
        Ok(Self {
            client: default_google_drive_http_client()?,
        })
    }

    /// Creates a token refresher with an explicit HTTP client.
    #[must_use]
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Exchanges a Google Drive refresh token for a short-lived access token.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] for transport failures, provider-denied
    /// credentials, retryable upstream failures, and malformed token responses.
    pub async fn refresh_access_token(
        &self,
        credential: &GoogleDriveCredential,
    ) -> StorageResult<GoogleDriveAccessToken> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", &credential.client_id)
            .append_pair("client_secret", &credential.client_secret)
            .append_pair("refresh_token", &credential.refresh_token)
            .append_pair("grant_type", "refresh_token")
            .finish();

        let response = self
            .client
            .post(credential.token_url.clone())
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(body)
            .send()
            .await
            .map_err(|source| refresh_transport_error(credential, source))?;
        let status = response.status();
        let response_body = read_google_response_body(response)
            .await
            .map_err(|source| refresh_transport_error(credential, source))?;

        if status.is_success() {
            parse_token_success(credential, status, &response_body)
        } else {
            Err(parse_token_error(credential, status, &response_body))
        }
    }
}

impl fmt::Debug for GoogleDriveTokenRefresher {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveTokenRefresher")
            .field("client", &"<redacted>")
            .finish()
    }
}

/// Metadata proving that a configured Google Drive root folder is usable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDriveRootFolder {
    /// Configured storage provider ID.
    pub provider_id: String,
    /// Google Drive folder ID from server configuration.
    pub id: String,
    /// Operator-facing Google Drive folder name.
    pub name: String,
    /// Whether the Drive API reports that this credential can create children.
    ///
    /// Successful validation guarantees this is `true`; a false or missing API
    /// value fails validation before returning this metadata.
    pub can_add_children: bool,
}

/// Validates that a configured Google Drive root folder is app-accessible.
///
/// The validator performs a non-mutating `files.get` probe. It confirms that
/// the configured ID resolves to a live folder and that the current credential
/// can add children under it. This is intentionally weaker than an upload
/// smoke test, but it is safe for startup and health checks.
#[derive(Clone)]
pub struct GoogleDriveRootValidator {
    client: Client,
    api_base_url: Url,
}

impl GoogleDriveRootValidator {
    /// Creates a validator using the default Google Drive HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the default HTTP client or API base URL
    /// cannot be initialized.
    pub fn new() -> StorageResult<Self> {
        Self::with_client_and_api_base_url(
            default_google_drive_root_validation_http_client()?,
            GOOGLE_DRIVE_API_BASE_URL,
        )
    }

    /// Creates a validator with an explicit HTTP client.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the default Drive API base URL is invalid.
    pub fn with_client(client: Client) -> StorageResult<Self> {
        Self::with_client_and_api_base_url(client, GOOGLE_DRIVE_API_BASE_URL)
    }

    /// Creates a validator with an explicit HTTP client and API base URL.
    ///
    /// This is primarily useful for tests that replace Google Drive with a
    /// local HTTP server.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if `api_base_url` is not an absolute HTTP(S)
    /// URL without credentials, query, or fragment components. HTTP is accepted
    /// only for loopback hosts used by local tests and development tools.
    pub fn with_client_and_api_base_url(
        client: Client,
        api_base_url: impl AsRef<str>,
    ) -> StorageResult<Self> {
        let api_base_url = validate_drive_api_base_url(api_base_url.as_ref())?;
        Ok(Self {
            client,
            api_base_url,
        })
    }

    /// Validates access to the configured Drive root folder.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the access token cannot authorize Drive
    /// requests, the folder cannot be found, the configured ID is not a live
    /// folder, or the credential cannot create child objects there.
    pub async fn validate_root_folder(
        &self,
        storage: &GoogleDriveStorageConfig,
        token: &GoogleDriveAccessToken,
    ) -> StorageResult<GoogleDriveRootFolder> {
        let response = self
            .client
            .get(drive_file_metadata_url(
                self.api_base_url.clone(),
                &storage.root_folder_id,
            )?)
            .header(ACCEPT, "application/json")
            .header(
                AUTHORIZATION,
                token.authorization_header_value(&storage.id)?,
            )
            .send()
            .await
            .map_err(|source| drive_transport_error(storage, token, source))?;
        let status = response.status();
        let response_body = read_google_response_body(response)
            .await
            .map_err(|source| drive_transport_error(storage, token, source))?;

        if !status.is_success() {
            return Err(parse_drive_root_error(
                storage,
                token,
                status,
                &response_body,
            ));
        }

        parse_drive_root_success(storage, token, status, &response_body)
    }
}

impl fmt::Debug for GoogleDriveRootValidator {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveRootValidator")
            .field("client", &"<redacted>")
            .field("api_base_url", &self.api_base_url)
            .finish()
    }
}

/// Deterministic Google Drive address metadata for one LFS object.
///
/// The display path is an inspection and cleanup convention under the
/// configured Drive root. Lookups still verify private Drive app properties so
/// the later SQLite metadata database can remain the ownership source of truth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDriveObjectKey {
    repo_namespace: String,
    object: LfsObject,
}

impl GoogleDriveObjectKey {
    /// Creates Drive object-addressing metadata for a repository-scoped object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the repository namespace is blank or
    /// contains control characters that cannot be rendered safely.
    pub fn new(repo_namespace: impl AsRef<str>, object: LfsObject) -> StorageResult<Self> {
        Ok(Self {
            repo_namespace: validate_repo_namespace(repo_namespace.as_ref())?,
            object,
        })
    }

    /// Returns the repository namespace associated with this object.
    #[must_use]
    pub fn repo_namespace(&self) -> &str {
        &self.repo_namespace
    }

    /// Returns the provider-independent object identity.
    #[must_use]
    pub fn object(&self) -> &LfsObject {
        &self.object
    }

    /// Returns the deterministic Drive file name for this LFS object.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!(
            "sha256-{}-{}.lfs",
            self.object.oid.as_hex(),
            self.object.size.bytes()
        )
    }

    /// Returns the human-readable object path below the configured Drive root.
    ///
    /// Google Drive addresses files by ID, not POSIX paths. This value is a
    /// deterministic convention for upload placement and operator inspection.
    #[must_use]
    pub fn display_path(&self) -> String {
        let oid = self.object.oid.as_hex();
        format!(
            "objects/{}/sha256/{}/{}/{}",
            percent_encode_drive_path_segment(&self.repo_namespace),
            &oid[..2],
            &oid[2..4],
            self.file_name()
        )
    }

    fn expected_app_properties(&self) -> GoogleDriveObjectProperties {
        GoogleDriveObjectProperties {
            repo_namespace: GoogleDriveRepositoryNamespaceProperty::new(&self.repo_namespace),
            oid: self.object.oid.as_hex().to_owned(),
            size: self.object.size.bytes().to_string(),
        }
    }
}

enum GoogleDriveRepositoryNamespaceProperty {
    Raw(String),
    Sha256(String),
}

impl GoogleDriveRepositoryNamespaceProperty {
    fn new(repo_namespace: &str) -> Self {
        // Preserve the original value for existing objects whenever Drive can
        // represent it. Oversized values need a tagged digest so a raw
        // namespace that resembles a digest cannot alias another repository.
        if GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len() + repo_namespace.len()
            <= MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
        {
            Self::Raw(repo_namespace.to_owned())
        } else {
            Self::Sha256(format!("{:x}", Sha256::digest(repo_namespace.as_bytes())))
        }
    }

    fn value(&self) -> &str {
        match self {
            Self::Raw(value) | Self::Sha256(value) => value,
        }
    }

    fn format(&self) -> Option<&'static str> {
        match self {
            Self::Raw(_) => None,
            Self::Sha256(_) => Some(GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT),
        }
    }
}

struct GoogleDriveObjectProperties {
    repo_namespace: GoogleDriveRepositoryNamespaceProperty,
    oid: String,
    size: String,
}

impl GoogleDriveObjectProperties {
    fn pairs(&self) -> Vec<(&'static str, &str)> {
        let mut pairs = vec![
            (
                GOOGLE_DRIVE_OBJECT_VERSION_PROPERTY,
                GOOGLE_DRIVE_OBJECT_VERSION,
            ),
            (
                GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY,
                self.repo_namespace.value(),
            ),
            (GOOGLE_DRIVE_OBJECT_OID_PROPERTY, &self.oid),
            (GOOGLE_DRIVE_OBJECT_SIZE_PROPERTY, &self.size),
        ];
        if let Some(format) = self.repo_namespace.format() {
            pairs.push((GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY, format));
        }
        pairs
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DriveUploadPhase {
    Initiate,
    Transfer,
}

/// A verified Google Drive object download exposed as an HTTP response body.
///
/// The response proxies Drive bytes directly while checking the requested LFS
/// object hash and size. It intentionally
/// does not expose Drive file IDs, URLs, or credentials to Git LFS clients. The
/// current scaffold uses Axum as its HTTP server boundary; this wrapper can move
/// behind a server crate boundary when the package is split.
pub struct GoogleDriveDownloadResponse {
    stored_object: StoredObject,
    response: AxumResponse,
}

impl GoogleDriveDownloadResponse {
    /// Returns the verified storage metadata for the downloaded object.
    #[must_use]
    pub fn stored_object(&self) -> &StoredObject {
        &self.stored_object
    }

    /// Consumes this download and returns the HTTP response to send downstream.
    #[must_use]
    pub fn into_response(self) -> AxumResponse {
        self.response
    }
}

impl fmt::Debug for GoogleDriveDownloadResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveDownloadResponse")
            .field("stored_object", &self.stored_object)
            .field("response", &"<streaming body>")
            .finish()
    }
}

/// Looks up repository-scoped LFS objects in Google Drive.
#[derive(Clone)]
pub struct GoogleDriveObjectStore {
    storage: GoogleDriveStorageConfig,
    repo_namespace: String,
    token: GoogleDriveAccessToken,
    metadata_client: Client,
    upload_client: Client,
    download_client: Client,
    api_base_url: Url,
    transfer_read_idle_timeout: Duration,
    upload_retry_initial_backoff: Duration,
}

impl GoogleDriveObjectStore {
    /// Creates an object store using the default Drive API client.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the HTTP client or repository namespace
    /// cannot be initialized.
    pub fn new(
        storage: GoogleDriveStorageConfig,
        repo_namespace: impl AsRef<str>,
        token: GoogleDriveAccessToken,
    ) -> StorageResult<Self> {
        Self::with_clients_and_api_base_url(
            storage,
            repo_namespace,
            token,
            default_google_drive_object_metadata_http_client()?,
            default_google_drive_object_upload_http_client()?,
            default_google_drive_object_download_http_client()?,
            GOOGLE_DRIVE_API_BASE_URL,
        )
    }

    /// Creates an object store with an explicit HTTP client and API base URL.
    ///
    /// This is primarily useful for tests that replace Google Drive with a
    /// local HTTP server.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the repository namespace or API base URL is
    /// not safe to use.
    pub fn with_client_and_api_base_url(
        storage: GoogleDriveStorageConfig,
        repo_namespace: impl AsRef<str>,
        token: GoogleDriveAccessToken,
        client: Client,
        api_base_url: impl AsRef<str>,
    ) -> StorageResult<Self> {
        Self::with_clients_and_api_base_url(
            storage,
            repo_namespace,
            token,
            client.clone(),
            client.clone(),
            client,
            api_base_url,
        )
    }

    fn with_clients_and_api_base_url(
        storage: GoogleDriveStorageConfig,
        repo_namespace: impl AsRef<str>,
        token: GoogleDriveAccessToken,
        metadata_client: Client,
        upload_client: Client,
        download_client: Client,
        api_base_url: impl AsRef<str>,
    ) -> StorageResult<Self> {
        Ok(Self {
            storage,
            repo_namespace: validate_repo_namespace(repo_namespace.as_ref())?,
            token,
            metadata_client,
            upload_client,
            download_client,
            api_base_url: validate_drive_api_base_url(api_base_url.as_ref())?,
            transfer_read_idle_timeout: GOOGLE_DRIVE_TRANSFER_READ_IDLE_TIMEOUT,
            upload_retry_initial_backoff: GOOGLE_DRIVE_RESUMABLE_UPLOAD_INITIAL_BACKOFF,
        })
    }

    /// Returns this store's configured storage-provider ID.
    #[must_use]
    pub fn provider_id(&self) -> &str {
        &self.storage.id
    }

    /// Creates a deterministic Drive object key for the configured repository.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the repository namespace cannot be rendered
    /// safely. This should only happen if the store was constructed with
    /// invalid state.
    pub fn object_key(&self, object: &LfsObject) -> StorageResult<GoogleDriveObjectKey> {
        GoogleDriveObjectKey::new(&self.repo_namespace, object.clone())
    }

    /// Checks whether the object exists under the configured Drive root.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] for backend authentication failures, retryable
    /// Drive failures or malformed Drive responses. Exact duplicate files are
    /// reconciled by selecting the lexicographically smallest Drive file ID.
    pub async fn object_exists(&self, object: &LfsObject) -> StorageResult<bool> {
        Ok(self.lookup_object(object).await?.is_some())
    }

    /// Returns verified backend metadata for an existing Drive object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] for backend authentication failures, retryable
    /// Drive failures or malformed Drive responses. Exact duplicate files are
    /// reconciled by selecting the lexicographically smallest Drive file ID.
    pub async fn lookup_object(&self, object: &LfsObject) -> StorageResult<Option<StoredObject>> {
        let key = self.object_key(object)?;
        let expected_properties = key.expected_app_properties();
        let mut stored_objects = Vec::new();
        let mut page_token = None;
        let mut seen_page_tokens = BTreeSet::new();

        loop {
            let response = self
                .metadata_client
                .get(drive_object_lookup_url(
                    self.api_base_url.clone(),
                    &self.storage.root_folder_id,
                    &key,
                    &expected_properties,
                    page_token.as_deref(),
                )?)
                .header(ACCEPT, "application/json")
                .header(
                    AUTHORIZATION,
                    self.token.authorization_header_value(&self.storage.id)?,
                )
                .header(ACCEPT_ENCODING, "identity")
                .send()
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            let status = response.status();
            let response_body = read_google_response_body(response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;

            if !status.is_success() {
                return Err(parse_drive_object_lookup_error(
                    &self.storage,
                    &self.token,
                    status,
                    &response_body,
                ));
            }

            let page = parse_drive_object_lookup_success(
                &self.storage,
                &key,
                &expected_properties,
                status,
                &response_body,
            )?;
            stored_objects.extend(page.stored_objects);

            let Some(next_page_token) = page.next_page_token else {
                break;
            };
            if !seen_page_tokens.insert(next_page_token.clone()) {
                return Err(StorageError::Retryable {
                    provider: self.storage.id.clone(),
                    message: "Google Drive object lookup repeated a page token".to_owned(),
                });
            }
            page_token = Some(next_page_token);
        }

        stored_objects.sort_unstable_by(|left, right| left.backend_id.cmp(&right.backend_id));
        Ok(stored_objects.into_iter().next())
    }

    /// Uploads a staged and locally verified object file through Drive resumable upload.
    ///
    /// The staged file is read before any Drive request so its SHA-256 and
    /// byte count can be checked against the LFS pointer metadata. Uploads use
    /// bounded 256 KiB-aligned chunks. Interrupted transfers query the existing
    /// session's committed offset and continue from Drive's authoritative
    /// `Range` response instead of creating a new backend file.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the staged file cannot be read, its bytes
    /// do not match the requested object identity, Drive cannot create a
    /// resumable session, or the upload completion response is malformed.
    pub async fn upload_object(
        &self,
        object: &LfsObject,
        source: impl AsRef<Path>,
    ) -> StorageResult<StoredObject> {
        let source = source.as_ref().to_path_buf();
        let verified_file =
            open_verified_staged_upload_file_on_blocking_thread(&self.storage, object, &source)
                .await?;

        let key = self.object_key(object)?;
        let expected_properties = key.expected_app_properties();
        let metadata = drive_upload_metadata(&self.storage.root_folder_id, &key);
        let initiate_response = self
            .upload_client
            .post(drive_resumable_upload_url(self.api_base_url.clone())?)
            .timeout(GOOGLE_DRIVE_OBJECT_METADATA_TIMEOUT)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, "application/json")
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header("X-Upload-Content-Type", GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header("X-Upload-Content-Length", object.size.bytes().to_string())
            .json(&metadata)
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let initiate_status = initiate_response.status();
        let session_url = if initiate_status.is_success() {
            let session_url = initiate_response
                .headers()
                .get(LOCATION)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.trim().is_empty())
                .ok_or_else(|| StorageError::Upstream {
                    provider: self.storage.id.clone(),
                    status: Some(initiate_status.as_u16()),
                    message: SanitizedMessage::new(
                        "Google Drive resumable upload response did not include Location",
                    ),
                })?;
            validate_drive_resumable_upload_session_url(
                &self.storage,
                &self.api_base_url,
                session_url,
            )?
        } else {
            let response_body = read_google_response_body(initiate_response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            return Err(parse_drive_upload_error(
                &self.storage,
                &self.token,
                object,
                DriveUploadPhase::Initiate,
                initiate_status,
                &response_body,
            ));
        };

        let mut file = tokio::fs::File::from_std(verified_file);
        let total_size = object.size.bytes();
        let mut committed_offset = 0_u64;
        let mut recovery_attempts = 0_u32;

        loop {
            let chunk = read_drive_upload_chunk(
                &self.storage,
                &source,
                &mut file,
                committed_offset,
                total_size,
            )
            .await?;
            let chunk_end = committed_offset
                .checked_add(chunk.len() as u64)
                .and_then(|end| end.checked_sub(1));
            let (upload_stream, upload_progress) = upload_chunk_progress_stream(chunk);
            let mut upload_request = self
                .upload_client
                .put(session_url.clone())
                .header(ACCEPT, "application/json")
                .header(
                    AUTHORIZATION,
                    self.token.authorization_header_value(&self.storage.id)?,
                )
                .header(CONTENT_TYPE, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
                .header(
                    CONTENT_LENGTH,
                    chunk_end
                        .map_or(0, |end| end - committed_offset + 1)
                        .to_string(),
                );
            if let Some(chunk_end) = chunk_end {
                upload_request = upload_request.header(
                    CONTENT_RANGE,
                    format!("bytes {committed_offset}-{chunk_end}/{total_size}"),
                );
            }
            let upload_result = match send_drive_upload_with_idle_timeout(
                &self.storage,
                &self.token,
                upload_request.body(ReqwestBody::wrap_stream(upload_stream)),
                upload_progress,
                self.transfer_read_idle_timeout,
            )
            .await
            {
                Ok(response) => {
                    parse_drive_resumable_upload_response(
                        self,
                        object,
                        &key,
                        &expected_properties,
                        response,
                        chunk_end.map_or(0, |end| end + 1),
                    )
                    .await
                }
                Err(error) => Err(error),
            };
            let upload_progress = match upload_result {
                Ok(progress) => progress,
                Err(error) if is_retryable_storage_error(&error) => {
                    recover_drive_resumable_upload(
                        self,
                        object,
                        &key,
                        &expected_properties,
                        &session_url,
                        &mut recovery_attempts,
                        error,
                    )
                    .await?
                }
                Err(error) => return Err(error),
            };

            match upload_progress {
                DriveResumableUploadProgress::Complete(stored_object) => {
                    return Ok(stored_object);
                }
                DriveResumableUploadProgress::Incomplete(next_offset) => {
                    if next_offset < committed_offset {
                        return Err(drive_resumable_upload_protocol_error(
                            &self.storage,
                            "Google Drive resumable upload moved its committed offset backwards",
                        ));
                    }
                    if next_offset > committed_offset {
                        committed_offset = next_offset;
                        recovery_attempts = 0;
                        continue;
                    }
                    if recovery_attempts >= GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS {
                        return Err(StorageError::Retryable {
                            provider: self.storage.id.clone(),
                            message: "Google Drive resumable upload made no committed progress"
                                .to_owned(),
                        });
                    }
                    sleep_drive_upload_backoff(self, recovery_attempts).await;
                    recovery_attempts += 1;
                }
            }
        }
    }

    /// Downloads a verified Drive object into a local destination path.
    ///
    /// This accepts only Drive files whose private object metadata and streamed
    /// bytes match the requested repository-scoped LFS object before publishing
    /// the bytes to the destination path.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the object is missing, Drive rejects the
    /// media request, the response omits or conflicts with the requested object
    /// size, streamed bytes fail integrity verification, or the destination
    /// path cannot be written.
    pub async fn download_object(
        &self,
        object: &LfsObject,
        destination: impl AsRef<Path>,
    ) -> StorageResult<StoredObject> {
        let destination = destination.as_ref();
        let (stored_object, verified_file) = self.download_object_to_verified_file(object).await?;
        persist_verified_drive_download_file(&self.storage, verified_file, destination).await?;

        Ok(stored_object)
    }

    /// Streams a verified Drive object as an HTTP response.
    ///
    /// This performs a metadata lookup first, so the Drive file ID is accepted
    /// only when private app properties and binary size match the requested
    /// repository-scoped LFS object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the object is missing, Drive rejects the
    /// media request, the response omits or conflicts with the requested object
    /// size, or the HTTP response cannot be built.
    pub async fn download_object_response(
        &self,
        object: &LfsObject,
    ) -> StorageResult<GoogleDriveDownloadResponse> {
        let stored_object =
            self.lookup_object(object)
                .await?
                .ok_or_else(|| StorageError::ObjectNotFound {
                    provider: self.storage.id.clone(),
                    oid: object.oid.as_hex().to_owned(),
                    size: object.size.bytes(),
                })?;
        let download_response = self
            .download_client
            .get(drive_media_download_url(
                self.api_base_url.clone(),
                &stored_object.backend_id,
            )?)
            .header(ACCEPT, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let status = download_response.status();
        if !status.is_success() {
            let response_body = read_google_response_body(download_response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            return Err(parse_drive_download_error(
                &self.storage,
                &self.token,
                object,
                status,
                &response_body,
            ));
        }
        let Some(actual_size) = download_response.content_length() else {
            return Err(StorageError::Upstream {
                provider: self.storage.id.clone(),
                status: None,
                message: SanitizedMessage::new(
                    "Google Drive download response omitted Content-Length",
                ),
            });
        };
        if actual_size != object.size.bytes() {
            return Err(StorageError::Upstream {
                provider: self.storage.id.clone(),
                status: None,
                message: SanitizedMessage::new(format!(
                    "Google Drive download response Content-Length {actual_size} did not match requested size {}",
                    object.size.bytes()
                )),
            });
        }

        let expected_oid = object.oid.as_hex().to_owned();
        let expected_size = object.size.bytes();
        let stream = futures_util::stream::try_unfold(
            (
                download_response.bytes_stream(),
                Sha256::new(),
                0_u64,
                false,
            ),
            move |(mut source, mut hasher, mut actual_size, finished)| {
                let expected_oid = expected_oid.clone();
                async move {
                    if finished {
                        return Ok(None);
                    }
                    match source.next().await {
                        Some(Ok(chunk)) => {
                            hasher.update(&chunk);
                            actual_size = actual_size.saturating_add(chunk.len() as u64);
                            if actual_size > expected_size {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "Google Drive download exceeded the requested object size",
                                ));
                            }
                            Ok(Some((chunk, (source, hasher, actual_size, false))))
                        }
                        Some(Err(_)) => {
                            Err(io::Error::other("Google Drive download stream failed"))
                        }
                        None => {
                            let actual_oid = format!("{:x}", hasher.finalize());
                            if actual_size != expected_size || actual_oid != expected_oid {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "Google Drive download failed LFS integrity verification",
                                ));
                            }
                            Ok(None)
                        }
                    }
                }
            },
        );
        let response_body = AxumBody::from_stream(stream);
        let response = AxumResponse::builder()
            .status(StatusCode::OK)
            .header(CONTENT_TYPE, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header(CONTENT_LENGTH, object.size.bytes().to_string())
            .body(response_body)
            .map_err(|source| StorageError::Upstream {
                provider: self.storage.id.clone(),
                status: None,
                message: SanitizedMessage::new(format!(
                    "Google Drive download response could not be built: {source}"
                )),
            })?;

        Ok(GoogleDriveDownloadResponse {
            stored_object,
            response,
        })
    }

    async fn download_object_to_verified_file(
        &self,
        object: &LfsObject,
    ) -> StorageResult<(StoredObject, File)> {
        let stored_object =
            self.lookup_object(object)
                .await?
                .ok_or_else(|| StorageError::ObjectNotFound {
                    provider: self.storage.id.clone(),
                    oid: object.oid.as_hex().to_owned(),
                    size: object.size.bytes(),
                })?;
        let download_response = self
            .download_client
            .get(drive_media_download_url(
                self.api_base_url.clone(),
                &stored_object.backend_id,
            )?)
            .header(ACCEPT, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
            .header(ACCEPT_ENCODING, "identity")
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let download_status = download_response.status();

        if !download_status.is_success() {
            let response_body = read_google_response_body(download_response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            return Err(parse_drive_download_error(
                &self.storage,
                &self.token,
                object,
                download_status,
                &response_body,
            ));
        }

        let Some(actual_size) = download_response.content_length() else {
            return Err(StorageError::Upstream {
                provider: self.storage.id.clone(),
                status: None,
                message: SanitizedMessage::new(
                    "Google Drive download response omitted Content-Length",
                ),
            });
        };
        if actual_size != object.size.bytes() {
            return Err(StorageError::Upstream {
                provider: self.storage.id.clone(),
                status: None,
                message: SanitizedMessage::new(format!(
                    "Google Drive download response Content-Length {actual_size} did not match requested size {}",
                    object.size.bytes()
                )),
            });
        }

        let verified_file = verify_drive_download_response_to_tempfile(
            &self.storage,
            &self.token,
            object,
            download_response,
        )
        .await?;

        Ok((stored_object, verified_file))
    }
}

impl StorageProvider for GoogleDriveObjectStore {
    fn provider_id(&self) -> &str {
        GoogleDriveObjectStore::provider_id(self)
    }

    fn object_exists<'a>(
        &'a self,
        object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<bool>> {
        Box::pin(async move { GoogleDriveObjectStore::object_exists(self, object).await })
    }

    fn upload_object<'a>(
        &'a self,
        object: &'a LfsObject,
        source: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        Box::pin(async move { GoogleDriveObjectStore::upload_object(self, object, source).await })
    }

    fn download_object<'a>(
        &'a self,
        object: &'a LfsObject,
        destination: &'a Path,
    ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
        // Delegate through the inherent method so the trait adapter keeps the
        // Drive-specific verification and atomic publication behavior in one path.
        Box::pin(
            async move { GoogleDriveObjectStore::download_object(self, object, destination).await },
        )
    }

    fn delete_or_mark_object<'a>(
        &'a self,
        _object: &'a LfsObject,
    ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
        Box::pin(async {
            Ok(StorageDeleteOutcome::Retained {
                reason: "Google Drive object deletion is not implemented".to_owned(),
            })
        })
    }
}

#[derive(Debug)]
enum DriveResumableUploadProgress {
    Complete(StoredObject),
    Incomplete(u64),
}

async fn read_drive_upload_chunk(
    storage: &GoogleDriveStorageConfig,
    source: &Path,
    file: &mut tokio::fs::File,
    offset: u64,
    total_size: u64,
) -> StorageResult<Vec<u8>> {
    file.seek(SeekFrom::Start(offset))
        .await
        .map_err(|error| staged_file_read_error(storage, source, error))?;
    let chunk_len = (total_size - offset).min(GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE as u64);
    let mut chunk = vec![0_u8; chunk_len as usize];
    file.read_exact(&mut chunk)
        .await
        .map_err(|error| staged_file_read_error(storage, source, error))?;
    Ok(chunk)
}

fn upload_chunk_progress_stream(
    chunk: Vec<u8>,
) -> (
    impl futures_util::Stream<Item = Result<Bytes, io::Error>> + Send + 'static,
    watch::Receiver<()>,
) {
    let (progress_sender, progress_receiver) = watch::channel(());
    let stream = futures_util::stream::once(async move {
        progress_sender.send_modify(|()| {});
        Ok(Bytes::from(chunk))
    });

    (stream, progress_receiver)
}

async fn parse_drive_resumable_upload_response(
    store: &GoogleDriveObjectStore,
    object: &LfsObject,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    response: reqwest::Response,
    maximum_committed_offset: u64,
) -> StorageResult<DriveResumableUploadProgress> {
    let status = response.status();
    if status.as_u16() == 308 {
        let committed_offset = parse_drive_resumable_upload_offset(
            &store.storage,
            response.headers(),
            object.size.bytes(),
            maximum_committed_offset,
        )?;
        return Ok(DriveResumableUploadProgress::Incomplete(committed_offset));
    }

    let body = read_drive_response_body_with_idle_timeout(
        &store.storage,
        &store.token,
        response,
        store.transfer_read_idle_timeout,
    )
    .await?;
    if matches!(status, StatusCode::OK | StatusCode::CREATED) {
        return parse_drive_upload_success(&store.storage, key, expected_properties, status, &body)
            .map(DriveResumableUploadProgress::Complete);
    }

    Err(parse_drive_upload_error(
        &store.storage,
        &store.token,
        object,
        DriveUploadPhase::Transfer,
        status,
        &body,
    ))
}

fn parse_drive_resumable_upload_offset(
    storage: &GoogleDriveStorageConfig,
    headers: &HeaderMap,
    total_size: u64,
    maximum_committed_offset: u64,
) -> StorageResult<u64> {
    let Some(range) = headers.get(RANGE) else {
        return Ok(0);
    };
    let range = range.to_str().map_err(|_| {
        drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned a non-text Range header",
        )
    })?;
    let Some(last_byte) = range.trim().strip_prefix("bytes=0-") else {
        return Err(drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an invalid Range header",
        ));
    };
    let last_byte = last_byte.parse::<u64>().map_err(|_| {
        drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an invalid Range header",
        )
    })?;
    let committed_offset = last_byte.checked_add(1).ok_or_else(|| {
        drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an overflowing Range header",
        )
    })?;
    if committed_offset > maximum_committed_offset
        || committed_offset >= total_size
        || total_size == 0
    {
        return Err(drive_resumable_upload_protocol_error(
            storage,
            "Google Drive resumable upload returned an impossible Range header",
        ));
    }
    Ok(committed_offset)
}

async fn recover_drive_resumable_upload(
    store: &GoogleDriveObjectStore,
    object: &LfsObject,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    session_url: &Url,
    recovery_attempts: &mut u32,
    mut last_error: StorageError,
) -> StorageResult<DriveResumableUploadProgress> {
    let total_size = object.size.bytes();
    loop {
        if *recovery_attempts >= GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS {
            return Err(last_error);
        }
        sleep_drive_upload_backoff(store, *recovery_attempts).await;
        *recovery_attempts += 1;

        let (progress_sender, progress_receiver) = watch::channel(());
        drop(progress_sender);
        let probe_request = store
            .upload_client
            .put(session_url.clone())
            .header(ACCEPT, "application/json")
            .header(
                AUTHORIZATION,
                store.token.authorization_header_value(&store.storage.id)?,
            )
            .header(CONTENT_LENGTH, "0")
            .header(CONTENT_RANGE, format!("bytes */{total_size}"));
        let probe_result = match send_drive_upload_with_idle_timeout(
            &store.storage,
            &store.token,
            probe_request,
            progress_receiver,
            store.transfer_read_idle_timeout,
        )
        .await
        {
            Ok(response) => {
                parse_drive_resumable_upload_response(
                    store,
                    object,
                    key,
                    expected_properties,
                    response,
                    total_size,
                )
                .await
            }
            Err(error) => Err(error),
        };
        match probe_result {
            Ok(progress) => return Ok(progress),
            Err(error) if is_retryable_storage_error(&error) => last_error = error,
            Err(error) => return Err(error),
        }
    }
}

async fn sleep_drive_upload_backoff(store: &GoogleDriveObjectStore, attempt: u32) {
    let multiplier = 1_u32 << attempt.min(8);
    tokio::time::sleep(
        store
            .upload_retry_initial_backoff
            .saturating_mul(multiplier),
    )
    .await;
}

fn is_retryable_storage_error(error: &StorageError) -> bool {
    matches!(error, StorageError::Retryable { .. })
}

fn drive_resumable_upload_protocol_error(
    storage: &GoogleDriveStorageConfig,
    message: &'static str,
) -> StorageError {
    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(308),
        message: SanitizedMessage::new(message),
    }
}

async fn send_drive_upload_with_idle_timeout(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    request: reqwest::RequestBuilder,
    mut progress: watch::Receiver<()>,
    idle_timeout: Duration,
) -> StorageResult<reqwest::Response> {
    let mut request = Box::pin(request.send());
    let idle = tokio::time::sleep(idle_timeout);
    tokio::pin!(idle);
    let mut body_is_streaming = true;

    loop {
        tokio::select! {
            response = &mut request => {
                return response.map_err(|source| drive_transport_error(storage, token, source));
            }
            progress_result = progress.changed(), if body_is_streaming => {
                if progress_result.is_err() {
                    body_is_streaming = false;
                }
                idle.as_mut().reset(tokio::time::Instant::now() + idle_timeout);
            }
            () = &mut idle => {
                return Err(StorageError::Retryable {
                    provider: storage.id.clone(),
                    message: "Google Drive upload made no progress before the idle timeout"
                        .to_owned(),
                });
            }
        }
    }
}

async fn read_drive_response_body_with_idle_timeout(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    mut response: reqwest::Response,
    idle_timeout: Duration,
) -> StorageResult<String> {
    let mut body = Vec::new();
    while body.len() < MAX_GOOGLE_ERROR_BODY_LEN {
        let chunk = tokio::time::timeout(idle_timeout, response.chunk())
            .await
            .map_err(|_| StorageError::Retryable {
                provider: storage.id.clone(),
                message: "Google Drive upload response stalled before completion".to_owned(),
            })?
            .map_err(|source| drive_transport_error(storage, token, source))?;
        let Some(chunk) = chunk else {
            break;
        };
        let remaining = MAX_GOOGLE_ERROR_BODY_LEN - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

async fn verify_drive_download_response_to_tempfile(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    object: &LfsObject,
    download_response: reqwest::Response,
) -> StorageResult<File> {
    let provider = storage.id.clone();
    let temp_file = tempfile::tempfile().map_err(|source| StorageError::Retryable {
        provider: provider.clone(),
        message: format!("Drive download staging file could not be created: {source}"),
    })?;
    let mut temp_file = tokio::fs::File::from_std(temp_file);
    let mut stream = download_response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|source| drive_transport_error(storage, token, source))?;
        hasher.update(&chunk);
        actual_size += chunk.len() as u64;
        temp_file
            .write_all(&chunk)
            .await
            .map_err(|source| drive_download_staging_file_error(storage, source))?;
    }

    let actual_oid = format!("{:x}", hasher.finalize());
    if actual_oid != object.oid.as_hex() || actual_size != object.size.bytes() {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: object.oid.as_hex().to_owned(),
            expected_size: object.size.bytes(),
            actual_oid,
            actual_size,
        });
    }

    temp_file
        .flush()
        .await
        .map_err(|source| drive_download_staging_file_error(storage, source))?;
    temp_file
        .seek(SeekFrom::Start(0))
        .await
        .map_err(|source| drive_download_staging_file_error(storage, source))?;

    Ok(temp_file.into_std().await)
}

async fn persist_verified_drive_download_file(
    storage: &GoogleDriveStorageConfig,
    mut source: File,
    destination: &Path,
) -> StorageResult<()> {
    let storage = storage.clone();
    let provider = storage.id.clone();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || {
        source.seek(SeekFrom::Start(0)).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        let destination_parent = destination
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(destination_parent).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        let mut destination_file = tempfile::Builder::new()
            .prefix(".lfs-cloud-download-")
            .tempfile_in(destination_parent)
            .map_err(|error| {
                drive_download_destination_file_error(&storage, &destination, error)
            })?;
        io::copy(&mut source, destination_file.as_file_mut()).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        destination_file.as_file_mut().sync_all().map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error)
        })?;
        destination_file.persist(&destination).map_err(|error| {
            drive_download_destination_file_error(&storage, &destination, error.error)
        })?;
        Ok(())
    })
    .await
    .map_err(|error| StorageError::Retryable {
        provider,
        message: format!("Drive download destination write task failed: {error}"),
    })?
}

fn drive_download_staging_file_error(
    storage: &GoogleDriveStorageConfig,
    source: std::io::Error,
) -> StorageError {
    StorageError::Retryable {
        provider: storage.id.clone(),
        message: format!("Drive download staging file could not be written: {source}"),
    }
}

fn drive_download_destination_file_error(
    storage: &GoogleDriveStorageConfig,
    path: &Path,
    source: std::io::Error,
) -> StorageError {
    StorageError::Retryable {
        provider: storage.id.clone(),
        message: format!(
            "Drive download destination file {} could not be written: {source}",
            path.display()
        ),
    }
}

impl fmt::Debug for GoogleDriveObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveObjectStore")
            .field("storage", &self.storage)
            .field("repo_namespace", &self.repo_namespace)
            .field("token", &"<redacted>")
            .field("metadata_client", &"<redacted>")
            .field("upload_client", &"<redacted>")
            .field("download_client", &"<redacted>")
            .field("api_base_url", &self.api_base_url)
            .finish()
    }
}

#[derive(Deserialize)]
struct RawGoogleDriveCredential {
    #[serde(default)]
    client_id: Option<String>,
    #[serde(default)]
    client_secret: Option<String>,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    token_uri: Option<String>,
}

#[derive(Deserialize)]
struct GoogleDriveTokenSuccess {
    access_token: Option<String>,
    #[serde(default)]
    token_type: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
    #[serde(default)]
    scope: Option<String>,
}

#[derive(Deserialize)]
struct GoogleDriveTokenError {
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    error_description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveFileMetadata {
    id: Option<String>,
    name: Option<String>,
    mime_type: Option<String>,
    #[serde(default)]
    trashed: bool,
    #[serde(default)]
    capabilities: GoogleDriveFileCapabilities,
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveFileCapabilities {
    can_add_children: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveFileList {
    #[serde(default)]
    files: Vec<GoogleDriveObjectFile>,
    #[serde(default)]
    next_page_token: Option<String>,
    #[serde(default)]
    incomplete_search: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoogleDriveObjectFile {
    id: Option<String>,
    name: Option<String>,
    size: Option<String>,
    #[serde(default)]
    app_properties: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct GoogleDriveErrorResponse {
    #[serde(default)]
    error: Option<GoogleDriveErrorBody>,
}

#[derive(Deserialize)]
struct GoogleDriveErrorBody {
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    errors: Vec<GoogleDriveErrorDetail>,
}

#[derive(Deserialize)]
struct GoogleDriveErrorDetail {
    #[serde(default)]
    reason: Option<String>,
}

fn required_credential_field(
    value: &Option<String>,
    field: &str,
    provider: &str,
    reference: &str,
) -> StorageResult<String> {
    value
        .as_ref()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .ok_or_else(|| credential_load_error(provider, reference, format!("{field} is required")))
}

fn credential_load_error(
    provider: &str,
    reference: &str,
    message: impl Into<String>,
) -> StorageError {
    StorageError::CredentialLoad {
        provider: provider.to_owned(),
        reference: reference.to_owned(),
        message: SanitizedMessage::new(message),
    }
}

fn validate_token_url(provider: &str, reference: &str, value: &str) -> StorageResult<Url> {
    let url = Url::parse(value)
        .map_err(|_| credential_load_error(provider, reference, "token_uri must be a valid URL"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(credential_load_error(
            provider,
            reference,
            "token_uri must be an absolute http or https URL",
        ));
    }
    if url.scheme() == "http" && !is_loopback_http_url(&url) {
        return Err(credential_load_error(
            provider,
            reference,
            "token_uri must use https unless it targets a loopback host",
        ));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(credential_load_error(
            provider,
            reference,
            "token_uri must not include credentials",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(credential_load_error(
            provider,
            reference,
            "token_uri must not include query strings or fragments",
        ));
    }

    Ok(url)
}

fn is_loopback_http_url(url: &Url) -> bool {
    if url.scheme() != "http" {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };

    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

fn validate_env_var_name(provider: &str, reference: &str, name: &str) -> StorageResult<()> {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return Err(credential_load_error(
            provider,
            reference,
            "credential environment variable name must not be empty",
        ));
    };
    if !(first.is_ascii_alphabetic() || first == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(credential_load_error(
            provider,
            reference,
            "credential environment variable name must contain only ASCII letters, digits, and '_' and must not start with a digit",
        ));
    }

    Ok(())
}

fn parse_token_success(
    credential: &GoogleDriveCredential,
    status: StatusCode,
    body: &str,
) -> StorageResult<GoogleDriveAccessToken> {
    let response = serde_json::from_str::<GoogleDriveTokenSuccess>(body).map_err(|_| {
        StorageError::Upstream {
            provider: credential.provider_id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google OAuth token response was invalid JSON"),
        }
    })?;
    let access_token = response
        .access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| StorageError::Upstream {
            provider: credential.provider_id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google OAuth token response did not include access_token",
            ),
        })?;
    let token_type = response
        .token_type
        .as_deref()
        .map(str::trim)
        .filter(|token_type| !token_type.is_empty())
        .unwrap_or("Bearer")
        .to_owned();
    if !token_type.eq_ignore_ascii_case("Bearer") {
        let mut message = sanitize_google_diagnostic(
            credential,
            &format!("Google OAuth token response used unsupported token_type {token_type:?}"),
        );
        if !access_token.is_empty() {
            message = message.replace(&access_token, "[redacted]");
        }
        return Err(StorageError::Upstream {
            provider: credential.provider_id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(message),
        });
    }

    Ok(GoogleDriveAccessToken {
        access_token,
        token_type,
        expires_in_seconds: response.expires_in,
        scope: response
            .scope
            .unwrap_or_default()
            .split_ascii_whitespace()
            .map(ToOwned::to_owned)
            .collect(),
    })
}

fn default_google_drive_http_client_from(
    client_slot: &'static OnceLock<Client>,
    timeout: Duration,
) -> StorageResult<Client> {
    if let Some(client) = client_slot.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .timeout(timeout)
        .connect_timeout(GOOGLE_DRIVE_CONNECT_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive HTTP client: {source}"),
        })?;

    match client_slot.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(client_slot.get().cloned().unwrap_or(client)),
    }
}

fn default_google_drive_http_client() -> StorageResult<Client> {
    default_google_drive_http_client_from(
        &DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT,
        GOOGLE_DRIVE_TOKEN_REFRESH_TIMEOUT,
    )
}

fn default_google_drive_root_validation_http_client() -> StorageResult<Client> {
    default_google_drive_http_client_from(
        &DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT,
        GOOGLE_DRIVE_ROOT_VALIDATION_TIMEOUT,
    )
}

fn default_google_drive_object_metadata_http_client() -> StorageResult<Client> {
    default_google_drive_http_client_from(
        &DEFAULT_GOOGLE_DRIVE_OBJECT_METADATA_HTTP_CLIENT,
        GOOGLE_DRIVE_OBJECT_METADATA_TIMEOUT,
    )
}

fn default_google_drive_object_upload_http_client() -> StorageResult<Client> {
    if let Some(client) = DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .connect_timeout(GOOGLE_DRIVE_CONNECT_TIMEOUT)
        .redirect(Policy::none())
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive upload HTTP client: {source}"),
        })?;

    match DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT
            .get()
            .cloned()
            .unwrap_or(client)),
    }
}

fn default_google_drive_object_download_http_client() -> StorageResult<Client> {
    if let Some(client) = DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .connect_timeout(GOOGLE_DRIVE_CONNECT_TIMEOUT)
        .read_timeout(GOOGLE_DRIVE_TRANSFER_READ_IDLE_TIMEOUT)
        .no_gzip()
        .no_brotli()
        .no_zstd()
        .no_deflate()
        .redirect(Policy::none())
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive download HTTP client: {source}"),
        })?;

    match DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT
            .get()
            .cloned()
            .unwrap_or(client)),
    }
}

fn validate_drive_api_base_url(value: &str) -> StorageResult<Url> {
    let url = Url::parse(value).map_err(|_| StorageError::Upstream {
        provider: "google_drive".to_owned(),
        status: None,
        message: SanitizedMessage::new("Google Drive API base URL must be valid"),
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive API base URL must be an absolute http or https URL",
            ),
        });
    }
    if url.scheme() == "http" && !is_loopback_http_url(&url) {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive API base URL must use https unless it targets a loopback host",
            ),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive API base URL must not include credentials",
            ),
        });
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive API base URL must not include query strings or fragments",
            ),
        });
    }

    Ok(url)
}

fn drive_api_base_path_already_targets_drive_api(api_base_url: &Url) -> bool {
    api_base_url
        .path_segments()
        .map(|segments| {
            let segments = segments
                .filter(|segment| !segment.is_empty())
                .collect::<Vec<_>>();
            segments.ends_with(&["drive", "v3"])
        })
        .unwrap_or(false)
}

fn drive_file_metadata_url(mut api_base_url: Url, root_folder_id: &str) -> StorageResult<Url> {
    if root_folder_id.trim().is_empty() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new("Google Drive root_folder_id must not be blank"),
        });
    }

    let base_path_already_targets_drive_api =
        drive_api_base_path_already_targets_drive_api(&api_base_url);

    {
        let mut segments =
            api_base_url
                .path_segments_mut()
                .map_err(|_| StorageError::Upstream {
                    provider: "google_drive".to_owned(),
                    status: None,
                    message: SanitizedMessage::new(
                        "Google Drive API base URL cannot be used for path construction",
                    ),
                })?;
        segments.pop_if_empty();
        if base_path_already_targets_drive_api {
            segments.extend(["files", root_folder_id]);
        } else {
            segments.extend(["drive", "v3", "files", root_folder_id]);
        }
    }
    api_base_url
        .query_pairs_mut()
        .append_pair(
            "fields",
            "id,name,mimeType,trashed,capabilities(canAddChildren)",
        )
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn drive_object_lookup_url(
    mut api_base_url: Url,
    root_folder_id: &str,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    page_token: Option<&str>,
) -> StorageResult<Url> {
    if root_folder_id.trim().is_empty() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new("Google Drive root_folder_id must not be blank"),
        });
    }

    let base_path_already_targets_drive_api =
        drive_api_base_path_already_targets_drive_api(&api_base_url);
    {
        let mut segments =
            api_base_url
                .path_segments_mut()
                .map_err(|_| StorageError::Upstream {
                    provider: "google_drive".to_owned(),
                    status: None,
                    message: SanitizedMessage::new(
                        "Google Drive API base URL cannot be used for path construction",
                    ),
                })?;
        segments.pop_if_empty();
        if base_path_already_targets_drive_api {
            segments.push("files");
        } else {
            segments.extend(["drive", "v3", "files"]);
        }
    }

    api_base_url
        .query_pairs_mut()
        .append_pair(
            "fields",
            "files(id,name,size,appProperties),nextPageToken,incompleteSearch",
        )
        .append_pair("pageSize", "2")
        .append_pair("spaces", "drive")
        .append_pair("corpora", "user")
        .append_pair("includeItemsFromAllDrives", "true")
        .append_pair("supportsAllDrives", "true")
        .append_pair(
            "q",
            &drive_object_lookup_query(root_folder_id, key, expected_properties),
        );
    if let Some(page_token) = page_token {
        api_base_url
            .query_pairs_mut()
            .append_pair("pageToken", page_token);
    }

    Ok(api_base_url)
}

fn drive_resumable_upload_url(mut api_base_url: Url) -> StorageResult<Url> {
    let base_path_already_targets_drive_api =
        drive_api_base_path_already_targets_drive_api(&api_base_url);

    {
        let mut segments =
            api_base_url
                .path_segments_mut()
                .map_err(|_| StorageError::Upstream {
                    provider: "google_drive".to_owned(),
                    status: None,
                    message: SanitizedMessage::new(
                        "Google Drive API base URL cannot be used for path construction",
                    ),
                })?;
        segments.pop_if_empty();
        if base_path_already_targets_drive_api {
            segments.pop();
            segments.pop();
        }
        segments.extend(["upload", "drive", "v3", "files"]);
    }

    api_base_url
        .query_pairs_mut()
        .append_pair("uploadType", "resumable")
        .append_pair("fields", "id,name,size,appProperties")
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn drive_media_download_url(mut api_base_url: Url, file_id: &str) -> StorageResult<Url> {
    if file_id.trim().is_empty() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new("Google Drive file ID must not be blank"),
        });
    }
    let base_path_already_targets_drive_api =
        drive_api_base_path_already_targets_drive_api(&api_base_url);

    {
        let mut segments =
            api_base_url
                .path_segments_mut()
                .map_err(|_| StorageError::Upstream {
                    provider: "google_drive".to_owned(),
                    status: None,
                    message: SanitizedMessage::new(
                        "Google Drive API base URL cannot be used for path construction",
                    ),
                })?;
        segments.pop_if_empty();
        if base_path_already_targets_drive_api {
            segments.extend(["files", file_id]);
        } else {
            segments.extend(["drive", "v3", "files", file_id]);
        }
    }
    api_base_url
        .query_pairs_mut()
        .append_pair("alt", "media")
        .append_pair("supportsAllDrives", "true");

    Ok(api_base_url)
}

fn validate_drive_resumable_upload_session_url(
    storage: &GoogleDriveStorageConfig,
    api_base_url: &Url,
    value: &str,
) -> StorageResult<Url> {
    let url = Url::parse(value).map_err(|_| StorageError::Upstream {
        provider: storage.id.clone(),
        status: None,
        message: SanitizedMessage::new("Google Drive resumable upload session URL must be valid"),
    })?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive resumable upload session URL must be an absolute http or https URL",
            ),
        });
    }
    if url.scheme() == "http" && !is_loopback_http_url(&url) {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive resumable upload session URL must use https unless it targets a loopback host",
            ),
        });
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive resumable upload session URL must not include credentials",
            ),
        });
    }
    if url.fragment().is_some() {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive resumable upload session URL must not include fragments",
            ),
        });
    }
    if !url_origins_match(&url, api_base_url) {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: None,
            message: SanitizedMessage::new(
                "Google Drive resumable upload session URL must match the configured Drive API origin",
            ),
        });
    }

    Ok(url)
}

fn url_origins_match(left: &Url, right: &Url) -> bool {
    left.scheme() == right.scheme()
        && left.host() == right.host()
        && left.port_or_known_default() == right.port_or_known_default()
}

fn drive_upload_metadata(root_folder_id: &str, key: &GoogleDriveObjectKey) -> serde_json::Value {
    let app_properties = key
        .expected_app_properties()
        .pairs()
        .into_iter()
        .map(|(property, value)| (property, value.to_owned()))
        .collect::<BTreeMap<_, _>>();

    serde_json::json!({
        "name": key.file_name(),
        "parents": [root_folder_id],
        "appProperties": app_properties,
    })
}

fn drive_object_lookup_query(
    root_folder_id: &str,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
) -> String {
    let mut query = format!(
        "'{}' in parents and trashed = false and name = '{}'",
        escape_drive_query_string(root_folder_id),
        escape_drive_query_string(&key.file_name())
    );

    for (property, value) in expected_properties.pairs() {
        query.push_str(&format!(
            " and appProperties has {{ key='{}' and value='{}' }}",
            escape_drive_query_string(property),
            escape_drive_query_string(value)
        ));
    }

    query
}

fn validate_repo_namespace(value: &str) -> StorageResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new("repository namespace must not be blank"),
        });
    }
    if trimmed.chars().any(char::is_control) {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new(
                "repository namespace must not contain control characters",
            ),
        });
    }

    Ok(trimmed.to_owned())
}

fn percent_encode_drive_path_segment(value: &str) -> String {
    let mut encoded = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                encoded.push(byte as char);
            }
            _ => {
                encoded.push('%');
                encoded.push(hex_digit(byte >> 4));
                encoded.push(hex_digit(byte & 0x0f));
            }
        }
    }
    encoded
}

fn hex_digit(value: u8) -> char {
    match value {
        0..=9 => (b'0' + value) as char,
        10..=15 => (b'A' + value - 10) as char,
        _ => unreachable!("hex digit nibble should be in range"),
    }
}

fn escape_drive_query_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\'' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

fn parse_drive_root_success(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    status: StatusCode,
    body: &str,
) -> StorageResult<GoogleDriveRootFolder> {
    let metadata = serde_json::from_str::<GoogleDriveFileMetadata>(body).map_err(|_| {
        StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive root folder response was invalid JSON"),
        }
    })?;
    let id = metadata
        .id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive root folder response did not include id"),
        })?;
    let name = metadata
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| storage.root_folder_id.clone());
    if id != storage.root_folder_id {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive returned folder id {id:?} for configured root_folder_id {:?}",
                storage.root_folder_id
            )),
        });
    }
    if metadata.mime_type.as_deref() != Some(GOOGLE_DRIVE_FOLDER_MIME_TYPE) {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive root_folder_id {:?} is not a folder",
                storage.root_folder_id
            )),
        });
    }
    if metadata.trashed {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive root_folder_id {:?} is trashed",
                storage.root_folder_id
            )),
        });
    }
    let can_add_children =
        metadata
            .capabilities
            .can_add_children
            .ok_or_else(|| StorageError::Upstream {
                provider: storage.id.clone(),
                status: Some(status.as_u16()),
                message: SanitizedMessage::new(
                    "Google Drive root folder response did not include capabilities.canAddChildren",
                ),
            })?;
    if !can_add_children {
        return Err(StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(sanitize_drive_diagnostic(
                token,
                &format!(
                    "Google Drive root folder {:?} is visible but cannot accept child objects",
                    storage.root_folder_id
                ),
            )),
        });
    }

    Ok(GoogleDriveRootFolder {
        provider_id: storage.id.clone(),
        id,
        name,
        can_add_children,
    })
}

fn parse_drive_root_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }
    if status == StatusCode::NOT_FOUND {
        return StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(format!(
                "Google Drive root_folder_id {:?} was not found or is not accessible",
                storage.root_folder_id
            )),
        };
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}

fn parse_drive_object_lookup_success(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    status: StatusCode,
    body: &str,
) -> StorageResult<VerifiedGoogleDriveObjectPage> {
    let response =
        serde_json::from_str::<GoogleDriveFileList>(body).map_err(|_| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive object lookup response was invalid JSON"),
        })?;
    if response.incomplete_search {
        return Err(StorageError::Retryable {
            provider: storage.id.clone(),
            message: "Google Drive object lookup returned incomplete search results".to_owned(),
        });
    }
    let next_page_token = response
        .next_page_token
        .map(|token| {
            if token.trim().is_empty() {
                return Err(StorageError::Upstream {
                    provider: storage.id.clone(),
                    status: Some(status.as_u16()),
                    message: SanitizedMessage::new(
                        "Google Drive object lookup returned a blank page token",
                    ),
                });
            }
            Ok(token)
        })
        .transpose()?;

    let stored_objects = response
        .files
        .into_iter()
        .map(|file| verify_drive_object_file(storage, key, expected_properties, status, file))
        .collect::<StorageResult<Vec<_>>>()?;
    Ok(VerifiedGoogleDriveObjectPage {
        stored_objects,
        next_page_token,
    })
}

struct VerifiedGoogleDriveObjectPage {
    stored_objects: Vec<StoredObject>,
    next_page_token: Option<String>,
}

fn verify_drive_object_file(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    status: StatusCode,
    file: GoogleDriveObjectFile,
) -> StorageResult<StoredObject> {
    let id = file
        .id
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive object lookup response did not include id",
            ),
        })?;
    if file.name.as_deref() != Some(&key.file_name()) {
        return Err(StorageError::Conflict {
            provider: storage.id.clone(),
            oid: key.object.oid.as_hex().to_owned(),
        });
    }
    for (property, expected) in expected_properties.pairs() {
        if file.app_properties.get(property).map(String::as_str) != Some(expected) {
            return Err(StorageError::Conflict {
                provider: storage.id.clone(),
                oid: key.object.oid.as_hex().to_owned(),
            });
        }
    }
    let actual_size = file
        .size
        .as_deref()
        .ok_or_else(|| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive object lookup response did not include size",
            ),
        })?
        .parse::<u64>()
        .map_err(|_| StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new("Google Drive object lookup response size was invalid"),
        })?;
    if actual_size != key.object.size.bytes() {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: key.object.oid.as_hex().to_owned(),
            expected_size: key.object.size.bytes(),
            actual_oid: key.object.oid.as_hex().to_owned(),
            actual_size,
        });
    }

    Ok(StoredObject::new(
        storage.id.clone(),
        key.object.clone(),
        id,
    ))
}

async fn open_verified_staged_upload_file_on_blocking_thread(
    storage: &GoogleDriveStorageConfig,
    object: &LfsObject,
    source: &Path,
) -> StorageResult<File> {
    let storage = storage.clone();
    let object = object.clone();
    let source = source.to_path_buf();
    let provider = storage.id.clone();

    tokio::task::spawn_blocking(move || {
        open_verified_staged_upload_file(&storage, &object, &source)
    })
    .await
    .map_err(|error| StorageError::Retryable {
        provider,
        message: format!("staged upload file verification task failed: {error}"),
    })?
}

fn open_verified_staged_upload_file(
    storage: &GoogleDriveStorageConfig,
    object: &LfsObject,
    source: &Path,
) -> StorageResult<File> {
    let file =
        File::open(source).map_err(|error| staged_file_read_error(storage, source, error))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];

    loop {
        let bytes_read = reader
            .read(&mut buffer)
            .map_err(|error| staged_file_read_error(storage, source, error))?;
        if bytes_read == 0 {
            break;
        }
        hasher.update(&buffer[..bytes_read]);
        actual_size += bytes_read as u64;
    }

    let actual_oid = format!("{:x}", hasher.finalize());
    if actual_oid != object.oid.as_hex() || actual_size != object.size.bytes() {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: object.oid.as_hex().to_owned(),
            expected_size: object.size.bytes(),
            actual_oid,
            actual_size,
        });
    }

    let mut file = reader.into_inner();
    file.seek(SeekFrom::Start(0))
        .map_err(|error| staged_file_read_error(storage, source, error))?;

    Ok(file)
}

fn staged_file_read_error(
    storage: &GoogleDriveStorageConfig,
    path: &Path,
    source: std::io::Error,
) -> StorageError {
    StorageError::StagedFileRead {
        provider: storage.id.clone(),
        path: path.to_path_buf(),
        source,
    }
}

fn parse_drive_upload_success(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    status: StatusCode,
    body: &str,
) -> StorageResult<StoredObject> {
    let file = serde_json::from_str::<GoogleDriveObjectFile>(body).map_err(|_| {
        StorageError::Upstream {
            provider: storage.id.clone(),
            status: Some(status.as_u16()),
            message: SanitizedMessage::new(
                "Google Drive upload completion response was invalid JSON",
            ),
        }
    })?;

    verify_drive_object_file(storage, key, expected_properties, status, file)
}

fn parse_drive_object_lookup_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}

fn parse_drive_upload_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    object: &LfsObject,
    phase: DriveUploadPhase,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }
    if status == StatusCode::CONFLICT {
        return StorageError::Conflict {
            provider: storage.id.clone(),
            oid: object.oid.as_hex().to_owned(),
        };
    }
    if status.as_u16() == 308
        || (phase == DriveUploadPhase::Transfer && status == StatusCode::NOT_FOUND)
    {
        // Retrying the full upload operation starts a fresh resumable session.
        return StorageError::Retryable {
            provider: storage.id.clone(),
            message: diagnostic.message,
        };
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}

fn parse_drive_download_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    object: &LfsObject,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if let Some(error) = classify_common_drive_error(storage, status, &diagnostic) {
        return error;
    }
    if status == StatusCode::NOT_FOUND
        || diagnostic.reasons.iter().any(|reason| reason == "notFound")
    {
        return StorageError::ObjectNotFound {
            provider: storage.id.clone(),
            oid: object.oid.as_hex().to_owned(),
            size: object.size.bytes(),
        };
    }
    if status == StatusCode::CONFLICT {
        return StorageError::Conflict {
            provider: storage.id.clone(),
            oid: object.oid.as_hex().to_owned(),
        };
    }

    StorageError::Upstream {
        provider: storage.id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}

fn classify_common_drive_error(
    storage: &GoogleDriveStorageConfig,
    status: StatusCode,
    diagnostic: &DriveDiagnostic,
) -> Option<StorageError> {
    if status == StatusCode::UNAUTHORIZED
        || diagnostic
            .reasons
            .iter()
            .any(|reason| matches!(reason.as_str(), "authError" | "insufficientPermissions"))
    {
        return Some(StorageError::AuthenticationRequired {
            provider: storage.id.clone(),
        });
    }
    if diagnostic
        .reasons
        .iter()
        .any(|reason| matches!(reason.as_str(), "quotaExceeded" | "storageQuotaExceeded"))
    {
        return Some(StorageError::QuotaExceeded {
            provider: storage.id.clone(),
            message: diagnostic.message.clone(),
        });
    }
    if status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || diagnostic.reasons.iter().any(|reason| {
            matches!(
                reason.as_str(),
                "rateLimitExceeded" | "userRateLimitExceeded"
            )
        })
    {
        return Some(StorageError::Retryable {
            provider: storage.id.clone(),
            message: diagnostic.message.clone(),
        });
    }

    None
}

fn drive_error_message(token: &GoogleDriveAccessToken, body: &str) -> DriveDiagnostic {
    if let Ok(GoogleDriveErrorResponse { error: Some(error) }) =
        serde_json::from_str::<GoogleDriveErrorResponse>(body)
    {
        let message = error
            .message
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Google Drive request failed".to_owned());
        let reasons = error
            .errors
            .into_iter()
            .filter_map(|detail| detail.reason)
            .filter(|reason| !reason.trim().is_empty())
            .collect();
        return DriveDiagnostic {
            message: sanitize_drive_diagnostic(token, &cap_google_diagnostic(&message)),
            reasons,
        };
    }

    DriveDiagnostic {
        message: sanitize_drive_diagnostic(token, &cap_google_diagnostic(body)),
        reasons: Vec::new(),
    }
}

struct DriveDiagnostic {
    message: String,
    reasons: Vec<String>,
}

fn sanitize_drive_diagnostic(token: &GoogleDriveAccessToken, message: &str) -> String {
    let sanitized = redact_secret_from_message(message, token.as_str());
    if sanitized.trim().is_empty() {
        "Google Drive request failed".to_owned()
    } else {
        sanitized
    }
}

fn drive_transport_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    source: reqwest::Error,
) -> StorageError {
    StorageError::Retryable {
        provider: storage.id.clone(),
        message: sanitize_drive_diagnostic(
            token,
            &format!("Google Drive request failed: {source}"),
        ),
    }
}

fn parse_token_error(
    credential: &GoogleDriveCredential,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = google_token_error_message(credential, body);
    if matches!(
        diagnostic.code.as_deref(),
        Some("invalid_grant" | "invalid_client" | "unauthorized_client")
    ) || matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
    {
        return StorageError::AuthenticationRequired {
            provider: credential.provider_id.clone(),
        };
    }
    if status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return StorageError::Retryable {
            provider: credential.provider_id.clone(),
            message: diagnostic.message,
        };
    }

    StorageError::Upstream {
        provider: credential.provider_id.clone(),
        status: Some(status.as_u16()),
        message: SanitizedMessage::new(diagnostic.message),
    }
}

fn google_token_error_message(
    credential: &GoogleDriveCredential,
    body: &str,
) -> GoogleTokenDiagnostic {
    if let Ok(error) = serde_json::from_str::<GoogleDriveTokenError>(body) {
        let code = error.error.filter(|value| !value.trim().is_empty());
        let message = error
            .error_description
            .or_else(|| code.clone())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Google OAuth token refresh failed".to_owned());
        return GoogleTokenDiagnostic {
            code,
            message: sanitize_google_diagnostic(credential, &cap_google_diagnostic(&message)),
        };
    }

    GoogleTokenDiagnostic {
        code: None,
        message: sanitize_google_diagnostic(credential, &cap_google_diagnostic(body)),
    }
}

struct GoogleTokenDiagnostic {
    code: Option<String>,
    message: String,
}

fn sanitize_google_diagnostic(credential: &GoogleDriveCredential, message: &str) -> String {
    let mut sanitized = message.to_owned();
    for secret in [
        &credential.client_id,
        &credential.client_secret,
        &credential.refresh_token,
    ] {
        sanitized = redact_secret_from_message(&sanitized, secret);
    }
    if sanitized.trim().is_empty() {
        "Google OAuth token refresh failed".to_owned()
    } else {
        sanitized
    }
}

fn refresh_transport_error(
    credential: &GoogleDriveCredential,
    source: reqwest::Error,
) -> StorageError {
    let message = sanitize_google_diagnostic(
        credential,
        &format!("Google OAuth token refresh request failed: {source}"),
    );

    StorageError::Retryable {
        provider: credential.provider_id.clone(),
        message,
    }
}

fn cap_google_diagnostic(message: &str) -> String {
    message.chars().take(MAX_GOOGLE_ERROR_BODY_LEN).collect()
}

fn redact_secret_from_message(message: &str, secret: &str) -> String {
    if secret.is_empty() {
        return message.to_owned();
    }

    let mut sanitized = message.replace(secret, "[redacted]");
    if secret.len() < MIN_REDACTED_SECRET_FRAGMENT_LEN {
        return sanitized;
    }

    for prefix_length in (MIN_REDACTED_SECRET_FRAGMENT_LEN..secret.len()).rev() {
        let Some(prefix) = secret.get(..prefix_length) else {
            continue;
        };
        if sanitized.ends_with(prefix) {
            let suffix_start = sanitized.len() - prefix.len();
            sanitized.replace_range(suffix_start.., "[redacted]");
            break;
        }
    }
    sanitized
}

async fn read_google_response_body(
    mut response: reqwest::Response,
) -> Result<String, reqwest::Error> {
    let mut body = Vec::new();
    while body.len() < MAX_GOOGLE_ERROR_BODY_LEN {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let remaining = MAX_GOOGLE_ERROR_BODY_LEN - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        io::Cursor,
        str::FromStr,
        sync::{Arc, Mutex},
        time::Duration,
    };

    use super::{
        GOOGLE_OAUTH_TOKEN_URL, GoogleDriveCredential, GoogleDriveCredentialLoader,
        GoogleDriveObjectKey, GoogleDriveObjectStore, GoogleDriveRootValidator,
        GoogleDriveTokenRefresher,
    };
    use crate::{
        GoogleDriveStorageConfig, LfsObject, LfsObjectSize, LfsOid, StorageDeleteOutcome,
        StorageError, StorageProvider,
    };
    use axum::{
        Router,
        body::{Body, Bytes, to_bytes},
        extract::{Path, State},
        http::{
            HeaderMap, HeaderValue, Uri,
            header::{ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
        },
        response::{IntoResponse, Response},
        routing::{get, post, put},
    };
    use reqwest::StatusCode;
    use sha2::{Digest, Sha256};
    use tokio_util::io::ReaderStream;

    const OBJECT_OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn loader_resolves_bare_credential_refs_to_server_side_env_secret_json() {
        let storage = storage_config("google-drive-user-a");
        let loader = GoogleDriveCredentialLoader::new();

        let credential = loader
            .load_from_env_with(&storage, |name| {
                assert_eq!(
                    name,
                    "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_GOOGLE_DRIVE_USER_A"
                );
                Some(credential_json())
            })
            .expect("credential should load");

        assert_eq!(credential.provider_id(), "drive-user-a");
        assert_eq!(credential.credential_ref(), "google-drive-user-a");
        assert_eq!(credential.token_url().as_str(), GOOGLE_OAUTH_TOKEN_URL);
        assert_eq!(credential.client_id, "client-id");
        assert_eq!(credential.client_secret, "client-secret");
        assert_eq!(credential.refresh_token, "refresh-token");
        assert!(!format!("{credential:?}").contains("refresh-token"));
        assert!(!format!("{credential:?}").contains("client-secret"));
    }

    #[test]
    fn default_drive_transfer_clients_bound_idle_reads_without_total_deadlines() {
        let upload_client = super::default_google_drive_object_upload_http_client()
            .expect("default Drive upload client should build");
        let download_client = super::default_google_drive_object_download_http_client()
            .expect("default Drive download client should build");

        let upload_debug = format!("{upload_client:?}");
        assert!(
            !upload_debug.contains("read_timeout"),
            "upload progress needs a body-aware watchdog, not a time-to-response limit: {upload_debug}"
        );
        assert!(
            !upload_debug.contains("total_timeout"),
            "large uploads must not impose a total request deadline: {upload_debug}"
        );

        let download_debug = format!("{download_client:?}");
        assert!(
            download_debug.contains("read_timeout: 30s"),
            "downloads should reset a 30-second idle watchdog after each read: {download_debug}"
        );
        assert!(
            !download_debug.contains("total_timeout"),
            "large downloads must not impose a total request deadline: {download_debug}"
        );
    }

    #[test]
    fn loader_supports_explicit_env_var_refs() {
        let storage = storage_config("env:DRIVE_USER_A_JSON");
        let loader = GoogleDriveCredentialLoader::new();

        let credential = loader
            .load_from_env_with(&storage, |name| {
                assert_eq!(name, "DRIVE_USER_A_JSON");
                Some(credential_json())
            })
            .expect("credential should load");

        assert_eq!(credential.credential_ref(), "env:DRIVE_USER_A_JSON");
    }

    #[test]
    fn loader_reports_missing_env_without_secret_material() {
        let storage = storage_config("google-drive-user-a");
        let loader = GoogleDriveCredentialLoader::new();

        let error = loader
            .load_from_env_with(&storage, |_| None)
            .expect_err("missing credential env should fail");

        assert!(matches!(
            error,
            StorageError::CredentialLoad {
                ref provider,
                ref reference,
                ..
            } if provider == "drive-user-a" && reference == "google-drive-user-a"
        ));
        assert!(!error.to_string().contains("client-secret"));
        assert!(
            error
                .to_string()
                .contains("LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_GOOGLE_DRIVE_USER_A")
        );
    }

    #[test]
    fn credential_json_requires_refresh_token() {
        let error = GoogleDriveCredential::from_json(
            "drive-user-a",
            "google-drive-user-a",
            r#"{"client_id":"client-id","client_secret":"client-secret"}"#,
        )
        .expect_err("missing refresh token should fail");

        assert!(matches!(
            error,
            StorageError::CredentialLoad {
                ref provider,
                ref reference,
                ..
            } if provider == "drive-user-a" && reference == "google-drive-user-a"
        ));
        assert!(error.to_string().contains("refresh_token is required"));
        assert!(!error.to_string().contains("client-secret"));
    }

    #[test]
    fn credential_json_reports_parse_details_without_secret_material() {
        let error = GoogleDriveCredential::from_json(
            "drive-user-a",
            "google-drive-user-a",
            r#"{"client_id":"client-id","client_secret":"client-secret","refresh_token":"refresh-token""#,
        )
        .expect_err("malformed JSON should fail");
        let display = error.to_string();

        assert!(display.contains("credential JSON is invalid"));
        assert!(display.contains("line"));
        assert!(!display.contains("client-secret"));
        assert!(!display.contains("refresh-token"));
    }

    #[test]
    fn credential_json_requires_https_token_uri_except_loopback() {
        let error = GoogleDriveCredential::from_json(
            "drive-user-a",
            "google-drive-user-a",
            r#"{"client_id":"client-id","client_secret":"client-secret","refresh_token":"refresh-token","token_uri":"http://tokens.example.com/token"}"#,
        )
        .expect_err("non-loopback HTTP token URI should fail");

        assert!(
            error
                .to_string()
                .contains("token_uri must use https unless it targets a loopback host")
        );

        let credential = GoogleDriveCredential::from_json(
            "drive-user-a",
            "google-drive-user-a",
            r#"{"client_id":"client-id","client_secret":"client-secret","refresh_token":"refresh-token","token_uri":"http://localhost/token"}"#,
        )
        .expect("loopback HTTP token URI should be accepted for local testing");

        assert_eq!(credential.token_url().as_str(), "http://localhost/token");
    }

    #[test]
    fn root_validator_requires_https_api_base_except_loopback() {
        let error = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            "http://drive.example.com/drive/v3",
        )
        .expect_err("non-loopback HTTP API base should fail");

        assert!(error.to_string().contains(
            "Google Drive API base URL must use https unless it targets a loopback host"
        ));

        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            "http://localhost/drive/v3",
        )
        .expect("loopback HTTP API base should be accepted for local testing");

        assert_eq!(validator.api_base_url.as_str(), "http://localhost/drive/v3");
    }

    #[test]
    fn drive_file_metadata_url_does_not_duplicate_existing_drive_api_path() {
        let url = super::drive_file_metadata_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-root",
        )
        .expect("metadata URL should build");

        assert_eq!(url.path(), "/proxy/drive/v3/files/drive-root");
    }

    #[test]
    fn drive_object_key_defines_stable_display_path_and_file_name() {
        let key = GoogleDriveObjectKey::new("github.com/Owner Repo/repo.git", lfs_object())
            .expect("object key should build");

        assert_eq!(key.repo_namespace(), "github.com/Owner Repo/repo.git");
        assert_eq!(key.object(), &lfs_object());
        assert_eq!(key.file_name(), format!("sha256-{OBJECT_OID}-42.lfs"));
        assert_eq!(
            key.display_path(),
            format!(
                "objects/github.com%2FOwner%20Repo%2Frepo.git/sha256/aa/aa/sha256-{OBJECT_OID}-42.lfs"
            )
        );
    }

    #[test]
    fn drive_object_properties_preserve_namespace_at_property_byte_limit() {
        let namespace_len = super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
            - super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len();
        let namespace = "r".repeat(namespace_len);
        let key = GoogleDriveObjectKey::new(&namespace, lfs_object())
            .expect("maximum raw namespace should build");
        let properties = key.expected_app_properties();
        let pairs = properties.pairs();

        assert!(pairs.contains(&(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY,
            namespace.as_str()
        )));
        assert!(
            !pairs
                .iter()
                .any(|(key, _)| *key == super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY)
        );
        assert!(pairs.iter().all(|(key, value)| {
            key.len() + value.len() <= super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
        }));
    }

    #[test]
    fn drive_object_properties_digest_oversized_namespace() {
        let namespace_byte_limit = super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
            - super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len();
        let namespace = format!("{}é", "r".repeat(namespace_byte_limit - 1));
        assert_eq!(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len() + namespace.len(),
            super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES + 1
        );
        let expected_digest = format!("{:x}", Sha256::digest(namespace.as_bytes()));
        let key = GoogleDriveObjectKey::new(&namespace, lfs_object())
            .expect("oversized raw namespace should build with digest metadata");
        let properties = key.expected_app_properties();
        let pairs = properties.pairs();

        assert!(pairs.contains(&(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY,
            expected_digest.as_str()
        )));
        assert!(pairs.contains(&(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY,
            super::GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT
        )));
        assert!(pairs.iter().all(|(key, value)| {
            key.len() + value.len() <= super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
        }));

        let metadata = super::drive_upload_metadata("drive-root", &key);
        assert_eq!(
            metadata["appProperties"][super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY],
            expected_digest
        );
        assert_eq!(
            metadata["appProperties"][super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY],
            super::GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT
        );

        let query = super::drive_object_lookup_query("drive-root", &key, &properties);
        assert!(query.contains(&format!(
            "appProperties has {{ key='{}' and value='{expected_digest}' }}",
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY
        )));
        assert!(query.contains(&format!(
            "appProperties has {{ key='{}' and value='{}' }}",
            super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY,
            super::GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT
        )));
    }

    #[test]
    fn drive_object_lookup_url_searches_with_private_app_properties() {
        let key = GoogleDriveObjectKey::new("github.com/owner/repo", lfs_object())
            .expect("key should build");
        let url = super::drive_object_lookup_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-root",
            &key,
            &key.expected_app_properties(),
            None,
        )
        .expect("lookup URL should build");

        assert_eq!(url.path(), "/proxy/drive/v3/files");
        let query = form_pairs(url.query().expect("lookup URL should include query"));
        assert_eq!(
            query["fields"],
            "files(id,name,size,appProperties),nextPageToken,incompleteSearch"
        );
        assert_eq!(query["pageSize"], "2");
        assert_eq!(query["spaces"], "drive");
        assert_eq!(query["corpora"], "user");
        assert_eq!(query["includeItemsFromAllDrives"], "true");
        assert_eq!(query["supportsAllDrives"], "true");
        assert!(query["q"].contains("'drive-root' in parents"));
        assert!(query["q"].contains("trashed = false"));
        assert!(query["q"].contains(&format!("name = 'sha256-{OBJECT_OID}-42.lfs'")));
        assert!(query["q"].contains(
            "appProperties has { key='lfsCloudRepoNamespace' and value='github.com/owner/repo' }"
        ));
        assert!(query["q"].contains(&format!(
            "appProperties has {{ key='lfsCloudOid' and value='{OBJECT_OID}' }}"
        )));
        assert!(query["q"].contains("appProperties has { key='lfsCloudSize' and value='42' }"));
    }

    #[test]
    fn drive_resumable_upload_url_does_not_duplicate_existing_drive_api_path() {
        let url = super::drive_resumable_upload_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
        )
        .expect("upload URL should build");

        assert_eq!(url.path(), "/proxy/upload/drive/v3/files");
        let query = form_pairs(url.query().expect("upload URL should include query"));
        assert_eq!(query["uploadType"], "resumable");
        assert_eq!(query["fields"], "id,name,size,appProperties");
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[test]
    fn drive_media_download_url_does_not_duplicate_existing_drive_api_path() {
        let url = super::drive_media_download_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-file-123",
        )
        .expect("download URL should build");

        assert_eq!(url.path(), "/proxy/drive/v3/files/drive-file-123");
        let query = form_pairs(url.query().expect("download URL should include query"));
        assert_eq!(query["alt"], "media");
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[test]
    fn drive_media_download_url_encodes_opaque_file_ids_as_one_segment() {
        let url = super::drive_media_download_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-file-123/../../other",
        )
        .expect("opaque file IDs should be path-encoded");

        assert_eq!(
            url.path(),
            "/proxy/drive/v3/files/drive-file-123%2F..%2F..%2Fother"
        );
    }

    #[test]
    fn drive_media_download_url_rejects_blank_file_ids() {
        let error = super::drive_media_download_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            " \t",
        )
        .expect_err("blank file IDs should fail before URL construction");

        assert!(matches!(
            error,
            StorageError::Upstream { ref message, .. }
                if message.as_str().contains("Google Drive file ID must not be blank")
        ));
    }

    #[test]
    fn drive_resumable_upload_session_url_requires_https_except_loopback() {
        let storage = storage_config("google-drive-user-a");
        let error = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("https://www.googleapis.com").expect("API base should parse"),
            "http://drive.example.com/upload/session-1?upload_id=123",
        )
        .expect_err("non-loopback HTTP session URL should fail");

        assert!(error.to_string().contains(
            "Google Drive resumable upload session URL must use https unless it targets a loopback host"
        ));

        let url = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("http://localhost").expect("API base should parse"),
            "http://localhost/upload/session-1?upload_id=123",
        )
        .expect("loopback HTTP session URL should be accepted for local testing");

        assert_eq!(
            url.as_str(),
            "http://localhost/upload/session-1?upload_id=123"
        );
    }

    #[test]
    fn drive_resumable_upload_session_url_must_match_api_origin() {
        let storage = storage_config("google-drive-user-a");
        let error = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("https://www.googleapis.com").expect("API base should parse"),
            "https://attacker.example/upload/session-1?upload_id=123",
        )
        .expect_err("cross-origin session URL should fail before forwarding auth");

        assert!(error.to_string().contains(
            "Google Drive resumable upload session URL must match the configured Drive API origin"
        ));

        let url = super::validate_drive_resumable_upload_session_url(
            &storage,
            &url::Url::parse("https://www.googleapis.com/drive/v3").expect("API base should parse"),
            "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&upload_id=123",
        )
        .expect("same-origin session URL should be accepted");

        assert_eq!(
            url.as_str(),
            "https://www.googleapis.com/upload/drive/v3/files?uploadType=resumable&upload_id=123"
        );
    }

    #[test]
    fn drive_object_key_rejects_blank_namespace() {
        let error = GoogleDriveObjectKey::new(" \t\n", lfs_object())
            .expect_err("blank namespace should fail");

        assert!(
            error
                .to_string()
                .contains("repository namespace must not be blank")
        );
    }

    #[test]
    fn drive_object_key_rejects_control_characters_in_namespace() {
        let error = GoogleDriveObjectKey::new("github.com/owner\nrepo", lfs_object())
            .expect_err("control character should fail");

        assert!(
            error
                .to_string()
                .contains("repository namespace must not contain control characters")
        );
    }

    #[test]
    fn credential_json_rejects_token_uri_query_and_fragment() {
        for token_uri in [
            "https://oauth2.googleapis.com/token?client_secret=client-secret",
            "https://oauth2.googleapis.com/token#client-secret",
        ] {
            let error = GoogleDriveCredential::from_json(
                "drive-user-a",
                "google-drive-user-a",
                &format!(
                    r#"{{
                        "client_id":"client-id",
                        "client_secret":"client-secret",
                        "refresh_token":"refresh-token",
                        "token_uri":"{token_uri}"
                    }}"#
                ),
            )
            .expect_err("token_uri query strings and fragments should fail");

            assert!(
                error
                    .to_string()
                    .contains("token_uri must not include query strings or fragments")
            );
        }
    }

    #[tokio::test]
    async fn refresher_exchanges_refresh_token_for_bearer_access_token() {
        let server = TokenServer::start(
            StatusCode::OK,
            r#"{"access_token":"access-token","token_type":"Bearer","expires_in":3599,"scope":"https://www.googleapis.com/auth/drive.file email"}"#,
        )
        .await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let token = refresher
            .refresh_access_token(&credential)
            .await
            .expect("refresh should succeed");

        assert_eq!(token.as_str(), "access-token");
        assert_eq!(token.expires_in_seconds(), Some(3599));
        assert_eq!(
            token.scope(),
            &[
                "https://www.googleapis.com/auth/drive.file".to_owned(),
                "email".to_owned()
            ]
        );
        assert_eq!(
            token
                .authorization_header_value("drive-user-a")
                .expect("header should encode"),
            "Bearer access-token"
        );
        assert!(!format!("{token:?}").contains("access-token"));

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0].headers.get(CONTENT_TYPE).unwrap(),
            "application/x-www-form-urlencoded"
        );
        let form = form_pairs(&requests[0].body);
        assert_eq!(form["grant_type"], "refresh_token");
        assert_eq!(form["client_id"], "client-id");
        assert_eq!(form["client_secret"], "client-secret");
        assert_eq!(form["refresh_token"], "refresh-token");
    }

    #[tokio::test]
    async fn refresher_accepts_bearer_token_type_with_surrounding_whitespace() {
        let server = TokenServer::start(
            StatusCode::OK,
            r#"{"access_token":"access-token","token_type":" Bearer \t"}"#,
        )
        .await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let token = refresher
            .refresh_access_token(&credential)
            .await
            .expect("whitespace-padded bearer token type should refresh");

        assert_eq!(token.as_str(), "access-token");
    }

    #[tokio::test]
    async fn refresher_redacts_credentials_from_unsupported_token_type() {
        let server = TokenServer::start(
            StatusCode::OK,
            r#"{"access_token":"access-token","token_type":"client-secret refresh-token"}"#,
        )
        .await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let error = refresher
            .refresh_access_token(&credential)
            .await
            .expect_err("unsupported token type should fail");
        let display = error.to_string();

        assert!(display.contains("unsupported token_type"));
        assert!(!display.contains("client-secret"));
        assert!(!display.contains("refresh-token"));
        assert!(display.contains("[redacted] [redacted]"));
    }

    #[tokio::test]
    async fn refresher_reports_status_for_malformed_success_response() {
        let server = TokenServer::start(StatusCode::OK, r#"{"token_type":"Bearer"}"#).await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let error = refresher
            .refresh_access_token(&credential)
            .await
            .expect_err("missing access token should fail");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(200),
                ..
            } if provider == "drive-user-a"
        ));
    }

    #[tokio::test]
    async fn refresher_maps_invalid_grant_to_authentication_required() {
        let server = TokenServer::start(
            StatusCode::BAD_REQUEST,
            r#"{"error":"invalid_grant","error_description":"refresh token expired"}"#,
        )
        .await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let error = refresher
            .refresh_access_token(&credential)
            .await
            .expect_err("invalid grant should require new credentials");

        assert!(matches!(
            error,
            StorageError::AuthenticationRequired { provider } if provider == "drive-user-a"
        ));
    }

    #[tokio::test]
    async fn refresher_redacts_credentials_from_upstream_diagnostics() {
        let server = TokenServer::start(
            StatusCode::BAD_REQUEST,
            r#"{"error":"bad_request","error_description":"client-secret refresh-token rejected"}"#,
        )
        .await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let error = refresher
            .refresh_access_token(&credential)
            .await
            .expect_err("bad request should fail");
        let display = error.to_string();

        assert!(!display.contains("client-secret"));
        assert!(!display.contains("refresh-token"));
        assert!(display.contains("[redacted] [redacted] rejected"));
    }

    #[tokio::test]
    async fn refresher_caps_large_upstream_error_body_before_diagnostics() {
        let large_body = format!(
            "{}after-limit client-secret refresh-token",
            "x".repeat(super::MAX_GOOGLE_ERROR_BODY_LEN)
        );
        let server = TokenServer::start(StatusCode::BAD_GATEWAY, large_body).await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let error = refresher
            .refresh_access_token(&credential)
            .await
            .expect_err("large upstream error should fail");

        assert!(matches!(
            error,
            StorageError::Retryable {
                provider,
                message,
            } if provider == "drive-user-a"
                && message.len() == super::MAX_GOOGLE_ERROR_BODY_LEN
                && !message.contains("after-limit")
                && !message.contains("client-secret")
                && !message.contains("refresh-token")
        ));
    }

    #[test]
    fn drive_diagnostics_redact_token_fragments_at_truncation_boundary() {
        let body = format!("{}access", "x".repeat(super::MAX_GOOGLE_ERROR_BODY_LEN - 6));
        let diagnostic = super::drive_error_message(&access_token(), &body);

        assert!(!diagnostic.message.contains("access"));
        assert!(diagnostic.message.ends_with("[redacted]"));
    }

    #[tokio::test]
    async fn refresher_maps_rate_limits_to_retryable_errors() {
        let server = TokenServer::start(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":"rate_limit_exceeded","error_description":"try later"}"#,
        )
        .await;
        let credential = credential_with_token_url(&server.url);
        let refresher = GoogleDriveTokenRefresher::with_client(reqwest::Client::new());

        let error = refresher
            .refresh_access_token(&credential)
            .await
            .expect_err("rate limit should be retryable");

        assert!(matches!(
            error,
            StorageError::Retryable {
                provider,
                message,
            } if provider == "drive-user-a" && message.contains("try later")
        ));
    }

    #[tokio::test]
    async fn object_store_finds_verified_object_by_repo_oid_and_size() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            drive_object_list_json("drive-file-123", OBJECT_OID, 42),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let found = store
            .lookup_object(&lfs_object())
            .await
            .expect("object lookup should succeed")
            .expect("object should exist");

        assert_eq!(found.provider_id, "drive-user-a");
        assert_eq!(found.object, lfs_object());
        assert_eq!(found.backend_id, "drive-file-123");
        assert!(
            store
                .object_exists(&lfs_object())
                .await
                .expect("exists should succeed")
        );

        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        let query = form_pairs(&requests[0].query);
        assert_eq!(query["corpora"], "user");
        assert_eq!(query["includeItemsFromAllDrives"], "true");
        assert_eq!(query["supportsAllDrives"], "true");
        assert!(query["q"].contains("'drive-root' in parents"));
        assert!(query["q"].contains(
            "appProperties has { key='lfsCloudRepoNamespace' and value='github.com/owner/repo' }"
        ));
    }

    #[tokio::test]
    async fn object_store_reports_missing_object_as_false() {
        let server = DriveFilesListServer::start(StatusCode::OK, r#"{"files":[]}"#).await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        assert!(
            !store
                .object_exists(&lfs_object())
                .await
                .expect("missing object lookup should succeed")
        );
    }

    #[tokio::test]
    async fn object_store_selects_duplicate_drive_matches_deterministically() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            format!(
                r#"{{
                    "files":[
                        {},
                        {}
                    ]
                }}"#,
                drive_object_json("drive-file-b", OBJECT_OID, 42),
                drive_object_json("drive-file-a", OBJECT_OID, 42)
            ),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let stored_object = store
            .lookup_object(&lfs_object())
            .await
            .expect("duplicate Drive matches should reconcile")
            .expect("an exact Drive match should be returned");

        assert_eq!(stored_object.backend_id, "drive-file-a");
    }

    #[tokio::test]
    async fn object_store_reconciles_drive_matches_across_all_pages() {
        let server = DriveFilesListServer::start_paginated(
            format!(
                r#"{{
                    "files":[{}],
                    "nextPageToken":"page-2"
                }}"#,
                drive_object_json("drive-file-b", OBJECT_OID, 42)
            ),
            [(
                "page-2",
                drive_object_list_json("drive-file-a", OBJECT_OID, 42),
            )],
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let stored_object = store
            .lookup_object(&lfs_object())
            .await
            .expect("paginated Drive matches should reconcile")
            .expect("an exact Drive match should be returned");

        assert_eq!(stored_object.backend_id, "drive-file-a");
        let requests = server.requests();
        assert_eq!(requests.len(), 2);
        let first_query = form_pairs(&requests[0].query);
        let second_query = form_pairs(&requests[1].query);
        assert!(!first_query.contains_key("pageToken"));
        assert_eq!(second_query["pageToken"], "page-2");
        assert_eq!(first_query["q"], second_query["q"]);
    }

    #[tokio::test]
    async fn object_store_verifies_drive_binary_size() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            format!(
                r#"{{
                    "files":[{{
                        "id":"drive-file-123",
                        "name":"sha256-{OBJECT_OID}-42.lfs",
                        "size":"41",
                        "appProperties":{{
                            "lfsCloudVersion":"1",
                            "lfsCloudRepoNamespace":"github.com/owner/repo",
                            "lfsCloudOid":"{OBJECT_OID}",
                            "lfsCloudSize":"42"
                        }}
                    }}]
                }}"#
            ),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .lookup_object(&lfs_object())
            .await
            .expect_err("wrong Drive binary size should fail integrity");

        assert!(matches!(
            error,
            StorageError::IntegrityMismatch {
                expected_size: 42,
                actual_size: 41,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_missing_drive_binary_size_as_upstream_error() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            format!(
                r#"{{
                    "files":[{{
                        "id":"drive-file-123",
                        "name":"sha256-{OBJECT_OID}-42.lfs",
                        "appProperties":{{
                            "lfsCloudVersion":"1",
                            "lfsCloudRepoNamespace":"github.com/owner/repo",
                            "lfsCloudOid":"{OBJECT_OID}",
                            "lfsCloudSize":"42"
                        }}
                    }}]
                }}"#
            ),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .lookup_object(&lfs_object())
            .await
            .expect_err("missing size should be an upstream error");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(200),
                ref message,
            } if provider == "drive-user-a"
                && message.as_str()
                    == "Google Drive object lookup response did not include size"
        ));
    }

    #[tokio::test]
    async fn object_store_maps_auth_and_rate_limit_failures() {
        let auth_server = DriveFilesListServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"missing scope access-token","errors":[{"reason":"insufficientPermissions"}]}}"#,
        )
        .await;
        let auth_store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &auth_server.base_url,
        )
        .expect("object store should build");

        let auth_error = auth_store
            .object_exists(&lfs_object())
            .await
            .expect_err("insufficient scope should fail");
        assert!(matches!(
            auth_error,
            StorageError::AuthenticationRequired { ref provider } if provider == "drive-user-a"
        ));

        let rate_limit_server = DriveFilesListServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"try later access-token","errors":[{"reason":"rateLimitExceeded"}]}}"#,
        )
        .await;
        let rate_limit_store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &rate_limit_server.base_url,
        )
        .expect("object store should build");

        let rate_limit_error = rate_limit_store
            .object_exists(&lfs_object())
            .await
            .expect_err("rate limit should fail");
        assert!(matches!(
            rate_limit_error,
            StorageError::Retryable {
                provider,
                message,
            } if provider == "drive-user-a"
                && message.contains("try later")
                && !message.contains("access-token")
        ));
    }

    #[tokio::test]
    async fn object_store_uploads_staged_file_with_resumable_session() {
        let staged_bytes = b"0123456789abcdef0123456789abcdef0123456789";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start(drive_object_json(
            "drive-file-uploaded",
            object.oid.as_hex(),
            object.size.bytes(),
        ))
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let uploaded = store
            .upload_object(&object, staged_file.path())
            .await
            .expect("resumable upload should succeed");

        assert_eq!(uploaded.provider_id, "drive-user-a");
        assert_eq!(uploaded.object, object);
        assert_eq!(uploaded.backend_id, "drive-file-uploaded");

        let initiate_requests = server.initiate_requests();
        assert_eq!(initiate_requests.len(), 1);
        assert_eq!(
            initiate_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            initiate_requests[0].headers.get(CONTENT_TYPE).unwrap(),
            "application/json"
        );
        assert_eq!(
            initiate_requests[0]
                .headers
                .get("x-upload-content-type")
                .unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            initiate_requests[0]
                .headers
                .get("x-upload-content-length")
                .unwrap(),
            &object.size.bytes().to_string()
        );
        let query = form_pairs(&initiate_requests[0].query);
        assert_eq!(query["uploadType"], "resumable");
        assert_eq!(query["supportsAllDrives"], "true");
        assert_eq!(query["fields"], "id,name,size,appProperties");
        let metadata: serde_json::Value =
            serde_json::from_str(&initiate_requests[0].body).expect("metadata should be JSON");
        assert_eq!(
            metadata["name"],
            format!("sha256-{}-{}.lfs", object.oid.as_hex(), object.size.bytes())
        );
        assert_eq!(metadata["parents"], serde_json::json!(["drive-root"]));
        assert_eq!(
            metadata["appProperties"]["lfsCloudRepoNamespace"],
            "github.com/owner/repo"
        );
        assert_eq!(
            metadata["appProperties"]["lfsCloudOid"],
            object.oid.as_hex()
        );
        assert_eq!(
            metadata["appProperties"]["lfsCloudSize"],
            object.size.bytes().to_string()
        );

        let upload_requests = server.upload_requests();
        assert_eq!(upload_requests.len(), 1);
        assert_eq!(upload_requests[0].session_id, "session-1");
        assert_eq!(
            upload_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            upload_requests[0].headers.get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            upload_requests[0].headers.get(CONTENT_LENGTH).unwrap(),
            &object.size.bytes().to_string()
        );
        assert_eq!(upload_requests[0].body, staged_bytes);
    }

    #[tokio::test]
    async fn object_store_times_out_a_stalled_upload_response() {
        let staged_bytes = b"upload response idle timeout bytes";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start_with_upload_response_delay(
            drive_object_json(
                "drive-file-uploaded",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            Duration::from_secs(5),
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let client = reqwest::Client::new();
        let mut store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            client,
            &server.base_url,
        )
        .expect("object store should build");
        store.transfer_read_idle_timeout = Duration::from_millis(50);
        store.upload_retry_initial_backoff = Duration::ZERO;

        let error = store
            .upload_object(&object, staged_file.path())
            .await
            .expect_err("a stalled upload response should time out");

        assert!(matches!(
            error,
            StorageError::Retryable {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("idle timeout")
                && !message.contains("access-token")
        ));
        assert_eq!(
            server.upload_requests().len(),
            super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS as usize + 1
        );
    }

    #[tokio::test]
    async fn object_store_storage_provider_trait_uploads_to_drive() {
        let staged_bytes = b"storage provider migration upload bytes";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start(drive_object_json(
            "drive-file-uploaded",
            object.oid.as_hex(),
            object.size.bytes(),
        ))
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        let storage: &dyn StorageProvider = &store;

        let uploaded = storage
            .upload_object(&object, staged_file.path())
            .await
            .expect("trait-backed Drive upload should succeed");

        assert_eq!(uploaded.provider_id, "drive-user-a");
        assert_eq!(uploaded.object, object);
        assert_eq!(uploaded.backend_id, "drive-file-uploaded");
        assert_eq!(server.upload_requests()[0].body, staged_bytes);
    }

    #[tokio::test]
    async fn object_store_uploads_large_files_in_drive_aligned_chunks() {
        let staged_bytes = vec![b'x'; super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE * 2 + 17];
        let object = lfs_object_for_bytes(&staged_bytes);
        let first_chunk_end = super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE - 1;
        let second_chunk_end = super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE * 2 - 1;
        let server = DriveUploadServer::start_with_upload_responses(
            vec![
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{first_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{second_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::CREATED,
                    body: drive_object_json(
                        "drive-file-uploaded",
                        object.oid.as_hex(),
                        object.size.bytes(),
                    ),
                    range: None,
                    delay: Duration::ZERO,
                },
            ],
            None,
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), &staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        store
            .upload_object(&object, staged_file.path())
            .await
            .expect("chunked upload should succeed");

        let requests = server.upload_requests();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].body, staged_bytes[..=first_chunk_end]);
        assert_eq!(
            requests[1].body,
            staged_bytes[first_chunk_end + 1..=second_chunk_end]
        );
        assert_eq!(requests[2].body, staged_bytes[second_chunk_end + 1..]);
        assert_eq!(
            requests[0].headers.get("content-range").unwrap(),
            &format!("bytes 0-{first_chunk_end}/{}", staged_bytes.len())
        );
        assert_eq!(
            requests[1].headers.get("content-range").unwrap(),
            &format!(
                "bytes {}-{second_chunk_end}/{}",
                first_chunk_end + 1,
                staged_bytes.len()
            )
        );
        assert_eq!(
            requests[2].headers.get("content-range").unwrap(),
            &format!(
                "bytes {}-{}/{}",
                second_chunk_end + 1,
                staged_bytes.len() - 1,
                staged_bytes.len()
            )
        );
    }

    #[tokio::test]
    async fn object_store_probes_and_resumes_an_interrupted_upload_session() {
        let chunk_size = super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE;
        let staged_bytes = vec![b'r'; chunk_size * 2];
        let object = lfs_object_for_bytes(&staged_bytes);
        let first_chunk_end = chunk_size - 1;
        let partial_second_chunk_end = chunk_size + chunk_size / 2 - 1;
        let server = DriveUploadServer::start_with_upload_responses(
            vec![
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{first_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::SERVICE_UNAVAILABLE,
                    body: r#"{"error":{"message":"retry access-token"}}"#.to_owned(),
                    range: None,
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::from_u16(308).expect("308 should be valid"),
                    body: String::new(),
                    range: Some(format!("bytes=0-{partial_second_chunk_end}")),
                    delay: Duration::ZERO,
                },
                StubDriveUploadResponse {
                    status: StatusCode::CREATED,
                    body: drive_object_json(
                        "drive-file-uploaded",
                        object.oid.as_hex(),
                        object.size.bytes(),
                    ),
                    range: None,
                    delay: Duration::ZERO,
                },
            ],
            None,
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), &staged_bytes).expect("staged file should be written");
        let mut store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        store.upload_retry_initial_backoff = Duration::ZERO;

        store
            .upload_object(&object, staged_file.path())
            .await
            .expect("interrupted upload should resume");

        let requests = server.upload_requests();
        assert_eq!(requests.len(), 4);
        assert_eq!(requests[0].body, staged_bytes[..chunk_size]);
        assert_eq!(requests[1].body, staged_bytes[chunk_size..]);
        assert!(requests[2].body.is_empty());
        assert_eq!(
            requests[2].headers.get("content-range").unwrap(),
            &format!("bytes */{}", staged_bytes.len())
        );
        assert_eq!(
            requests[3].body,
            staged_bytes[partial_second_chunk_end + 1..]
        );
        assert_eq!(
            requests[3].headers.get("content-range").unwrap(),
            &format!(
                "bytes {}-{}/{}",
                partial_second_chunk_end + 1,
                staged_bytes.len() - 1,
                staged_bytes.len()
            )
        );
    }

    #[tokio::test]
    async fn object_store_bounds_resumable_upload_recovery_attempts() {
        let staged_bytes = b"bounded Drive resumable upload retries";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start_with_upload_response(
            StatusCode::SERVICE_UNAVAILABLE,
            r#"{"error":{"message":"still unavailable access-token"}}"#,
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let mut store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        store.upload_retry_initial_backoff = Duration::ZERO;

        let error = store
            .upload_object(&object, staged_file.path())
            .await
            .expect_err("repeated upload failures should stop");

        assert!(matches!(
            error,
            StorageError::Retryable { provider, message }
                if provider == "drive-user-a"
                    && message.contains("still unavailable")
                    && !message.contains("access-token")
        ));
        assert!(
            server.upload_requests().len()
                <= (super::GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS as usize * 2) + 1
        );
    }

    #[test]
    fn drive_upload_404_classification_is_phase_aware() {
        let storage = storage_config("google-drive-user-a");
        let token = access_token();
        let object = lfs_object();

        let initiate_error = super::parse_drive_upload_error(
            &storage,
            &token,
            &object,
            super::DriveUploadPhase::Initiate,
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"missing initiate endpoint access-token"}}"#,
        );
        assert!(matches!(
            initiate_error,
            StorageError::Upstream {
                ref provider,
                status: Some(404),
                ref message,
            } if provider == "drive-user-a"
                && message.as_str().contains("missing initiate endpoint")
                && !message.as_str().contains("access-token")
        ));

        let transfer_error = super::parse_drive_upload_error(
            &storage,
            &token,
            &object,
            super::DriveUploadPhase::Transfer,
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"expired session access-token"}}"#,
        );
        assert!(matches!(
            transfer_error,
            StorageError::Retryable {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("expired session")
                && !message.contains("access-token")
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_cross_origin_upload_session_before_put() {
        let staged_bytes = b"0123456789abcdef0123456789abcdef0123456789";
        let object = lfs_object_for_bytes(staged_bytes);
        let server = DriveUploadServer::start_with_session_url(
            "https://attacker.example/upload_session/session-1",
            drive_object_json(
                "drive-file-unused",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
        )
        .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), staged_bytes).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .upload_object(&object, staged_file.path())
            .await
            .expect_err("cross-origin session URL should fail before upload PUT");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: None,
                ref message,
            } if provider == "drive-user-a"
                && message.as_str()
                    == "Google Drive resumable upload session URL must match the configured Drive API origin"
        ));
        assert_eq!(server.initiate_requests().len(), 1);
        assert!(server.upload_requests().is_empty());
    }

    #[tokio::test]
    async fn object_store_rejects_staged_file_mismatch_before_drive_upload() {
        let server =
            DriveUploadServer::start(drive_object_list_json("drive-file-unused", OBJECT_OID, 42))
                .await;
        let staged_file = tempfile::NamedTempFile::new().expect("temp file should be created");
        std::fs::write(staged_file.path(), [b'x'; 42]).expect("staged file should be written");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .upload_object(&lfs_object(), staged_file.path())
            .await
            .expect_err("hash mismatch should fail before Drive upload starts");

        assert!(matches!(
            error,
            StorageError::IntegrityMismatch {
                ref expected_oid,
                expected_size: 42,
                actual_size: 42,
                ..
            } if expected_oid == OBJECT_OID
        ));
        assert!(server.initiate_requests().is_empty());
        assert!(server.upload_requests().is_empty());
    }

    #[tokio::test]
    async fn object_store_streams_download_response_from_verified_drive_file() {
        let object_bytes = b"0123456789abcdef0123456789abcdef0123456789";
        let object = lfs_object_for_bytes(object_bytes);
        let server = DriveDownloadServer::start(
            drive_object_list_json(
                "drive-file-download",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            StatusCode::OK,
            object_bytes.to_vec(),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let download = store
            .download_object_response(&object)
            .await
            .expect("Drive media download should stream");

        assert_eq!(download.stored_object().provider_id, "drive-user-a");
        assert_eq!(download.stored_object().object, object);
        assert_eq!(download.stored_object().backend_id, "drive-file-download");

        let response = download.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(CONTENT_TYPE).unwrap(),
            "application/octet-stream"
        );
        assert_eq!(
            response.headers().get(CONTENT_LENGTH).unwrap(),
            &object.size.bytes().to_string()
        );
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("download body should collect");
        assert_eq!(&body[..], object_bytes);

        let list_requests = server.list_requests();
        assert_eq!(list_requests.len(), 1);
        assert_eq!(
            list_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        let download_requests = server.download_requests();
        assert_eq!(download_requests.len(), 1);
        assert_eq!(download_requests[0].file_id, "drive-file-download");
        assert_eq!(
            download_requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        assert_eq!(
            download_requests[0].headers.get(ACCEPT_ENCODING).unwrap(),
            "identity"
        );
        let query = form_pairs(&download_requests[0].query);
        assert_eq!(query["alt"], "media");
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[tokio::test]
    async fn object_store_times_out_a_stalled_download_stream() {
        let object_bytes = b"download stream idle timeout bytes";
        let object = lfs_object_for_bytes(object_bytes);
        let server = DriveDownloadServer::start_with_download_delay(
            drive_object_list_json(
                "drive-file-download",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            object_bytes.to_vec(),
            Duration::from_secs(5),
        )
        .await;
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_millis(50))
            .build()
            .expect("test Drive client should build");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            client,
            &server.base_url,
        )
        .expect("object store should build");

        let download = store
            .download_object_response(&object)
            .await
            .expect("valid response metadata should begin proxying");
        let error = to_bytes(download.into_response().into_body(), usize::MAX)
            .await
            .expect_err("a stalled Drive body should terminate the proxy stream");

        assert!(error.to_string().contains("download stream failed"));
    }

    #[tokio::test]
    async fn object_store_storage_provider_trait_downloads_to_path() {
        let object_bytes = b"storage provider migration download bytes";
        let object = lfs_object_for_bytes(object_bytes);
        let server = DriveDownloadServer::start(
            drive_object_list_json(
                "drive-file-download",
                object.oid.as_hex(),
                object.size.bytes(),
            ),
            StatusCode::OK,
            object_bytes.to_vec(),
        )
        .await;
        let destination_root =
            tempfile::tempdir().expect("temporary download root should be created");
        let destination = destination_root.path().join("nested/object.bin");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");
        let storage: &dyn StorageProvider = &store;

        let downloaded = storage
            .download_object(&object, &destination)
            .await
            .expect("trait-backed Drive download should succeed");

        assert_eq!(downloaded.provider_id, "drive-user-a");
        assert_eq!(downloaded.object, object);
        assert_eq!(downloaded.backend_id, "drive-file-download");
        assert_eq!(
            std::fs::read(&destination).expect("downloaded file should be readable"),
            object_bytes
        );
    }

    #[tokio::test]
    async fn object_store_storage_provider_delete_retains_drive_objects() {
        let object = lfs_object_for_bytes(b"retained migration object bytes");
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            "http://127.0.0.1:1",
        )
        .expect("object store should build");
        let storage: &dyn StorageProvider = &store;

        let outcome = storage
            .delete_or_mark_object(&object)
            .await
            .expect("Drive object cleanup should retain objects for now");

        assert!(matches!(
            outcome,
            StorageDeleteOutcome::Retained { ref reason }
                if reason.contains("deletion is not implemented")
        ));
    }

    #[tokio::test]
    async fn object_store_maps_missing_lookup_to_download_object_not_found() {
        let server =
            DriveDownloadServer::start(r#"{"files":[]}"#, StatusCode::OK, Vec::new()).await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .download_object_response(&lfs_object())
            .await
            .expect_err("missing Drive object should not stream");

        assert!(matches!(
            error,
            StorageError::ObjectNotFound {
                ref provider,
                ref oid,
                size: 42,
            } if provider == "drive-user-a" && oid == OBJECT_OID
        ));
        assert!(server.download_requests().is_empty());
    }

    #[tokio::test]
    async fn object_store_classifies_download_provider_failures() {
        let auth_error = download_error_from(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"missing scope access-token","errors":[{"reason":"insufficientPermissions"}]}}"#,
        )
        .await;
        assert!(matches!(
            auth_error,
            StorageError::AuthenticationRequired { ref provider } if provider == "drive-user-a"
        ));
        assert!(!auth_error.to_string().contains("access-token"));

        let not_found_error = download_error_from(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"file missing","errors":[{"reason":"notFound"}]}}"#,
        )
        .await;
        assert!(matches!(
            not_found_error,
            StorageError::ObjectNotFound {
                ref provider,
                ref oid,
                size: 42,
            } if provider == "drive-user-a" && oid == OBJECT_OID
        ));

        let conflict_error =
            download_error_from(StatusCode::CONFLICT, r#"{"error":{"message":"conflict"}}"#).await;
        assert!(matches!(
            conflict_error,
            StorageError::Conflict {
                ref provider,
                ref oid,
            } if provider == "drive-user-a" && oid == OBJECT_OID
        ));

        let quota_error = download_error_from(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"storage full access-token","errors":[{"reason":"storageQuotaExceeded"}]}}"#,
        )
        .await;
        assert!(matches!(
            quota_error,
            StorageError::QuotaExceeded {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("storage full")
                && !message.contains("access-token")
        ));

        let retryable_error = download_error_from(
            StatusCode::TOO_MANY_REQUESTS,
            r#"{"error":{"message":"try later access-token","errors":[{"reason":"userRateLimitExceeded"}]}}"#,
        )
        .await;
        assert!(matches!(
            retryable_error,
            StorageError::Retryable {
                ref provider,
                ref message,
            } if provider == "drive-user-a"
                && message.contains("try later")
                && !message.contains("access-token")
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_download_content_length_mismatch() {
        let server = DriveDownloadServer::start(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 41],
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .download_object_response(&lfs_object())
            .await
            .expect_err("content-length mismatch should fail before streaming");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                ref message,
                ..
            } if provider == "drive-user-a"
                && message.as_str().contains("Content-Length 41")
                && message.as_str().contains("requested size 42")
        ));
    }

    #[tokio::test]
    async fn object_store_rejects_corrupt_download_stream() {
        let server = DriveDownloadServer::start(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 42],
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let download = store
            .download_object_response(&lfs_object())
            .await
            .expect("valid response metadata should begin proxying");
        let error = to_bytes(download.into_response().into_body(), usize::MAX)
            .await
            .expect_err("hash mismatch should terminate the response stream");
        assert!(error.to_string().contains("integrity verification"));
    }

    #[tokio::test]
    async fn object_store_rejects_truncated_download_stream() {
        let server = DriveDownloadServer::start_with_declared_download_content_length(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 41],
            Some(42),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = match store.download_object_response(&lfs_object()).await {
            Ok(download) => to_bytes(download.into_response().into_body(), usize::MAX)
                .await
                .expect_err("body stream should reject truncated response")
                .to_string(),
            Err(error) => error.to_string(),
        };
        assert!(
            error.contains("integrity mismatch")
                || error.contains("error decoding response body")
                || error.contains("Google Drive request failed")
        );
    }

    #[tokio::test]
    async fn object_store_rejects_download_without_content_length() {
        let server = DriveDownloadServer::start_without_download_content_length(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            StatusCode::OK,
            vec![b'x'; 42],
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        let error = store
            .download_object_response(&lfs_object())
            .await
            .expect_err("missing content-length should fail before streaming");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                ref message,
                ..
            } if provider == "drive-user-a" && message.as_str().contains("omitted Content-Length")
        ));
    }

    #[tokio::test]
    async fn root_validator_confirms_app_accessible_writable_folder() {
        let server = DriveMetadataServer::start(StatusCode::OK, drive_folder_json()).await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let folder = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect("root folder should validate");

        assert_eq!(folder.provider_id, "drive-user-a");
        assert_eq!(folder.id, "drive-root");
        assert_eq!(folder.name, "LFS Cloud Root");
        assert!(folder.can_add_children);

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].file_id, "drive-root");
        assert_eq!(
            requests[0].headers.get(AUTHORIZATION).unwrap(),
            "Bearer access-token"
        );
        let query = form_pairs(&requests[0].query);
        assert_eq!(
            query["fields"],
            "id,name,mimeType,trashed,capabilities(canAddChildren)"
        );
        assert_eq!(query["supportsAllDrives"], "true");
    }

    #[tokio::test]
    async fn root_validator_uses_existing_drive_api_base_path_once() {
        let server = DriveMetadataServer::start(StatusCode::OK, drive_folder_json()).await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            format!("{}/drive/v3", server.base_url),
        )
        .expect("validator should build");

        validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect("root folder should validate through Drive API base path");

        let requests = server.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].file_id, "drive-root");
    }

    #[tokio::test]
    async fn root_validator_rejects_non_folder_root_ids() {
        let server = DriveMetadataServer::start(
            StatusCode::OK,
            r#"{
                "id":"drive-root",
                "name":"not-a-folder.bin",
                "mimeType":"application/octet-stream",
                "capabilities":{"canAddChildren":true}
            }"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("non-folder root should fail");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(200),
                ..
            } if provider == "drive-user-a"
        ));
        assert!(error.to_string().contains("is not a folder"));
    }

    #[tokio::test]
    async fn root_validator_rejects_visible_folder_without_child_write_access() {
        let server = DriveMetadataServer::start(
            StatusCode::OK,
            r#"{
                "id":"drive-root",
                "name":"Read Only",
                "mimeType":"application/vnd.google-apps.folder",
                "capabilities":{"canAddChildren":false}
            }"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("read-only root should fail");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(200),
                ..
            } if provider == "drive-user-a"
        ));
        assert!(error.to_string().contains("cannot accept child objects"));
    }

    #[tokio::test]
    async fn root_validator_reports_missing_child_write_capability() {
        let server = DriveMetadataServer::start(
            StatusCode::OK,
            r#"{
                "id":"drive-root",
                "name":"Unexpected Shape",
                "mimeType":"application/vnd.google-apps.folder"
            }"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("missing canAddChildren should fail");

        assert!(error.to_string().contains("capabilities.canAddChildren"));
    }

    #[tokio::test]
    async fn root_validator_maps_missing_root_to_clear_upstream_error() {
        let server = DriveMetadataServer::start(
            StatusCode::NOT_FOUND,
            r#"{"error":{"message":"File not found: drive-root"}}"#,
        )
        .await;
        let validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("validator should build");

        let error = validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("missing root should fail");

        assert!(matches!(
            error,
            StorageError::Upstream {
                ref provider,
                status: Some(404),
                ..
            } if provider == "drive-user-a"
        ));
        assert!(
            error
                .to_string()
                .contains("was not found or is not accessible")
        );
    }

    #[tokio::test]
    async fn root_validator_maps_auth_and_rate_limit_failures() {
        let auth_server = DriveMetadataServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"missing scope access-token","errors":[{"reason":"insufficientPermissions"}]}}"#,
        )
        .await;
        let auth_validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &auth_server.base_url,
        )
        .expect("validator should build");

        let auth_error = auth_validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("insufficient scope should fail");
        assert!(matches!(
            auth_error,
            StorageError::AuthenticationRequired { ref provider } if provider == "drive-user-a"
        ));
        assert!(!auth_error.to_string().contains("access-token"));

        let rate_limit_server = DriveMetadataServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"try later access-token","errors":[{"reason":"rateLimitExceeded"}]}}"#,
        )
        .await;
        let rate_limit_validator = GoogleDriveRootValidator::with_client_and_api_base_url(
            reqwest::Client::new(),
            &rate_limit_server.base_url,
        )
        .expect("validator should build");

        let rate_limit_error = rate_limit_validator
            .validate_root_folder(&storage_config("google-drive-user-a"), &access_token())
            .await
            .expect_err("rate limit should fail");
        assert!(matches!(
            rate_limit_error,
            StorageError::Retryable {
                provider,
                message,
            } if provider == "drive-user-a"
                && message.contains("try later")
                && !message.contains("access-token")
        ));
    }

    async fn download_error_from(status: StatusCode, body: &'static str) -> StorageError {
        let server = DriveDownloadServer::start(
            drive_object_list_json("drive-file-download", OBJECT_OID, 42),
            status,
            body.as_bytes().to_vec(),
        )
        .await;
        let store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &server.base_url,
        )
        .expect("object store should build");

        store
            .download_object_response(&lfs_object())
            .await
            .expect_err("download should fail for this provider response")
    }

    fn storage_config(credential_ref: &str) -> GoogleDriveStorageConfig {
        GoogleDriveStorageConfig {
            id: "drive-user-a".to_owned(),
            credential_ref: credential_ref.to_owned(),
            root_folder_id: "drive-root".to_owned(),
            display_name: None,
        }
    }

    fn credential_json() -> String {
        r#"{"client_id":"client-id","client_secret":"client-secret","refresh_token":"refresh-token"}"#.to_owned()
    }

    fn credential_with_token_url(token_url: &str) -> GoogleDriveCredential {
        GoogleDriveCredential::from_json(
            "drive-user-a",
            "google-drive-user-a",
            &format!(
                r#"{{
                    "client_id":"client-id",
                    "client_secret":"client-secret",
                    "refresh_token":"refresh-token",
                    "token_uri":"{token_url}"
                }}"#
            ),
        )
        .expect("credential should parse")
    }

    fn access_token() -> super::GoogleDriveAccessToken {
        super::GoogleDriveAccessToken {
            access_token: "access-token".to_owned(),
            token_type: "Bearer".to_owned(),
            expires_in_seconds: Some(3600),
            scope: vec![super::GOOGLE_DRIVE_FILE_SCOPE.to_owned()],
        }
    }

    fn lfs_object() -> LfsObject {
        LfsObject::new(
            LfsOid::from_str(OBJECT_OID).expect("test OID should parse"),
            LfsObjectSize::new(42),
        )
    }

    fn lfs_object_for_bytes(bytes: &[u8]) -> LfsObject {
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        LfsObject::new(
            LfsOid::from_str(&format!("{:x}", hasher.finalize())).expect("test OID should parse"),
            LfsObjectSize::new(bytes.len() as u64),
        )
    }

    fn drive_folder_json() -> &'static str {
        r#"{
            "id":"drive-root",
            "name":"LFS Cloud Root",
            "mimeType":"application/vnd.google-apps.folder",
            "trashed":false,
            "capabilities":{"canAddChildren":true}
        }"#
    }

    fn drive_object_list_json(file_id: &str, oid: &str, size: u64) -> String {
        format!(r#"{{"files":[{}]}}"#, drive_object_json(file_id, oid, size))
    }

    fn drive_object_json(file_id: &str, oid: &str, size: u64) -> String {
        format!(
            r#"{{
                "id":"{file_id}",
                "name":"sha256-{oid}-{size}.lfs",
                "size":"{size}",
                "appProperties":{{
                    "lfsCloudVersion":"1",
                    "lfsCloudRepoNamespace":"github.com/owner/repo",
                    "lfsCloudOid":"{oid}",
                    "lfsCloudSize":"{size}"
                }}
            }}"#
        )
    }

    struct DriveFilesListServer {
        base_url: String,
        state: Arc<DriveFilesListServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveFilesListServer {
        async fn start(status: StatusCode, body: impl Into<String>) -> Self {
            Self::start_with_pages(status, body, BTreeMap::new()).await
        }

        async fn start_paginated(
            first_body: impl Into<String>,
            subsequent_pages: impl IntoIterator<Item = (&'static str, String)>,
        ) -> Self {
            Self::start_with_pages(
                StatusCode::OK,
                first_body,
                subsequent_pages
                    .into_iter()
                    .map(|(token, body)| (token.to_owned(), body))
                    .collect(),
            )
            .await
        }

        async fn start_with_pages(
            status: StatusCode,
            body: impl Into<String>,
            paginated_bodies: BTreeMap<String, String>,
        ) -> Self {
            let state = Arc::new(DriveFilesListServerState {
                status,
                body: body.into(),
                paginated_bodies,
                requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/drive/v3/files", get(drive_files_list_handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive files-list server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive files-list server address should be available");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive files-list server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        fn requests(&self) -> Vec<CapturedDriveFilesListRequest> {
            self.state
                .requests
                .lock()
                .expect("test Drive files-list requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveFilesListServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveFilesListServerState {
        status: StatusCode,
        body: String,
        paginated_bodies: BTreeMap<String, String>,
        requests: Mutex<Vec<CapturedDriveFilesListRequest>>,
    }

    #[derive(Clone)]
    struct CapturedDriveFilesListRequest {
        headers: HeaderMap,
        query: String,
    }

    async fn drive_files_list_handler(
        State(state): State<Arc<DriveFilesListServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        let query = uri.query().unwrap_or_default();
        state
            .requests
            .lock()
            .expect("test Drive files-list requests lock should not poison")
            .push(CapturedDriveFilesListRequest {
                headers,
                query: query.to_owned(),
            });
        let page_token = url::form_urlencoded::parse(query.as_bytes())
            .find_map(|(key, value)| (key == "pageToken").then(|| value.into_owned()));
        let body = page_token
            .as_deref()
            .and_then(|token| state.paginated_bodies.get(token))
            .unwrap_or(&state.body);

        (
            state.status,
            [(CONTENT_TYPE, "application/json")],
            body.clone(),
        )
            .into_response()
    }

    struct DriveDownloadServer {
        base_url: String,
        state: Arc<DriveDownloadServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveDownloadServer {
        async fn start(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
        ) -> Self {
            let declared_download_content_length = download_body.len() as u64;
            Self::start_with_declared_download_content_length(
                list_body,
                download_status,
                download_body,
                Some(declared_download_content_length),
            )
            .await
        }

        async fn start_with_download_delay(
            list_body: impl Into<String>,
            download_body: Vec<u8>,
            download_delay: Duration,
        ) -> Self {
            let declared_download_content_length = download_body.len() as u64;
            Self::start_with_declared_download_content_length_and_delay(
                list_body,
                StatusCode::OK,
                download_body,
                Some(declared_download_content_length),
                download_delay,
            )
            .await
        }

        async fn start_without_download_content_length(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
        ) -> Self {
            Self::start_with_declared_download_content_length(
                list_body,
                download_status,
                download_body,
                None,
            )
            .await
        }

        async fn start_with_declared_download_content_length(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
            declared_download_content_length: Option<u64>,
        ) -> Self {
            Self::start_with_declared_download_content_length_and_delay(
                list_body,
                download_status,
                download_body,
                declared_download_content_length,
                Duration::ZERO,
            )
            .await
        }

        async fn start_with_declared_download_content_length_and_delay(
            list_body: impl Into<String>,
            download_status: StatusCode,
            download_body: Vec<u8>,
            declared_download_content_length: Option<u64>,
            download_delay: Duration,
        ) -> Self {
            let state = Arc::new(DriveDownloadServerState {
                list_body: list_body.into(),
                download_status,
                download_body,
                declared_download_content_length,
                download_delay,
                list_requests: Mutex::new(Vec::new()),
                download_requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/drive/v3/files", get(drive_download_list_handler))
                .route(
                    "/drive/v3/files/{file_id}",
                    get(drive_download_media_handler),
                )
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive download server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive download server address should be available");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive download server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        fn list_requests(&self) -> Vec<CapturedDriveFilesListRequest> {
            self.state
                .list_requests
                .lock()
                .expect("test Drive download list requests lock should not poison")
                .clone()
        }

        fn download_requests(&self) -> Vec<CapturedDriveDownloadRequest> {
            self.state
                .download_requests
                .lock()
                .expect("test Drive download media requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveDownloadServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveDownloadServerState {
        list_body: String,
        download_status: StatusCode,
        download_body: Vec<u8>,
        declared_download_content_length: Option<u64>,
        download_delay: Duration,
        list_requests: Mutex<Vec<CapturedDriveFilesListRequest>>,
        download_requests: Mutex<Vec<CapturedDriveDownloadRequest>>,
    }

    #[derive(Clone)]
    struct CapturedDriveDownloadRequest {
        file_id: String,
        headers: HeaderMap,
        query: String,
    }

    async fn drive_download_list_handler(
        State(state): State<Arc<DriveDownloadServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        state
            .list_requests
            .lock()
            .expect("test Drive download list requests lock should not poison")
            .push(CapturedDriveFilesListRequest {
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
            });

        (
            StatusCode::OK,
            [(CONTENT_TYPE, "application/json")],
            state.list_body.clone(),
        )
            .into_response()
    }

    async fn drive_download_media_handler(
        Path(file_id): Path<String>,
        State(state): State<Arc<DriveDownloadServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        state
            .download_requests
            .lock()
            .expect("test Drive download media requests lock should not poison")
            .push(CapturedDriveDownloadRequest {
                file_id,
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
            });

        let response_body = if state.download_delay.is_zero() {
            Body::from_stream(ReaderStream::new(Cursor::new(state.download_body.clone())))
        } else {
            let download_delay = state.download_delay;
            let stream = futures_util::stream::unfold(
                Some(state.download_body.clone()),
                move |body| async move {
                    let body = body?;
                    tokio::time::sleep(download_delay).await;
                    Some((Ok::<_, std::io::Error>(Bytes::from(body)), None))
                },
            );
            Body::from_stream(stream)
        };
        let mut response = Response::builder()
            .status(state.download_status)
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(response_body)
            .expect("streaming download response should build");
        if let Some(content_length) = state.declared_download_content_length {
            response.headers_mut().insert(
                CONTENT_LENGTH,
                HeaderValue::from_str(&content_length.to_string())
                    .expect("download body length should be a valid header"),
            );
        }
        response
    }

    fn form_pairs(body: &str) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    struct TokenServer {
        url: String,
        state: Arc<TokenServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl TokenServer {
        async fn start(status: StatusCode, body: impl Into<String>) -> Self {
            let state = Arc::new(TokenServerState {
                status,
                body: body.into(),
                requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/token", post(token_handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test token server should bind");
            let address = listener
                .local_addr()
                .expect("test token server address should be available");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test token server should run");
            });

            Self {
                url: format!("http://{address}/token"),
                state,
                server_task,
            }
        }

        fn requests(&self) -> Vec<CapturedTokenRequest> {
            self.state
                .requests
                .lock()
                .expect("test token requests lock should not poison")
                .clone()
        }
    }

    impl Drop for TokenServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct TokenServerState {
        status: StatusCode,
        body: String,
        requests: Mutex<Vec<CapturedTokenRequest>>,
    }

    #[derive(Clone)]
    struct CapturedTokenRequest {
        headers: HeaderMap,
        body: String,
    }

    async fn token_handler(
        State(state): State<Arc<TokenServerState>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        state
            .requests
            .lock()
            .expect("test token requests lock should not poison")
            .push(CapturedTokenRequest {
                headers,
                body: String::from_utf8(body.to_vec()).expect("token request body should be UTF-8"),
            });

        (
            state.status,
            [(CONTENT_TYPE, "application/json")],
            state.body.clone(),
        )
            .into_response()
    }

    struct DriveMetadataServer {
        base_url: String,
        state: Arc<DriveMetadataServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveMetadataServer {
        async fn start(status: StatusCode, body: impl Into<String>) -> Self {
            let state = Arc::new(DriveMetadataServerState {
                status,
                body: body.into(),
                requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route("/drive/v3/files/{file_id}", get(drive_metadata_handler))
                .with_state(state.clone());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive metadata server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive metadata server address should be available");
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive metadata server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        fn requests(&self) -> Vec<CapturedDriveMetadataRequest> {
            self.state
                .requests
                .lock()
                .expect("test Drive metadata requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveMetadataServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveMetadataServerState {
        status: StatusCode,
        body: String,
        requests: Mutex<Vec<CapturedDriveMetadataRequest>>,
    }

    #[derive(Clone)]
    struct CapturedDriveMetadataRequest {
        file_id: String,
        headers: HeaderMap,
        query: String,
    }

    async fn drive_metadata_handler(
        Path(file_id): Path<String>,
        State(state): State<Arc<DriveMetadataServerState>>,
        headers: HeaderMap,
        uri: Uri,
    ) -> Response {
        state
            .requests
            .lock()
            .expect("test Drive metadata requests lock should not poison")
            .push(CapturedDriveMetadataRequest {
                file_id,
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
            });

        (
            state.status,
            [(CONTENT_TYPE, "application/json")],
            state.body.clone(),
        )
            .into_response()
    }

    struct DriveUploadServer {
        base_url: String,
        state: Arc<DriveUploadServerState>,
        server_task: tokio::task::JoinHandle<()>,
    }

    impl DriveUploadServer {
        async fn start(upload_body: impl Into<String>) -> Self {
            Self::start_with_upload_response(StatusCode::CREATED, upload_body).await
        }

        async fn start_with_upload_response_delay(
            upload_body: impl Into<String>,
            upload_response_delay: Duration,
        ) -> Self {
            Self::start_with_upload_response_session_url_and_delay(
                StatusCode::CREATED,
                upload_body,
                None,
                upload_response_delay,
            )
            .await
        }

        async fn start_with_session_url(
            session_url: impl Into<String>,
            upload_body: impl Into<String>,
        ) -> Self {
            Self::start_with_upload_response_and_session_url(
                StatusCode::CREATED,
                upload_body,
                Some(session_url.into()),
            )
            .await
        }

        async fn start_with_upload_response(
            upload_status: StatusCode,
            upload_body: impl Into<String>,
        ) -> Self {
            Self::start_with_upload_response_and_session_url(upload_status, upload_body, None).await
        }

        async fn start_with_upload_response_and_session_url(
            upload_status: StatusCode,
            upload_body: impl Into<String>,
            session_url: Option<String>,
        ) -> Self {
            Self::start_with_upload_response_session_url_and_delay(
                upload_status,
                upload_body,
                session_url,
                Duration::ZERO,
            )
            .await
        }

        async fn start_with_upload_response_session_url_and_delay(
            upload_status: StatusCode,
            upload_body: impl Into<String>,
            session_url: Option<String>,
            upload_response_delay: Duration,
        ) -> Self {
            Self::start_with_upload_responses(
                vec![StubDriveUploadResponse {
                    status: upload_status,
                    body: upload_body.into(),
                    range: None,
                    delay: upload_response_delay,
                }],
                session_url,
            )
            .await
        }

        async fn start_with_upload_responses(
            upload_responses: Vec<StubDriveUploadResponse>,
            session_url: Option<String>,
        ) -> Self {
            assert!(!upload_responses.is_empty());
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive upload server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive upload server address should be available");
            let state = Arc::new(DriveUploadServerState {
                session_url: session_url
                    .unwrap_or_else(|| format!("http://{address}/upload_session/session-1")),
                initiate_status: StatusCode::OK,
                initiate_body: String::new(),
                upload_responses,
                initiate_requests: Mutex::new(Vec::new()),
                upload_requests: Mutex::new(Vec::new()),
            });
            let app = Router::new()
                .route(
                    "/upload/drive/v3/files",
                    post(drive_upload_initiate_handler),
                )
                .route(
                    "/upload_session/{session_id}",
                    put(drive_upload_session_handler),
                )
                .with_state(state.clone());
            let server_task = tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive upload server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
                server_task,
            }
        }

        fn initiate_requests(&self) -> Vec<CapturedDriveUploadInitiateRequest> {
            self.state
                .initiate_requests
                .lock()
                .expect("test Drive upload initiate requests lock should not poison")
                .clone()
        }

        fn upload_requests(&self) -> Vec<CapturedDriveUploadRequest> {
            self.state
                .upload_requests
                .lock()
                .expect("test Drive upload requests lock should not poison")
                .clone()
        }
    }

    impl Drop for DriveUploadServer {
        fn drop(&mut self) {
            self.server_task.abort();
        }
    }

    struct DriveUploadServerState {
        session_url: String,
        initiate_status: StatusCode,
        initiate_body: String,
        upload_responses: Vec<StubDriveUploadResponse>,
        initiate_requests: Mutex<Vec<CapturedDriveUploadInitiateRequest>>,
        upload_requests: Mutex<Vec<CapturedDriveUploadRequest>>,
    }

    #[derive(Clone)]
    struct StubDriveUploadResponse {
        status: StatusCode,
        body: String,
        range: Option<String>,
        delay: Duration,
    }

    #[derive(Clone)]
    struct CapturedDriveUploadInitiateRequest {
        headers: HeaderMap,
        query: String,
        body: String,
    }

    #[derive(Clone)]
    struct CapturedDriveUploadRequest {
        session_id: String,
        headers: HeaderMap,
        body: Vec<u8>,
    }

    async fn drive_upload_initiate_handler(
        State(state): State<Arc<DriveUploadServerState>>,
        headers: HeaderMap,
        uri: Uri,
        body: Bytes,
    ) -> Response {
        state
            .initiate_requests
            .lock()
            .expect("test Drive upload initiate requests lock should not poison")
            .push(CapturedDriveUploadInitiateRequest {
                headers,
                query: uri.query().unwrap_or_default().to_owned(),
                body: String::from_utf8(body.to_vec())
                    .expect("initiate metadata body should be UTF-8"),
            });

        let mut response = (
            state.initiate_status,
            [(CONTENT_TYPE, "application/json")],
            state.initiate_body.clone(),
        )
            .into_response();
        if state.initiate_status.is_success() {
            response.headers_mut().insert(
                LOCATION,
                HeaderValue::from_str(&state.session_url)
                    .expect("session URL should be a valid header"),
            );
        }
        response
    }

    async fn drive_upload_session_handler(
        Path(session_id): Path<String>,
        State(state): State<Arc<DriveUploadServerState>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let request_index = {
            let mut upload_requests = state
                .upload_requests
                .lock()
                .expect("test Drive upload requests lock should not poison");
            let request_index = upload_requests.len();
            upload_requests.push(CapturedDriveUploadRequest {
                session_id,
                headers,
                body: body.to_vec(),
            });
            request_index
        };

        let stub_response = state
            .upload_responses
            .get(request_index)
            .or_else(|| state.upload_responses.last())
            .expect("test Drive upload response sequence should not be empty")
            .clone();
        tokio::time::sleep(stub_response.delay).await;

        let mut response = (
            stub_response.status,
            [(CONTENT_TYPE, "application/json")],
            stub_response.body.clone(),
        )
            .into_response();
        if let Some(range) = &stub_response.range {
            response.headers_mut().insert(
                "range",
                HeaderValue::from_str(range).expect("test upload range should be valid"),
            );
        }
        response
    }
}
