//! Google Drive storage-provider authentication helpers.
//!
//! This module loads server-owned Google Drive OAuth credentials from
//! configuration references and exchanges refresh tokens for short-lived
//! access tokens. It does not expose Drive credentials to Git LFS clients.

use std::{
    collections::BTreeMap,
    fmt,
    fs::File,
    io::{BufReader, Read},
    net::IpAddr,
    path::Path,
    sync::OnceLock,
    time::Duration,
};

use reqwest::{
    Body, Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, LOCATION},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    GoogleDriveStorageConfig, LfsObject, SanitizedMessage, StorageError, StorageResult,
    StoredObject,
};

const GOOGLE_DRIVE_TOKEN_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_ROOT_VALIDATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_GOOGLE_DRIVE_CREDENTIAL_ENV_PREFIX: &str = "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_";
const MAX_GOOGLE_ERROR_BODY_LEN: usize = 16 * 1024;
const MIN_REDACTED_SECRET_FRAGMENT_LEN: usize = 6;
const GOOGLE_DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
const GOOGLE_DRIVE_OBJECT_CONTENT_TYPE: &str = "application/octet-stream";
const GOOGLE_DRIVE_OBJECT_VERSION: &str = "1";
const GOOGLE_DRIVE_OBJECT_VERSION_PROPERTY: &str = "lfsCloudVersion";
const GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY: &str = "lfsCloudRepoNamespace";
const GOOGLE_DRIVE_OBJECT_OID_PROPERTY: &str = "lfsCloudOid";
const GOOGLE_DRIVE_OBJECT_SIZE_PROPERTY: &str = "lfsCloudSize";

static DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_OBJECT_STORE_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

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
            repo_namespace: self.repo_namespace.clone(),
            oid: self.object.oid.as_hex().to_owned(),
            size: self.object.size.bytes().to_string(),
        }
    }
}

struct GoogleDriveObjectProperties {
    repo_namespace: String,
    oid: String,
    size: String,
}

impl GoogleDriveObjectProperties {
    fn pairs(&self) -> [(&'static str, &str); 4] {
        [
            (
                GOOGLE_DRIVE_OBJECT_VERSION_PROPERTY,
                GOOGLE_DRIVE_OBJECT_VERSION,
            ),
            (GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY, &self.repo_namespace),
            (GOOGLE_DRIVE_OBJECT_OID_PROPERTY, &self.oid),
            (GOOGLE_DRIVE_OBJECT_SIZE_PROPERTY, &self.size),
        ]
    }
}

/// Looks up repository-scoped LFS objects in Google Drive.
#[derive(Clone)]
pub struct GoogleDriveObjectStore {
    storage: GoogleDriveStorageConfig,
    repo_namespace: String,
    token: GoogleDriveAccessToken,
    client: Client,
    api_base_url: Url,
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
        Self::with_client_and_api_base_url(
            storage,
            repo_namespace,
            token,
            default_google_drive_object_store_http_client()?,
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
        Ok(Self {
            storage,
            repo_namespace: validate_repo_namespace(repo_namespace.as_ref())?,
            token,
            client,
            api_base_url: validate_drive_api_base_url(api_base_url.as_ref())?,
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
    /// Drive failures, malformed Drive responses, or conflicting duplicate
    /// files for the same repository/OID/size tuple.
    pub async fn object_exists(&self, object: &LfsObject) -> StorageResult<bool> {
        Ok(self.lookup_object(object).await?.is_some())
    }

    /// Returns verified backend metadata for an existing Drive object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] for backend authentication failures, retryable
    /// Drive failures, malformed Drive responses, or conflicting duplicate
    /// files for the same repository/OID/size tuple.
    pub async fn lookup_object(&self, object: &LfsObject) -> StorageResult<Option<StoredObject>> {
        let key = self.object_key(object)?;
        let expected_properties = key.expected_app_properties();
        let response = self
            .client
            .get(drive_object_lookup_url(
                self.api_base_url.clone(),
                &self.storage.root_folder_id,
                &key,
                &expected_properties,
            )?)
            .header(ACCEPT, "application/json")
            .header(
                AUTHORIZATION,
                self.token.authorization_header_value(&self.storage.id)?,
            )
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

        parse_drive_object_lookup_success(
            &self.storage,
            &key,
            &expected_properties,
            status,
            &response_body,
        )
    }

    /// Uploads a staged and locally verified object file through Drive resumable upload.
    ///
    /// The staged file is read before any Drive request so its SHA-256 and
    /// byte count can be checked against the LFS pointer metadata. The current
    /// implementation sends the resumable upload content in one request after
    /// session initiation; if Drive reports an interrupted session, callers get
    /// a retryable storage error and may retry the whole upload.
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
        let source = source.as_ref();
        verify_staged_upload_file(&self.storage, object, source)?;

        let key = self.object_key(object)?;
        let expected_properties = key.expected_app_properties();
        let metadata = drive_upload_metadata(&self.storage.root_folder_id, &key);
        let initiate_response = self
            .client
            .post(drive_resumable_upload_url(self.api_base_url.clone())?)
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
            initiate_response
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
                })?
                .to_owned()
        } else {
            let response_body = read_google_response_body(initiate_response)
                .await
                .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
            return Err(parse_drive_upload_error(
                &self.storage,
                &self.token,
                object,
                initiate_status,
                &response_body,
            ));
        };

