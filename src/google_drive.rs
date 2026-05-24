//! Google Drive storage-provider authentication helpers.
//!
//! This module loads server-owned Google Drive OAuth credentials from
//! configuration references and exchanges refresh tokens for short-lived
//! access tokens. It does not expose Drive credentials to Git LFS clients.

use std::{fmt, sync::OnceLock, time::Duration};

use reqwest::{
    Client, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde::Deserialize;
use url::Url;

use crate::{GoogleDriveStorageConfig, SanitizedMessage, StorageError, StorageResult};

const GOOGLE_DRIVE_TOKEN_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_GOOGLE_DRIVE_CREDENTIAL_ENV_PREFIX: &str = "LFS_CLOUD_GOOGLE_DRIVE_CREDENTIAL_";
const MAX_GOOGLE_OAUTH_ERROR_BODY_LEN: usize = 16 * 1024;

static DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();

/// Google OAuth token endpoint used by Drive refresh-token exchange.
pub const GOOGLE_OAUTH_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

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
    /// missing, or `token_uri` is not an absolute HTTP(S) URL.
    pub fn from_json(
        provider_id: impl Into<String>,
        credential_ref: impl Into<String>,
        contents: &str,
    ) -> StorageResult<Self> {
        let provider_id = provider_id.into();
        let credential_ref = credential_ref.into();
        let raw = serde_json::from_str::<RawGoogleDriveCredential>(contents).map_err(|_| {
            credential_load_error(&provider_id, &credential_ref, "credential JSON is invalid")
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
        let client = DEFAULT_GOOGLE_DRIVE_HTTP_CLIENT
            .get_or_init(|| {
                Client::builder()
                    .timeout(GOOGLE_DRIVE_TOKEN_REFRESH_TIMEOUT)
                    .build()
                    .map_err(|source| source.to_string())
            })
            .as_ref()
            .map_err(|message| StorageError::Retryable {
                provider: "google_drive".to_owned(),
                message: format!("failed to initialize Google Drive HTTP client: {message}"),
            })?
            .clone();

        Ok(Self { client })
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
            parse_token_success(credential, &response_body)
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
    if !url.username().is_empty() || url.password().is_some() {
        return Err(credential_load_error(
            provider,
            reference,
            "token_uri must not include credentials",
        ));
    }

    Ok(url)
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
    body: &str,
) -> StorageResult<GoogleDriveAccessToken> {
    let response = serde_json::from_str::<GoogleDriveTokenSuccess>(body).map_err(|_| {
        StorageError::Upstream {
            provider: credential.provider_id.clone(),
            status: None,
            message: SanitizedMessage::new("Google OAuth token response was invalid JSON"),
        }
    })?;
    let access_token = response
        .access_token
        .filter(|token| !token.trim().is_empty())
        .ok_or_else(|| StorageError::Upstream {
            provider: credential.provider_id.clone(),
            status: None,
            message: SanitizedMessage::new(
                "Google OAuth token response did not include access_token",
            ),
        })?;
    let token_type = response
        .token_type
        .filter(|token_type| !token_type.trim().is_empty())
        .unwrap_or_else(|| "Bearer".to_owned());
    if !token_type.eq_ignore_ascii_case("Bearer") {
        return Err(StorageError::Upstream {
            provider: credential.provider_id.clone(),
            status: None,
            message: SanitizedMessage::new(format!(
                "Google OAuth token response used unsupported token_type {token_type:?}"
            )),
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
    StorageError::Retryable {
        provider: credential.provider_id.clone(),
        message: format!("Google OAuth token refresh request failed: {source}"),
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
        extract::State,
        http::{HeaderMap, header::CONTENT_TYPE},
        response::{IntoResponse, Response},
        routing::post,
    };
    use reqwest::StatusCode;

    use super::{
        GOOGLE_OAUTH_TOKEN_URL, GoogleDriveCredential, GoogleDriveCredentialLoader,
        GoogleDriveTokenRefresher,
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
}
