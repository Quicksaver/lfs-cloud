//! GitHub OAuth helpers for repository-provider login.
//!
//! This module owns the browser-facing authorization URL construction used at
//! the start of the GitHub login flow. Later callback and token-exchange code
//! should validate the returned CSRF state before accepting an OAuth code.

use std::fmt;

use oauth2::{
    AuthUrl, ClientId, CsrfToken, RedirectUrl, Scope, basic::BasicClient,
    url::ParseError as UrlParseError,
};
use reqwest::{
    Client,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
};
use serde::Deserialize;
use url::Url;

use crate::{
    GitHubProviderConfig, RepositoryProviderError, SanitizedMessage, ServerError, ServerResult,
};

const MAX_OAUTH_SENSITIVE_VALUE_LEN: usize = 1024;

/// GitHub's OAuth authorization endpoint for the initial GitHub.com provider.
///
/// This browser-facing OAuth URL is intentionally not derived from
/// [`GitHubProviderConfig::api_url`], which is the REST API base URL.
pub const GITHUB_OAUTH_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";

/// GitHub's OAuth token endpoint for authorization-code exchange.
///
/// Like the authorization URL, this browser OAuth endpoint is intentionally not
/// derived from [`GitHubProviderConfig::api_url`], which is the REST API base
/// URL used after authentication.
pub const GITHUB_OAUTH_TOKEN_URL: &str = "https://github.com/login/oauth/access_token";

/// Callback route path GitHub redirects to after browser OAuth login.
///
/// The full redirect URL is formed by combining the configured server public
/// URL with this path.
pub const GITHUB_OAUTH_CALLBACK_PATH: &str = "/auth/github/callback";

/// Default GitHub OAuth scopes for the initial login URL.
///
/// `read:user` lets the server identify the authenticated GitHub account
/// without putting the broader repository scope decision into this URL helper.
pub const DEFAULT_GITHUB_OAUTH_SCOPES: &[&str] = &["read:user"];

/// GitHub OAuth access token returned after exchanging an authorization code.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubOAuthAccessToken(String);

impl GitHubOAuthAccessToken {
    /// Restores an OAuth access token secret returned by GitHub.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the token is blank, padded, too long, or
    /// contains whitespace/control characters.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::GitHubOAuthAccessToken;
    ///
    /// let token = GitHubOAuthAccessToken::from_secret("gho_example")?;
    ///
    /// assert_eq!(token.as_str(), "gho_example");
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn from_secret(secret: impl Into<String>) -> ServerResult<Self> {
        validate_sensitive_oauth_value(secret.into(), "github oauth access token").map(Self)
    }

    /// Returns the raw access token for GitHub API requests.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GitHubOAuthAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHubOAuthAccessToken(<redacted>)")
    }
}

/// Successful GitHub OAuth token-exchange response.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubOAuthToken {
    /// GitHub OAuth access token used only by server-side provider calls.
    pub access_token: GitHubOAuthAccessToken,
    /// OAuth token type returned by GitHub, usually `bearer`.
    pub token_type: String,
    /// Scopes GitHub granted to the access token.
    pub scopes: Vec<String>,
}

impl fmt::Debug for GitHubOAuthToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubOAuthToken")
            .field("access_token", &self.access_token)
            .field("token_type", &self.token_type)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// HTTP client for exchanging validated GitHub OAuth callbacks for tokens.
#[derive(Clone, Debug)]
pub struct GitHubOAuthTokenExchanger {
    client: Client,
    token_url: Url,
}

impl GitHubOAuthTokenExchanger {
    /// Creates a token exchanger that talks to GitHub.com.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if the built-in GitHub token URL is invalid.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::GitHubOAuthTokenExchanger;
    ///
    /// let exchanger = GitHubOAuthTokenExchanger::new()?;
    ///
    /// assert!(format!("{exchanger:?}").contains("GitHubOAuthTokenExchanger"));
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn new() -> ServerResult<Self> {
        Self::with_token_url(GITHUB_OAUTH_TOKEN_URL)
    }

    /// Creates a token exchanger with an explicit token endpoint.
    ///
    /// This is primarily useful for mocked GitHub API tests.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when `token_url` is not a valid absolute URL.
    pub fn with_token_url(token_url: impl AsRef<str>) -> ServerResult<Self> {
        let token_url = Url::parse(token_url.as_ref())
            .map_err(|source| invalid_oauth_url("github oauth token endpoint", source))?;

        Ok(Self {
            client: Client::new(),
            token_url,
        })
    }