        let file = tokio::fs::File::open(source)
            .await
            .map_err(|error| staged_file_read_error(&self.storage, source, error))?;
        let upload_response = self
            .client
            .put(session_url)
            .header(ACCEPT, "application/json")
            .header(CONTENT_TYPE, GOOGLE_DRIVE_OBJECT_CONTENT_TYPE)
            .header(CONTENT_LENGTH, object.size.bytes().to_string())
            .body(Body::from(file))
            .send()
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;
        let upload_status = upload_response.status();
        let upload_body = read_google_response_body(upload_response)
            .await
            .map_err(|source| drive_transport_error(&self.storage, &self.token, source))?;

        if !matches!(upload_status, StatusCode::OK | StatusCode::CREATED) {
            return Err(parse_drive_upload_error(
                &self.storage,
                &self.token,
                object,
                upload_status,
                &upload_body,
            ));
        }

        parse_drive_upload_success(
            &self.storage,
            &key,
            &expected_properties,
            upload_status,
            &upload_body,
        )
    }
}

impl fmt::Debug for GoogleDriveObjectStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveObjectStore")
            .field("storage", &self.storage)
            .field("repo_namespace", &self.repo_namespace)
            .field("token", &"<redacted>")
            .field("client", &"<redacted>")
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

fn default_google_drive_object_store_http_client() -> StorageResult<Client> {
    if let Some(client) = DEFAULT_GOOGLE_DRIVE_OBJECT_STORE_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive object HTTP client: {source}"),
        })?;

    match DEFAULT_GOOGLE_DRIVE_OBJECT_STORE_HTTP_CLIENT.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(DEFAULT_GOOGLE_DRIVE_OBJECT_STORE_HTTP_CLIENT
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
    if status == StatusCode::UNAUTHORIZED
        || diagnostic
            .reasons
            .iter()
            .any(|reason| matches!(reason.as_str(), "authError" | "insufficientPermissions"))
    {
        return StorageError::AuthenticationRequired {
            provider: storage.id.clone(),
        };
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
    if diagnostic
        .reasons
        .iter()
        .any(|reason| matches!(reason.as_str(), "quotaExceeded" | "storageQuotaExceeded"))
    {
        return StorageError::QuotaExceeded {
            provider: storage.id.clone(),
            message: diagnostic.message,
        };
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

fn parse_drive_object_lookup_success(
    storage: &GoogleDriveStorageConfig,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
    status: StatusCode,
    body: &str,
) -> StorageResult<Option<StoredObject>> {
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
    if response.next_page_token.is_some() || response.files.len() > 1 {
        return Err(StorageError::Conflict {
            provider: storage.id.clone(),
            oid: key.object.oid.as_hex().to_owned(),
        });
    }

    let Some(file) = response.files.into_iter().next() else {
        return Ok(None);
    };
    verify_drive_object_file(storage, key, expected_properties, status, file).map(Some)
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

fn verify_staged_upload_file(
    storage: &GoogleDriveStorageConfig,
    object: &LfsObject,
    source: &Path,
) -> StorageResult<()> {
    let file =
        File::open(source).map_err(|error| staged_file_read_error(storage, source, error))?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];

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

    Ok(())
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
    if status == StatusCode::UNAUTHORIZED
        || diagnostic
            .reasons
            .iter()
            .any(|reason| matches!(reason.as_str(), "authError" | "insufficientPermissions"))
    {
        return StorageError::AuthenticationRequired {
            provider: storage.id.clone(),
        };
    }
    if diagnostic
        .reasons
        .iter()
        .any(|reason| matches!(reason.as_str(), "quotaExceeded" | "storageQuotaExceeded"))
    {
        return StorageError::QuotaExceeded {
            provider: storage.id.clone(),
            message: diagnostic.message,
        };
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

fn parse_drive_upload_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    object: &LfsObject,
    status: StatusCode,
    body: &str,
) -> StorageError {
    let diagnostic = drive_error_message(token, body);
    if status == StatusCode::UNAUTHORIZED
        || diagnostic
            .reasons
            .iter()
            .any(|reason| matches!(reason.as_str(), "authError" | "insufficientPermissions"))
    {
        return StorageError::AuthenticationRequired {
            provider: storage.id.clone(),
        };
    }
    if status == StatusCode::CONFLICT {
        return StorageError::Conflict {
            provider: storage.id.clone(),
            oid: object.oid.as_hex().to_owned(),
        };
    }
    if diagnostic
        .reasons
        .iter()
        .any(|reason| matches!(reason.as_str(), "quotaExceeded" | "storageQuotaExceeded"))
    {
        return StorageError::QuotaExceeded {
            provider: storage.id.clone(),
            message: diagnostic.message,
        };
    }
    if status == StatusCode::NOT_FOUND
        || status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || diagnostic.reasons.iter().any(|reason| {
            matches!(
                reason.as_str(),
                "rateLimitExceeded" | "userRateLimitExceeded"
            )
        })
    {
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
        str::FromStr,
        sync::{Arc, Mutex},
    };

    use axum::{
        Router,
        body::Bytes,
        extract::{Path, State},
        http::{
            HeaderMap, HeaderValue, Uri,
            header::{AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, LOCATION},
        },
        response::{IntoResponse, Response},
        routing::{get, post, put},
    };
    use reqwest::StatusCode;
    use sha2::{Digest, Sha256};

    use super::{
        GOOGLE_OAUTH_TOKEN_URL, GoogleDriveCredential, GoogleDriveCredentialLoader,
        GoogleDriveObjectKey, GoogleDriveObjectStore, GoogleDriveRootValidator,
        GoogleDriveTokenRefresher,
    };
    use crate::{GoogleDriveStorageConfig, LfsObject, LfsObjectSize, LfsOid, StorageError};

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
    fn drive_object_lookup_url_searches_with_private_app_properties() {
        let key = GoogleDriveObjectKey::new("github.com/owner/repo", lfs_object())
            .expect("key should build");
        let url = super::drive_object_lookup_url(
            url::Url::parse("http://localhost/proxy/drive/v3").expect("base URL should parse"),
            "drive-root",
            &key,
            &key.expected_app_properties(),
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
    async fn object_store_rejects_duplicate_drive_matches() {
        let server = DriveFilesListServer::start(
            StatusCode::OK,
            format!(
                r#"{{
                    "files":[
                        {},
                        {}
                    ]
                }}"#,
                drive_object_json("drive-file-a", OBJECT_OID, 42),
                drive_object_json("drive-file-b", OBJECT_OID, 42)
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
            .expect_err("duplicate Drive matches should conflict");

        assert!(matches!(
            error,
            StorageError::Conflict {
                ref provider,
                ref oid,
            } if provider == "drive-user-a" && oid == OBJECT_OID
        ));
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
            let state = Arc::new(DriveFilesListServerState {
                status,
                body: body.into(),
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
        state
            .requests
            .lock()
            .expect("test Drive files-list requests lock should not poison")
            .push(CapturedDriveFilesListRequest {
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
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test Drive upload server should bind");
            let address = listener
                .local_addr()
                .expect("test Drive upload server address should be available");
            let state = Arc::new(DriveUploadServerState {
                session_url: format!("http://{address}/upload_session/session-1"),
                initiate_status: StatusCode::OK,
                initiate_body: String::new(),
                upload_status: StatusCode::CREATED,
                upload_body: upload_body.into(),
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
        upload_status: StatusCode,
        upload_body: String,
        initiate_requests: Mutex<Vec<CapturedDriveUploadInitiateRequest>>,
        upload_requests: Mutex<Vec<CapturedDriveUploadRequest>>,
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
        state
            .upload_requests
            .lock()
            .expect("test Drive upload requests lock should not poison")
            .push(CapturedDriveUploadRequest {
                session_id,
                headers,
                body: body.to_vec(),
            });

        (
            state.upload_status,
            [(CONTENT_TYPE, "application/json")],
            state.upload_body.clone(),
        )
            .into_response()
    }
}
