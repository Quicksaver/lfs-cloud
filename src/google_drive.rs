//! Google Drive storage-provider authentication helpers.
//!
//! This module loads server-owned Google Drive OAuth credentials from
//! configuration references and exchanges refresh tokens for short-lived
//! access tokens. It does not expose Drive credentials to Git LFS clients.

use std::{fmt, net::IpAddr, sync::OnceLock, time::Duration};

use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderValue},
};
use serde::Deserialize;
use url::Url;

use crate::{GoogleDriveStorageConfig, SanitizedMessage, StorageError, StorageResult};

const GOOGLE_DRIVE_TOKEN_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_ROOT_VALIDATION_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_GOOGLE_DRIVE_CREDENTIAL_ENV_PREFIX: &str = "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_";
const MAX_GOOGLE_OAUTH_ERROR_BODY_LEN: usize = 16 * 1024;
const GOOGLE_DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";

static DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

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
        let response_body = read_google_oauth_response_body(response)
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
        let response_body = read_google_oauth_response_body(response)
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

fn default_google_drive_http_client() -> StorageResult<Client> {
    if let Some(client) = DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .timeout(GOOGLE_DRIVE_TOKEN_REFRESH_TIMEOUT)
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive HTTP client: {source}"),
        })?;

    match DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT
            .get()
            .cloned()
            .unwrap_or(client)),
    }
}

fn default_google_drive_root_validation_http_client() -> StorageResult<Client> {
    if let Some(client) = DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT.get() {
        return Ok(client.clone());
    }

    let client = Client::builder()
        .timeout(GOOGLE_DRIVE_ROOT_VALIDATION_TIMEOUT)
        .build()
        .map_err(|source| StorageError::Retryable {
            provider: "google_drive".to_owned(),
            message: format!("failed to initialize Google Drive HTTP client: {source}"),
        })?;

    match DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT.set(client.clone()) {
        Ok(()) => Ok(client),
        Err(client) => Ok(DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT
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

fn drive_file_metadata_url(mut api_base_url: Url, root_folder_id: &str) -> StorageResult<Url> {
    if root_folder_id.trim().is_empty() {
        return Err(StorageError::Upstream {
            provider: "google_drive".to_owned(),
            status: None,
            message: SanitizedMessage::new("Google Drive root_folder_id must not be blank"),
        });
    }

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
        segments.extend(["drive", "v3", "files", root_folder_id]);
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
    if metadata.capabilities.can_add_children != Some(true) {
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
        can_add_children: true,
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

fn drive_error_message(token: &GoogleDriveAccessToken, body: &str) -> DriveDiagnostic {
    let capped = body
        .chars()
        .take(MAX_GOOGLE_OAUTH_ERROR_BODY_LEN)
        .collect::<String>();
    if let Ok(error) = serde_json::from_str::<GoogleDriveErrorResponse>(&capped)
        && let Some(error) = error.error
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
            message: sanitize_drive_diagnostic(token, &message),
            reasons,
        };
    }

    DriveDiagnostic {
        message: sanitize_drive_diagnostic(token, &capped),
        reasons: Vec::new(),
    }
}

struct DriveDiagnostic {
    message: String,
    reasons: Vec<String>,
}

fn sanitize_drive_diagnostic(token: &GoogleDriveAccessToken, message: &str) -> String {
    let sanitized = message.replace(token.as_str(), "[redacted]");
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
    let capped = body
        .chars()
        .take(MAX_GOOGLE_OAUTH_ERROR_BODY_LEN)
        .collect::<String>();
    if let Ok(error) = serde_json::from_str::<GoogleDriveTokenError>(&capped) {
        let code = error.error.filter(|value| !value.trim().is_empty());
        let message = error
            .error_description
            .or_else(|| code.clone())
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Google OAuth token refresh failed".to_owned());
        return GoogleTokenDiagnostic {
            code,
            message: sanitize_google_diagnostic(credential, &message),
        };
    }

    GoogleTokenDiagnostic {
        code: None,
        message: sanitize_google_diagnostic(credential, &capped),
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
        if !secret.is_empty() {
            sanitized = sanitized.replace(secret, "[redacted]");
        }
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

async fn read_google_oauth_response_body(
    mut response: reqwest::Response,
) -> Result<String, reqwest::Error> {
    let mut body = Vec::new();
    while body.len() < MAX_GOOGLE_OAUTH_ERROR_BODY_LEN {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let remaining = MAX_GOOGLE_OAUTH_ERROR_BODY_LEN - body.len();
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
        sync::{Arc, Mutex},
    };

    use axum::{
        Router,
        body::Bytes,
        extract::{Path, State},
        http::{
            HeaderMap, Uri,
            header::{AUTHORIZATION, CONTENT_TYPE},
        },
        response::{IntoResponse, Response},
        routing::{get, post},
    };
    use reqwest::StatusCode;

    use super::{
        GOOGLE_OAUTH_TOKEN_URL, GoogleDriveCredential, GoogleDriveCredentialLoader,
        GoogleDriveRootValidator, GoogleDriveTokenRefresher,
    };
    use crate::{GoogleDriveStorageConfig, StorageError};

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
            "x".repeat(super::MAX_GOOGLE_OAUTH_ERROR_BODY_LEN)
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
                && message.len() == super::MAX_GOOGLE_OAUTH_ERROR_BODY_LEN
                && !message.contains("after-limit")
                && !message.contains("client-secret")
                && !message.contains("refresh-token")
        ));
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

    fn drive_folder_json() -> &'static str {
        r#"{
            "id":"drive-root",
            "name":"LFS Cloud Root",
            "mimeType":"application/vnd.google-apps.folder",
            "trashed":false,
            "capabilities":{"canAddChildren":true}
        }"#
    }

    fn form_pairs(body: &str) -> BTreeMap<String, String> {
        url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }

    #[derive(Clone)]
    struct TokenServer {
        url: String,
        state: Arc<TokenServerState>,
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
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test token server should run");
            });

            Self {
                url: format!("http://{address}/token"),
                state,
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

    #[derive(Clone)]
    struct DriveMetadataServer {
        base_url: String,
        state: Arc<DriveMetadataServerState>,
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
            tokio::spawn(async move {
                axum::serve(listener, app)
                    .await
                    .expect("test Drive metadata server should run");
            });

            Self {
                base_url: format!("http://{address}"),
                state,
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
}