    /// Exchanges a validated GitHub OAuth callback code for an access token.
    ///
    /// `redirect_url` must match the redirect URL used to create the
    /// authorization request, binding the code exchange to the callback route.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the redirect URL is invalid, the request to
    /// GitHub fails, GitHub rejects the code, or the token response is malformed.
    pub async fn exchange_code(
        &self,
        provider: &GitHubProviderConfig,
        callback: &GitHubOAuthCallback,
        redirect_url: impl AsRef<str>,
    ) -> ServerResult<GitHubOAuthToken> {
        let redirect_url = RedirectUrl::new(redirect_url.as_ref().to_owned())
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        let request_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", provider.oauth_client_id.as_str())
            .append_pair("client_secret", provider.oauth_client_secret.as_str())
            .append_pair("code", callback.code.as_str())
            .append_pair("redirect_uri", redirect_url.as_str())
            .finish();

        let response = self
            .client
            .post(self.token_url.clone())
            .header(ACCEPT, HeaderValue::from_static("application/json"))
            .header(
                CONTENT_TYPE,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            )
            .body(request_body)
            .send()
            .await
            .map_err(|source| token_exchange_upstream_error(provider, None, source))?;
        let status = response.status();

        if !status.is_success() {
            let error = response
                .json::<GitHubOAuthTokenErrorResponse>()
                .await
                .map_err(|source| {
                    token_exchange_upstream_error(provider, Some(status.as_u16()), source)
                })?;

            return Err(ServerError::Unauthorized {
                reason: format!(
                    "github oauth token exchange failed: {}",
                    error.to_sanitized_reason()
                ),
            });
        }

        let response = response
            .json::<GitHubOAuthTokenResponse>()
            .await
            .map_err(|source| {
                token_exchange_upstream_error(provider, Some(status.as_u16()), source)
            })?;

        GitHubOAuthToken::try_from_response(provider, response)
    }
}

/// Browser URL plus CSRF state for a GitHub OAuth authorization attempt.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubOAuthAuthorization {
    /// URL the user should open in a browser to start GitHub OAuth login.
    pub authorization_url: Url,
    /// CSRF state that must match the callback's `state` query parameter.
    pub csrf_state: GitHubOAuthState,
}

impl GitHubOAuthAuthorization {
    /// Creates a GitHub OAuth authorization URL using default LFS Cloud scopes.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the configured redirect URL is invalid.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{GitHubOAuthAuthorization, GitHubProviderConfig};
    ///
    /// let provider = GitHubProviderConfig {
    ///     id: "github-main".to_owned(),
    ///     api_url: "https://api.github.com".to_owned(),
    ///     oauth_client_id: "client-id".to_owned(),
    ///     oauth_client_secret: "client-secret".to_owned(),
    /// };
    ///
    /// let authorization = GitHubOAuthAuthorization::new(
    ///     &provider,
    ///     "http://127.0.0.1:8080/auth/github/callback",
    /// )?;
    ///
    /// assert_eq!(authorization.authorization_url.host_str(), Some("github.com"));
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn new(
        provider: &GitHubProviderConfig,
        redirect_url: impl Into<String>,
    ) -> ServerResult<Self> {
        Self::with_scopes(
            provider,
            redirect_url,
            DEFAULT_GITHUB_OAUTH_SCOPES.iter().copied(),
        )
    }

    /// Creates a GitHub OAuth authorization URL using explicit scopes.
    ///
    /// The generated URL includes a fresh CSRF state. Callers must persist that
    /// state and compare it with the callback before exchanging the code.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the configured redirect URL is invalid.
    pub fn with_scopes<S>(
        provider: &GitHubProviderConfig,
        redirect_url: impl Into<String>,
        scopes: impl IntoIterator<Item = S>,
    ) -> ServerResult<Self>
    where
        S: AsRef<str>,
    {
        Self::with_state(provider, redirect_url, scopes, CsrfToken::new_random)
    }

    fn with_state<S>(
        provider: &GitHubProviderConfig,
        redirect_url: impl Into<String>,
        scopes: impl IntoIterator<Item = S>,
        state_fn: impl FnOnce() -> CsrfToken,
    ) -> ServerResult<Self>
    where
        S: AsRef<str>,
    {
        let redirect_url = RedirectUrl::new(redirect_url.into())
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        let auth_url = AuthUrl::new(GITHUB_OAUTH_AUTHORIZE_URL.to_owned())
            .map_err(|source| invalid_oauth_url("github oauth authorization endpoint", source))?;
        let client = BasicClient::new(ClientId::new(provider.oauth_client_id.clone()))
            .set_auth_uri(auth_url)
            .set_redirect_uri(redirect_url);
        let mut request = client.authorize_url(state_fn);
        for scope in scopes {
            let scope = scope.as_ref().trim();
            if scope.is_empty() {
                return Err(ServerError::InvalidConfiguration {
                    message: "github oauth scope must not be blank".to_owned(),
                });
            }
            request = request.add_scope(Scope::new(scope.to_owned()));
        }
        let (authorization_url, csrf_state) = request.url();

        Ok(Self {
            authorization_url,
            csrf_state: GitHubOAuthState::from(csrf_state),
        })
    }
}

impl fmt::Debug for GitHubOAuthAuthorization {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubOAuthAuthorization")
            .field("authorization_url", &"<redacted OAuth authorization URL>")
            .field("csrf_state", &self.csrf_state)
            .finish()
    }
}

/// CSRF state generated for a GitHub OAuth authorization attempt.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubOAuthState(String);

impl GitHubOAuthState {
    /// Restores a previously persisted OAuth CSRF state secret.
    ///
    /// The callback validator compares this value with the `state` query
    /// parameter before allowing token exchange.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the state is blank, padded, too long, or
    /// contains whitespace/control characters.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::GitHubOAuthState;
    ///
    /// let state = GitHubOAuthState::from_secret("csrf-state")?;
    ///
    /// assert_eq!(state.as_str(), "csrf-state");
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn from_secret(secret: impl Into<String>) -> ServerResult<Self> {
        validate_sensitive_oauth_value(secret.into(), "github oauth state").map(Self)
    }

    /// Returns the state value that must match the OAuth callback.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GitHubOAuthState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHubOAuthState(<redacted>)")
    }
}

impl From<CsrfToken> for GitHubOAuthState {
    fn from(value: CsrfToken) -> Self {
        Self(value.secret().clone())
    }
}

/// Authorization code returned by the GitHub OAuth callback.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubOAuthCode(String);

impl GitHubOAuthCode {
    /// Returns the raw OAuth code to pass to token exchange.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GitHubOAuthCode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHubOAuthCode(<redacted>)")
    }
}

/// Query parameters accepted by the GitHub OAuth callback route.
///
/// GitHub returns either `code` and `state` for a successful authorization, or
/// `error` fields when the user or provider denies the login attempt.
#[derive(Clone, Deserialize, Eq, PartialEq)]
pub struct GitHubOAuthCallbackQuery {
    /// OAuth authorization code returned by GitHub.
    #[serde(default)]
    code: Option<String>,
    /// CSRF state returned by GitHub.
    #[serde(default)]
    state: Option<String>,
    /// OAuth error code returned by GitHub, such as `access_denied`.
    #[serde(default)]
    error: Option<String>,
    /// Optional human-readable GitHub OAuth error description.
    #[serde(default)]
    error_description: Option<String>,
    /// Optional GitHub documentation URL for the OAuth error.
    #[serde(default)]
    error_uri: Option<String>,
}

impl GitHubOAuthCallbackQuery {
    /// Creates a successful callback query from an authorization code and state.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::GitHubOAuthCallbackQuery;
    ///
    /// let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", "csrf-state");
    ///
    /// let rendered = format!("{query:?}");
    /// assert!(!rendered.contains("oauth-code"));
    /// assert!(!rendered.contains("csrf-state"));
    /// ```
    #[must_use]
    pub fn authorization_code(code: impl Into<String>, state: impl Into<String>) -> Self {
        Self {
            code: Some(code.into()),
            state: Some(state.into()),
            error: None,
            error_description: None,
            error_uri: None,
        }
    }
}

impl fmt::Debug for GitHubOAuthCallbackQuery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubOAuthCallbackQuery")
            .field("code", &self.code.as_ref().map(|_| "<redacted>"))
            .field("state", &self.state.as_ref().map(|_| "<redacted>"))
            .field("error", &self.error)
            .field("error_description", &self.error_description)
            .field("error_uri", &self.error_uri)
            .finish()
    }
}

/// Validated GitHub OAuth callback ready for code-to-token exchange.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubOAuthCallback {
    /// Authorization code returned by GitHub.
    pub code: GitHubOAuthCode,
    /// CSRF state that matched the stored authorization attempt.
    pub state: GitHubOAuthState,
}

impl GitHubOAuthCallback {
    /// Validates a GitHub OAuth callback query against the expected CSRF state.
    ///
    /// The returned callback contains the authorization code only after the
    /// state matches. Callers should then exchange the code for a GitHub token.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when GitHub reports an OAuth error, required
    /// callback fields are missing or blank, or the CSRF state does not match.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{
    ///     GitHubOAuthCallback, GitHubOAuthCallbackQuery, GitHubOAuthState,
    /// };
    ///
    /// let expected_state = GitHubOAuthState::from_secret("csrf-state")?;
    /// let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", "csrf-state");
    ///
    /// let callback = GitHubOAuthCallback::validate(query, &expected_state)?;
    ///
    /// assert_eq!(callback.code.as_str(), "oauth-code");
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn validate(
        query: GitHubOAuthCallbackQuery,
        expected_state: &GitHubOAuthState,
    ) -> ServerResult<Self> {
        let state = required_callback_param(query.state, "state")?;
        if !constant_time_str_eq(&state, expected_state.as_str()) {
            return Err(ServerError::Unauthorized {
                reason: "github oauth csrf state mismatch".to_owned(),
            });
        }

        if let Some(error) = query.error {
            let error = sanitize_oauth_diagnostic_value(&validate_required_callback_error(error)?);
            let description = query
                .error_description
                .as_deref()
                .map(sanitize_oauth_diagnostic_value)
                .filter(|value| !value.is_empty())
                .map(|value| format!(": {value}"))
                .unwrap_or_default();

            return Err(ServerError::Unauthorized {
                reason: format!("github oauth callback failed with {error}{description}"),
            });
        }

        let code = required_callback_param(query.code, "code")?;

        Ok(Self {
            code: GitHubOAuthCode(code),
            state: GitHubOAuthState(state),
        })
    }
}

impl fmt::Debug for GitHubOAuthCallback {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubOAuthCallback")
            .field("code", &"<redacted>")
            .field("state", &"<redacted>")
            .finish()
    }
}

/// Creates a GitHub OAuth authorization URL using default LFS Cloud scopes.
///
/// This is a compatibility wrapper for callers that used the original free
/// function API. New code may call [`GitHubOAuthAuthorization::new`] directly.
///
/// # Errors
///
/// Returns [`ServerError`] when the configured redirect URL is invalid.
///
/// # Examples
///
/// ```
/// use lfs_cloud::{GitHubProviderConfig, github_oauth_authorization_url};
///
/// let provider = GitHubProviderConfig {
///     id: "github-main".to_owned(),
///     api_url: "https://api.github.com".to_owned(),
///     oauth_client_id: "client-id".to_owned(),
///     oauth_client_secret: "client-secret".to_owned(),
/// };
///
/// let authorization = github_oauth_authorization_url(
///     &provider,
///     "http://127.0.0.1:8080/auth/github/callback",
/// )?;
///
/// assert_eq!(authorization.authorization_url.host_str(), Some("github.com"));
/// # Ok::<(), lfs_cloud::ServerError>(())
/// ```
pub fn github_oauth_authorization_url(
    provider: &GitHubProviderConfig,
    redirect_url: impl Into<String>,
) -> ServerResult<GitHubOAuthAuthorization> {
    GitHubOAuthAuthorization::new(provider, redirect_url)
}

/// Exchanges a validated GitHub OAuth callback code for an access token.
///
/// This convenience wrapper uses the default GitHub.com token endpoint.
///
/// # Errors
///
/// Returns [`ServerError`] when the token exchange cannot complete.
pub async fn exchange_github_oauth_code(
    provider: &GitHubProviderConfig,
    callback: &GitHubOAuthCallback,
    redirect_url: impl AsRef<str>,
) -> ServerResult<GitHubOAuthToken> {
    GitHubOAuthTokenExchanger::new()?
        .exchange_code(provider, callback, redirect_url)
        .await
}

#[derive(Debug, Deserialize)]
struct GitHubOAuthTokenResponse {
    access_token: Option<String>,
    token_type: Option<String>,
    scope: Option<String>,
}

impl GitHubOAuthToken {
    fn try_from_response(
        provider: &GitHubProviderConfig,
        response: GitHubOAuthTokenResponse,
    ) -> ServerResult<Self> {
        let access_token = response
            .access_token
            .ok_or_else(|| malformed_token_response_error(provider, "missing access_token"))?;
        let token_type = response
            .token_type
            .ok_or_else(|| malformed_token_response_error(provider, "missing token_type"))?;

        Ok(Self {
            access_token: GitHubOAuthAccessToken::from_secret(access_token)
                .map_err(|_| malformed_token_response_error(provider, "invalid access_token"))?,
            token_type: validate_token_type(token_type)
                .map_err(|_| malformed_token_response_error(provider, "invalid token_type"))?,
            scopes: parse_scope_list(response.scope.as_deref().unwrap_or_default()),
        })
    }
}

#[derive(Debug, Deserialize)]
struct GitHubOAuthTokenErrorResponse {
    error: Option<String>,
    error_description: Option<String>,
}

impl GitHubOAuthTokenErrorResponse {
    fn to_sanitized_reason(&self) -> String {
        let error = self
            .error
            .as_deref()
            .map(sanitize_oauth_diagnostic_value)
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "unknown_error".to_owned());
        let description = self
            .error_description
            .as_deref()
            .map(sanitize_oauth_diagnostic_value)
            .filter(|value| !value.is_empty())
            .map(|value| format!(": {value}"))
            .unwrap_or_default();

        format!("{error}{description}")
    }
}

fn validate_token_type(token_type: String) -> ServerResult<String> {
    validate_sensitive_oauth_value(token_type, "github oauth token type")
}

fn parse_scope_list(scopes: &str) -> Vec<String> {
    scopes
        .split([' ', ','])
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn token_exchange_upstream_error(
    provider: &GitHubProviderConfig,
    status: Option<u16>,
    source: reqwest::Error,
) -> ServerError {
    repository_provider_upstream_error(provider, status, &source.to_string())
}

fn malformed_token_response_error(provider: &GitHubProviderConfig, message: &str) -> ServerError {
    repository_provider_upstream_error(
        provider,
        None,
        &format!("malformed github oauth token response: {message}"),
    )
}

fn repository_provider_upstream_error(
    provider: &GitHubProviderConfig,
    status: Option<u16>,
    message: &str,
) -> ServerError {
    ServerError::RepositoryProvider {
        source: RepositoryProviderError::Upstream {
            provider: provider.id.clone(),
            status,
            message: SanitizedMessage::new(sanitize_oauth_diagnostic_value(message)),
        },
    }
}

fn invalid_oauth_url(path: &str, source: UrlParseError) -> ServerError {
    ServerError::InvalidConfiguration {
        message: format!("{path} must be a valid absolute URL: {source}"),
    }
}

fn required_callback_param(value: Option<String>, name: &str) -> ServerResult<String> {
    let value = value.ok_or_else(|| ServerError::InvalidRequest {
        message: format!("github oauth callback {name} is required"),
    })?;
    validate_sensitive_oauth_value(value, &format!("github oauth callback {name}"))
}

fn validate_sensitive_oauth_value(value: String, label: &str) -> ServerResult<String> {
    if value.len() > MAX_OAUTH_SENSITIVE_VALUE_LEN {
        return Err(ServerError::InvalidRequest {
            message: format!("{label} must not exceed {MAX_OAUTH_SENSITIVE_VALUE_LEN} bytes"),
        });
    }

    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() != value.len() {
        return Err(ServerError::InvalidRequest {
            message: format!("{label} must not be blank or padded"),
        });
    }

    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ServerError::InvalidRequest {
            message: format!("{label} must not contain whitespace or control characters"),
        });
    }

    Ok(value)
}

fn validate_required_callback_error(value: String) -> ServerResult<String> {
    if value.len() > MAX_OAUTH_SENSITIVE_VALUE_LEN {
        return Err(ServerError::InvalidRequest {
            message: format!(
                "github oauth callback error must not exceed {MAX_OAUTH_SENSITIVE_VALUE_LEN} bytes"
            ),
        });
    }

    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() != value.len() {
        return Err(ServerError::InvalidRequest {
            message: "github oauth callback error must not be blank or padded".to_owned(),
        });
    }

    Ok(value)
}

fn constant_time_str_eq(candidate: &str, expected: &str) -> bool {
    let candidate = candidate.as_bytes();
    let expected = expected.as_bytes();
    let mut diff = candidate.len() ^ expected.len();

    for index in 0..MAX_OAUTH_SENSITIVE_VALUE_LEN {
        let candidate_byte = candidate.get(index).copied().unwrap_or_default();
        let expected_byte = expected.get(index).copied().unwrap_or_default();
        diff |= usize::from(candidate_byte ^ expected_byte);
    }

    diff == 0
}

fn sanitize_oauth_diagnostic_value(value: &str) -> String {
    const MAX_LEN: usize = 200;

    let mut sanitized: String = value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .take(MAX_LEN)
        .collect();
    let trimmed_end_len = sanitized.trim_end().len();
    sanitized.truncate(trimmed_end_len);
    let trimmed_start_len = sanitized.len() - sanitized.trim_start().len();
    if trimmed_start_len > 0 {
        sanitized.drain(..trimmed_start_len);
    }
    sanitized
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use axum::{
        Router,
        extract::State,
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::post,
    };
    use oauth2::CsrfToken;

    use super::{
        DEFAULT_GITHUB_OAUTH_SCOPES, GITHUB_OAUTH_AUTHORIZE_URL, GITHUB_OAUTH_CALLBACK_PATH,
        GITHUB_OAUTH_TOKEN_URL, GitHubOAuthAuthorization, GitHubOAuthCallback,
        GitHubOAuthCallbackQuery, GitHubOAuthState, GitHubOAuthTokenExchanger,
        exchange_github_oauth_code, github_oauth_authorization_url,
    };
    use crate::{GitHubProviderConfig, ServerError};

    const REDIRECT_URL: &str = "http://127.0.0.1:8080/auth/github/callback";

    fn provider_config() -> GitHubProviderConfig {
        GitHubProviderConfig {
            id: "github-main".to_owned(),
            api_url: "https://api.github.com".to_owned(),
            oauth_client_id: "client-id".to_owned(),
            oauth_client_secret: "client-secret".to_owned(),
        }
    }

    #[test]
    fn authorization_url_targets_github_with_required_oauth_parameters() {
        let authorization = GitHubOAuthAuthorization::with_state(
            &provider_config(),
            REDIRECT_URL,
            DEFAULT_GITHUB_OAUTH_SCOPES.iter().copied(),
            || CsrfToken::new("csrf-state".to_owned()),
        )
        .expect("authorization URL should build");

        assert_eq!(authorization.authorization_url.scheme(), "https");
        assert_eq!(
            authorization.authorization_url.host_str(),
            Some("github.com")
        );
        assert_eq!(
            authorization.authorization_url.path(),
            "/login/oauth/authorize"
        );
        let query = query_pairs(&authorization);
        assert_eq!(query.get("response_type").map(String::as_str), Some("code"));
        assert_eq!(
            query.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(
            query.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URL)
        );
        assert_eq!(query.get("scope").map(String::as_str), Some("read:user"));
        assert_eq!(query.get("state").map(String::as_str), Some("csrf-state"));
        assert_eq!(authorization.csrf_state.as_str(), "csrf-state");
    }

    #[test]
    fn csrf_state_matches_url_query_parameter() {
        let authorization = GitHubOAuthAuthorization::new(&provider_config(), REDIRECT_URL)
            .expect("authorization URL should build");
        let query = query_pairs(&authorization);

        assert_eq!(
            query.get("state").map(String::as_str),
            Some(authorization.csrf_state.as_str())
        );
        assert!(!authorization.csrf_state.as_str().is_empty());
    }

    #[test]
    fn generated_csrf_states_are_fresh() {
        let first = GitHubOAuthAuthorization::new(&provider_config(), REDIRECT_URL)
            .expect("first authorization URL should build");
        let second = GitHubOAuthAuthorization::new(&provider_config(), REDIRECT_URL)
            .expect("second authorization URL should build");

        assert_ne!(first.csrf_state, second.csrf_state);
    }

    #[test]
    fn csrf_state_from_secret_validates_stored_secret() {
        for invalid in ["", "  ", " csrf-state", "csrf-state ", "csrf\nstate"] {
            let error = GitHubOAuthState::from_secret(invalid).unwrap_err();
            assert!(matches!(error, ServerError::InvalidRequest { .. }));
        }

        let oversized = "x".repeat(super::MAX_OAUTH_SENSITIVE_VALUE_LEN + 1);
        let error = GitHubOAuthState::from_secret(oversized).unwrap_err();
        assert!(matches!(error, ServerError::InvalidRequest { .. }));

        let state = GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");
        assert_eq!(state.as_str(), "csrf-state");
    }

    #[test]
    fn free_function_oauth_helper_remains_available() {
        let authorization = github_oauth_authorization_url(&provider_config(), REDIRECT_URL)
            .expect("authorization URL should build");

        assert_eq!(
            authorization.authorization_url.host_str(),
            Some("github.com")
        );
        assert_eq!(
            query_pairs(&authorization).get("scope").map(String::as_str),
            Some("read:user")
        );
    }

    #[tokio::test]
    async fn token_exchange_posts_validated_callback_to_github_token_endpoint() {
        let (token_url, token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user repo"}"#,
        )
        .await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let token = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect("token exchange should succeed");

        assert_eq!(token.access_token.as_str(), "gho_token");
        assert_eq!(token.token_type, "bearer");
        assert_eq!(token.scopes, vec!["read:user", "repo"]);
        let requests = token_server
            .requests
            .lock()
            .expect("request lock should not poison");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.accept.as_deref(), Some("application/json"));
        assert_eq!(
            request.body.get("client_id").map(String::as_str),
            Some("client-id")
        );
        assert_eq!(
            request.body.get("client_secret").map(String::as_str),
            Some("client-secret")
        );
        assert_eq!(
            request.body.get("code").map(String::as_str),
            Some("oauth-code")
        );
        assert_eq!(
            request.body.get("redirect_uri").map(String::as_str),
            Some(REDIRECT_URL)
        );
    }

    #[tokio::test]
    async fn token_exchange_wrapper_uses_default_github_endpoint() {
        assert_eq!(
            url::Url::parse(GITHUB_OAUTH_TOKEN_URL)
                .expect("constant should parse")
                .host_str(),
            Some("github.com")
        );

        let callback = validated_callback("oauth-code", "csrf-state");
        let error = exchange_github_oauth_code(&provider_config(), &callback, "not a url")
            .await
            .expect_err("invalid redirect URL should fail before network I/O");

        assert!(error.to_string().contains("github oauth redirect_url"));
    }

    #[tokio::test]
    async fn token_exchange_maps_github_oauth_error_without_echoing_secrets() {
        let (token_url, _token_server) = token_server(
            StatusCode::BAD_REQUEST,
            r#"{"error":"bad_verification_code","error_description":"The code passed is incorrect or expired."}"#,
        )
        .await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("GitHub OAuth denial should fail");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::Unauthorized { .. }));
        assert!(rendered.contains("bad_verification_code"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("oauth-code"));
        assert!(!rendered.contains("csrf-state"));
    }

    #[tokio::test]
    async fn token_exchange_rejects_malformed_success_response() {
        let (token_url, _token_server) = token_server(
            StatusCode::OK,
            r#"{"token_type":"bearer","scope":"read:user"}"#,
        )
        .await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("missing access_token should fail");

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(error.to_string().contains("access_token"));
        assert!(!error.to_string().contains("oauth-code"));
    }

    #[test]
    fn token_debug_output_redacts_access_token() {
        let token =
            super::GitHubOAuthAccessToken::from_secret("gho_secret").expect("token should parse");
        let response = super::GitHubOAuthToken {
            access_token: token,
            token_type: "bearer".to_owned(),
            scopes: vec!["read:user".to_owned()],
        };

        let rendered = format!("{response:?}");

        assert!(rendered.contains("GitHubOAuthToken"));
        assert!(!rendered.contains("gho_secret"));
    }

    #[test]
    fn authorization_url_does_not_expose_client_secret() {
        let authorization = GitHubOAuthAuthorization::new(&provider_config(), REDIRECT_URL)
            .expect("authorization URL should build");
        let rendered = authorization.authorization_url.as_str();

        assert!(!rendered.contains("client-secret"));
        assert_eq!(query_pairs(&authorization).get("client_secret"), None);
    }

    #[test]
    fn custom_scopes_are_encoded_as_space_separated_github_scope() {
        let scopes = vec![" read:user ".to_owned(), "repo".to_owned()];
        let authorization =
            GitHubOAuthAuthorization::with_state(&provider_config(), REDIRECT_URL, scopes, || {
                CsrfToken::new("csrf-state".to_owned())
            })
            .expect("authorization URL should build");

        assert_eq!(
            query_pairs(&authorization).get("scope").map(String::as_str),
            Some("read:user repo")
        );
    }

    #[test]
    fn blank_custom_scopes_are_rejected() {
        let error = GitHubOAuthAuthorization::with_state(
            &provider_config(),
            REDIRECT_URL,
            ["read:user", "  "],
            || CsrfToken::new("csrf-state".to_owned()),
        )
        .unwrap_err();

        assert!(error.to_string().contains("github oauth scope"));
    }

    #[test]
    fn invalid_redirect_url_is_rejected() {
        let error = GitHubOAuthAuthorization::new(&provider_config(), "not a url").unwrap_err();

        assert!(error.to_string().contains("github oauth redirect_url"));
    }

    #[test]
    fn github_authorize_endpoint_constant_is_valid() {
        assert_eq!(
            url::Url::parse(GITHUB_OAUTH_AUTHORIZE_URL)
                .expect("constant should parse")
                .host_str(),
            Some("github.com")
        );
    }

    #[test]
    fn debug_output_redacts_csrf_state() {
        let authorization = GitHubOAuthAuthorization::with_state(
            &provider_config(),
            REDIRECT_URL,
            ["read:user"],
            || CsrfToken::new("csrf-state".to_owned()),
        )
        .expect("authorization URL should build");

        let rendered = format!("{authorization:?}");

        assert!(rendered.contains("GitHubOAuthAuthorization"));
        assert!(!rendered.contains("csrf-state"));
    }

    #[test]
    fn callback_path_matches_authorization_redirect_examples() {
        assert_eq!(GITHUB_OAUTH_CALLBACK_PATH, "/auth/github/callback");
        assert!(REDIRECT_URL.ends_with(GITHUB_OAUTH_CALLBACK_PATH));
    }

    #[test]
    fn callback_validation_accepts_matching_state_and_code() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");
        let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", "csrf-state");

        let callback =
            GitHubOAuthCallback::validate(query, &expected_state).expect("callback should match");

        assert_eq!(callback.code.as_str(), "oauth-code");
        assert_eq!(callback.state.as_str(), "csrf-state");
    }

    #[test]
    fn callback_validation_rejects_state_mismatch_without_echoing_values() {
        let expected_state =
            GitHubOAuthState::from_secret("expected-state").expect("state should be valid");
        let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", "returned-state");

        let error = GitHubOAuthCallback::validate(query, &expected_state).unwrap_err();

        assert!(matches!(error, ServerError::Unauthorized { .. }));
        let rendered = error.to_string();
        assert!(rendered.contains("csrf state mismatch"));
        assert!(!rendered.contains("expected-state"));
        assert!(!rendered.contains("returned-state"));
        assert!(!rendered.contains("oauth-code"));
    }

    #[test]
    fn callback_validation_rejects_missing_or_blank_required_fields() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");

        for query in [
            GitHubOAuthCallbackQuery {
                code: None,
                state: Some("csrf-state".to_owned()),
                error: None,
                error_description: None,
                error_uri: None,
            },
            GitHubOAuthCallbackQuery::authorization_code("  ", "csrf-state"),
            GitHubOAuthCallbackQuery {
                code: Some("oauth-code".to_owned()),
                state: None,
                error: None,
                error_description: None,
                error_uri: None,
            },
            GitHubOAuthCallbackQuery::authorization_code("oauth-code", "  "),
        ] {
            let error = GitHubOAuthCallback::validate(query, &expected_state).unwrap_err();
            assert!(matches!(error, ServerError::InvalidRequest { .. }));
        }
    }

    #[test]
    fn callback_validation_rejects_oversized_required_fields() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");
        let oversized = "x".repeat(super::MAX_OAUTH_SENSITIVE_VALUE_LEN + 1);

        for query in [
            GitHubOAuthCallbackQuery::authorization_code(oversized.clone(), "csrf-state"),
            GitHubOAuthCallbackQuery::authorization_code("oauth-code", oversized),
        ] {
            let error = GitHubOAuthCallback::validate(query, &expected_state).unwrap_err();
            assert!(matches!(error, ServerError::InvalidRequest { .. }));
        }
    }

    #[test]
    fn callback_validation_converts_github_error_to_unauthorized() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");
        let query = GitHubOAuthCallbackQuery {
            code: None,
            state: Some("csrf-state".to_owned()),
            error: Some("access_denied".to_owned()),
            error_description: Some("User denied\naccess".to_owned()),
            error_uri: Some("https://docs.github.com".to_owned()),
        };

        let error = GitHubOAuthCallback::validate(query, &expected_state).unwrap_err();

        assert!(matches!(error, ServerError::Unauthorized { .. }));
        let rendered = error.to_string();
        assert!(rendered.contains("access_denied"));
        assert!(rendered.contains("User denied access"));
    }

    #[test]
    fn callback_validation_sanitizes_github_error_code() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");
        let query = GitHubOAuthCallbackQuery {
            code: None,
            state: Some("csrf-state".to_owned()),
            error: Some(format!("access_denied\n{}", "x".repeat(240))),
            error_description: None,
            error_uri: None,
        };

        let error = GitHubOAuthCallback::validate(query, &expected_state).unwrap_err();

        assert!(matches!(error, ServerError::Unauthorized { .. }));
        let rendered = error.to_string();
        assert!(rendered.contains("access_denied "));
        assert!(!rendered.contains('\n'));
        assert!(!rendered.contains(&"x".repeat(201)));
    }

    #[test]
    fn callback_validation_rejects_github_error_without_matching_state() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");

        let missing_state = GitHubOAuthCallbackQuery {
            code: None,
            state: None,
            error: Some("access_denied".to_owned()),
            error_description: None,
            error_uri: None,
        };
        let missing_error = GitHubOAuthCallback::validate(missing_state, &expected_state)
            .expect_err("missing state should fail before provider error handling");
        assert!(matches!(missing_error, ServerError::InvalidRequest { .. }));
        assert!(!missing_error.to_string().contains("access_denied"));

        let mismatched_state = GitHubOAuthCallbackQuery {
            code: None,
            state: Some("attacker-state".to_owned()),
            error: Some("access_denied".to_owned()),
            error_description: None,
            error_uri: None,
        };
        let mismatch_error = GitHubOAuthCallback::validate(mismatched_state, &expected_state)
            .expect_err("mismatched state should fail before provider error handling");
        assert!(matches!(mismatch_error, ServerError::Unauthorized { .. }));
        let rendered = mismatch_error.to_string();
        assert!(rendered.contains("csrf state mismatch"));
        assert!(!rendered.contains("access_denied"));
        assert!(!rendered.contains("attacker-state"));
    }

    #[test]
    fn callback_debug_output_redacts_code_and_state() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");
        let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", "csrf-state");

        let callback =
            GitHubOAuthCallback::validate(query.clone(), &expected_state).expect("valid callback");

        let query_debug = format!("{query:?}");
        let callback_debug = format!("{callback:?}");

        assert!(!query_debug.contains("oauth-code"));
        assert!(!query_debug.contains("csrf-state"));
        assert!(!callback_debug.contains("oauth-code"));
        assert!(!callback_debug.contains("csrf-state"));
    }

    #[derive(Debug, Default)]
    struct TokenServerState {
        status: StatusCode,
        body: &'static str,
        requests: Mutex<Vec<CapturedTokenRequest>>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedTokenRequest {
        accept: Option<String>,
        body: BTreeMap<String, String>,
    }

    async fn token_server(
        status: StatusCode,
        body: &'static str,
    ) -> (String, Arc<TokenServerState>) {
        let state = Arc::new(TokenServerState {
            status,
            body,
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/login/oauth/access_token", post(capture_token_request))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock token server should bind");
        let address = listener
            .local_addr()
            .expect("mock token server address should be available");

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock token server should run");
        });

        (format!("http://{address}/login/oauth/access_token"), state)
    }

    async fn capture_token_request(
        State(state): State<Arc<TokenServerState>>,
        headers: HeaderMap,
        body: String,
    ) -> impl IntoResponse {
        let accept = headers
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let body = url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();

        state
            .requests
            .lock()
            .expect("request lock should not poison")
            .push(CapturedTokenRequest { accept, body });

        (
            state.status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            state.body,
        )
    }

    fn validated_callback(code: &str, state: &str) -> GitHubOAuthCallback {
        let expected_state = GitHubOAuthState::from_secret(state).expect("state should be valid");
        let query = GitHubOAuthCallbackQuery::authorization_code(code, state);

        GitHubOAuthCallback::validate(query, &expected_state).expect("callback should validate")
    }

    fn query_pairs(authorization: &GitHubOAuthAuthorization) -> BTreeMap<String, String> {
        authorization
            .authorization_url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }
}
