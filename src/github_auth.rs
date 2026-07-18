//! GitHub OAuth helpers for repository-provider login.
//!
//! This module owns the browser-facing authorization URL construction and
//! callback handling used by the GitHub login flow. The callback route validates
//! the returned CSRF state before exchanging an OAuth code and only returns
//! non-secret identity metadata.

use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    extract::{Query, State},
    middleware,
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use oauth2::{
    AuthUrl, ClientId, CsrfToken, PkceCodeChallenge, PkceCodeVerifier, RedirectUrl, Scope,
    basic::BasicClient, url::ParseError as UrlParseError,
};
use reqwest::{
    Client, RequestBuilder, StatusCode,
    header::{ACCEPT, CONTENT_TYPE, HeaderValue},
    redirect::Policy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    GitHubProviderConfig, IssuedLfsSession, LocalLfsSessionStore, ProviderFuture,
    RepositoryAuthentication, RepositoryAuthorization, RepositoryIdentity, RepositoryPermission,
    RepositoryProvider, RepositoryProviderError, RepositoryUser, SanitizedMessage, ServerError,
    ServerResult, http_transport::uses_protected_http_transport,
};

const MAX_OAUTH_SENSITIVE_VALUE_LEN: usize = 1024;
const MAX_GITHUB_ERROR_BODY_LEN: usize = 16 * 1024;
const MAX_GITHUB_OAUTH_TOKEN_RESPONSE_BODY_LEN: usize = 16 * 1024;
const GITHUB_OAUTH_TOKEN_EXCHANGE_TIMEOUT: Duration = Duration::from_secs(30);
const GITHUB_USER_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const GITHUB_PERMISSION_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const GITHUB_OAUTH_STATE_TTL: Duration = Duration::from_secs(10 * 60);
const MAX_PENDING_GITHUB_OAUTH_STATES: usize = 1024;
const GITHUB_USER_AGENT: &str = concat!("lfs-cloud/", env!("CARGO_PKG_VERSION"));
const GITHUB_API_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_API_VERSION_HEADER: &str = "x-github-api-version";
const GITHUB_API_VERSION: &str = "2022-11-28";
const GITHUB_SSO_HEADER: &str = "x-github-sso";

static DEFAULT_GITHUB_OAUTH_HTTP_CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();
static DEFAULT_GITHUB_API_HTTP_CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();

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

/// Login redirect path that starts the GitHub OAuth browser flow.
pub const GITHUB_OAUTH_LOGIN_PATH: &str = "/auth/github/login";

/// Default GitHub OAuth scopes for the initial login URL.
///
/// `read:user` identifies the authenticated GitHub account, and `repo` lets
/// the server re-check repository permissions for private repositories.
pub const DEFAULT_GITHUB_OAUTH_SCOPES: &[&str] = &["read:user", "repo"];

/// State required by the GitHub OAuth callback route.
#[derive(Clone)]
pub struct GitHubOAuthCallbackRouteState {
    provider: GitHubProviderConfig,
    csrf_states: GitHubOAuthStateRegistry,
    redirect_url: String,
    token_exchanger: GitHubOAuthTokenExchanger,
    user_client: GitHubUserClient,
    session_store: LocalLfsSessionStore,
}

impl GitHubOAuthCallbackRouteState {
    /// Creates callback route state using default GitHub HTTP clients.
    ///
    /// `csrf_states` must contain the CSRF values generated for pending
    /// authorization URLs, and `redirect_url` must match the OAuth app callback.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{
    ///     GitHubOAuthCallbackRouteState, GitHubOAuthStateRegistry, GitHubProviderConfig,
    /// };
    ///
    /// let provider = GitHubProviderConfig {
    ///     id: "github-main".to_owned(),
    ///     api_url: "https://api.github.com".to_owned(),
    ///     oauth_client_id: "client-id".to_owned(),
    ///     oauth_client_secret: "client-secret".to_owned(),
    ///     allow_insecure_http: false,
    /// };
    /// let csrf_states = GitHubOAuthStateRegistry::new();
    ///
    /// let route_state = GitHubOAuthCallbackRouteState::new(
    ///     provider,
    ///     csrf_states,
    ///     "https://lfs.example.com/auth/github/callback",
    /// )?;
    ///
    /// assert!(format!("{route_state:?}").contains("GitHubOAuthCallbackRouteState"));
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when default HTTP clients cannot be created or
    /// the redirect URL is not a valid absolute URL.
    pub fn new(
        provider: GitHubProviderConfig,
        csrf_states: GitHubOAuthStateRegistry,
        redirect_url: impl Into<String>,
    ) -> ServerResult<Self> {
        Self::with_clients(
            provider,
            csrf_states,
            redirect_url,
            GitHubOAuthTokenExchanger::new()?,
            GitHubUserClient::new()?,
        )
    }

    /// Creates callback route state with explicit provider clients.
    ///
    /// This constructor is useful for tests and for server code that shares
    /// tuned HTTP clients across provider components.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{
    ///     GitHubOAuthCallbackRouteState, GitHubOAuthTokenExchanger,
    ///     GitHubOAuthStateRegistry, GitHubProviderConfig, GitHubUserClient,
    /// };
    ///
    /// let provider = GitHubProviderConfig {
    ///     id: "github-main".to_owned(),
    ///     api_url: "https://api.github.com".to_owned(),
    ///     oauth_client_id: "client-id".to_owned(),
    ///     oauth_client_secret: "client-secret".to_owned(),
    ///     allow_insecure_http: false,
    /// };
    /// let csrf_states = GitHubOAuthStateRegistry::new();
    /// let token_exchanger = GitHubOAuthTokenExchanger::new()?;
    /// let user_client = GitHubUserClient::new()?;
    ///
    /// let route_state = GitHubOAuthCallbackRouteState::with_clients(
    ///     provider,
    ///     csrf_states,
    ///     "https://lfs.example.com/auth/github/callback",
    ///     token_exchanger,
    ///     user_client,
    /// )?;
    ///
    /// assert!(format!("{route_state:?}").contains("GitHubOAuthCallbackRouteState"));
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when `redirect_url` is not a valid absolute URL.
    pub fn with_clients(
        provider: GitHubProviderConfig,
        csrf_states: GitHubOAuthStateRegistry,
        redirect_url: impl Into<String>,
        token_exchanger: GitHubOAuthTokenExchanger,
        user_client: GitHubUserClient,
    ) -> ServerResult<Self> {
        Self::with_clients_and_session_store(
            provider,
            csrf_states,
            redirect_url,
            token_exchanger,
            user_client,
            LocalLfsSessionStore::new(),
        )
    }

    /// Creates callback route state with explicit clients and session storage.
    ///
    /// Tests and future server wiring can inject a shared store so successful
    /// callbacks make the issued LFS Cloud token immediately verifiable by
    /// request-auth middleware.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when `redirect_url` is not a valid absolute URL.
    pub fn with_clients_and_session_store(
        provider: GitHubProviderConfig,
        csrf_states: GitHubOAuthStateRegistry,
        redirect_url: impl Into<String>,
        token_exchanger: GitHubOAuthTokenExchanger,
        user_client: GitHubUserClient,
        session_store: LocalLfsSessionStore,
    ) -> ServerResult<Self> {
        let redirect_url = redirect_url.into();
        let parsed_redirect_url = Url::parse(&redirect_url)
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        validate_github_transport_url(
            &parsed_redirect_url,
            "github oauth redirect_url",
            provider.allow_insecure_http,
        )?;
        RedirectUrl::new(redirect_url.clone())
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;

        Ok(Self {
            provider,
            csrf_states,
            redirect_url,
            token_exchanger,
            user_client,
            session_store,
        })
    }
}

impl fmt::Debug for GitHubOAuthCallbackRouteState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubOAuthCallbackRouteState")
            .field("provider_id", &self.provider.id)
            .field("csrf_states", &self.csrf_states)
            .field("redirect_url", &self.redirect_url)
            .field("token_exchanger", &"<redacted>")
            .field("user_client", &"<redacted>")
            .field("session_store", &self.session_store)
            .finish()
    }
}

/// Response returned by the GitHub OAuth callback route.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct GitHubOAuthCallbackRouteResponse {
    /// Configured repository provider that authenticated the user.
    pub provider_id: String,
    /// Authenticated GitHub login.
    pub login: String,
    /// Stable GitHub user ID, when GitHub returned one.
    pub stable_id: Option<String>,
    /// OAuth scopes granted by GitHub.
    pub granted_scopes: Vec<String>,
    /// Opaque LFS Cloud token to store for the configured Git LFS URL.
    ///
    /// This response intentionally exposes only a local LFS Cloud bearer token,
    /// never the upstream GitHub OAuth token. Do not log serialized responses.
    pub lfs_token: String,
    /// Expiration time for `lfs_token`, as seconds since the Unix epoch.
    pub lfs_token_expires_at_unix_seconds: u64,
}

#[derive(Serialize)]
struct GitHubOAuthCallbackRouteErrorBody {
    error: &'static str,
    message: &'static str,
}

impl GitHubOAuthCallbackRouteResponse {
    fn new(
        provider: &GitHubProviderConfig,
        user: RepositoryUser,
        scopes: Vec<String>,
        session: IssuedLfsSession,
    ) -> Self {
        Self {
            provider_id: provider.id.clone(),
            login: user.login,
            stable_id: user.stable_id,
            granted_scopes: scopes,
            lfs_token: session.token.as_str().to_owned(),
            lfs_token_expires_at_unix_seconds: session.metadata.expires_at_unix_seconds(),
        }
    }
}

impl fmt::Debug for GitHubOAuthCallbackRouteResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubOAuthCallbackRouteResponse")
            .field("provider_id", &self.provider_id)
            .field("login", &self.login)
            .field("stable_id", &self.stable_id)
            .field("granted_scopes", &self.granted_scopes)
            .field("lfs_token", &"<redacted>")
            .field(
                "lfs_token_expires_at_unix_seconds",
                &self.lfs_token_expires_at_unix_seconds,
            )
            .finish()
    }
}

/// Creates an Axum router for the GitHub OAuth callback endpoint.
///
/// The route is mounted at [`GITHUB_OAUTH_CALLBACK_PATH`] and performs callback
/// validation, code exchange, and authenticated-user lookup. It never returns
/// the GitHub OAuth access token in the HTTP response.
pub fn github_oauth_callback_router(state: GitHubOAuthCallbackRouteState) -> Router {
    Router::new()
        .route(GITHUB_OAUTH_CALLBACK_PATH, get(github_oauth_callback_route))
        .layer(middleware::map_response(
            protect_github_oauth_callback_response,
        ))
        .with_state(state)
}

async fn protect_github_oauth_callback_response(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        axum::http::header::CACHE_CONTROL,
        HeaderValue::from_static("no-store, private"),
    );
    headers.insert(
        axum::http::header::PRAGMA,
        HeaderValue::from_static("no-cache"),
    );
    headers.insert(
        axum::http::header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

/// Creates an Axum router for the GitHub OAuth login redirect endpoint.
///
/// The route is mounted at [`GITHUB_OAUTH_LOGIN_PATH`], generates a fresh
/// browser authorization URL, registers the matching CSRF state and PKCE
/// verifier in the shared registry, and redirects the browser to GitHub.
pub fn github_oauth_login_router(state: GitHubOAuthCallbackRouteState) -> Router {
    Router::new()
        .route(GITHUB_OAUTH_LOGIN_PATH, get(github_oauth_login_route))
        .with_state(state)
}

async fn github_oauth_login_route(
    State(state): State<GitHubOAuthCallbackRouteState>,
) -> Result<Redirect, GitHubOAuthCallbackRouteError> {
    let authorization = GitHubOAuthAuthorization::new(&state.provider, &state.redirect_url)?;
    let authorization_url = state.csrf_states.register(authorization)?;

    Ok(Redirect::temporary(authorization_url.as_str()))
}

async fn github_oauth_callback_route(
    State(state): State<GitHubOAuthCallbackRouteState>,
    Query(query): Query<GitHubOAuthCallbackQuery>,
) -> Result<Json<GitHubOAuthCallbackRouteResponse>, GitHubOAuthCallbackRouteError> {
    let callback = GitHubOAuthCallback::validate_registered(query, &state.csrf_states)?;
    let token = state
        .token_exchanger
        .exchange_code(&state.provider, &callback, &state.redirect_url)
        .await?;
    let user = state
        .user_client
        .fetch_authenticated_user(&state.provider, &token.access_token)
        .await?;
    let scopes = token.scopes;
    let session = state.session_store.issue_session_with_github_token(
        &user,
        scopes.clone(),
        token.access_token,
    )?;
    let response = GitHubOAuthCallbackRouteResponse::new(&state.provider, user, scopes, session);

    Ok(Json(response))
}

struct GitHubOAuthCallbackRouteError(ServerError);

impl From<ServerError> for GitHubOAuthCallbackRouteError {
    fn from(error: ServerError) -> Self {
        Self(error)
    }
}

impl IntoResponse for GitHubOAuthCallbackRouteError {
    fn into_response(self) -> Response {
        let (status, error, message, retry_after_seconds) = match &self.0 {
            ServerError::InvalidRequest { .. } => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "Invalid GitHub OAuth callback request.",
                None,
            ),
            ServerError::RateLimited {
                retry_after_seconds,
            } => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limited",
                "GitHub OAuth login capacity is temporarily exhausted.",
                Some(*retry_after_seconds),
            ),
            ServerError::Unauthorized { .. } => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "GitHub OAuth callback was not authorized.",
                None,
            ),
            ServerError::RepositoryProvider {
                source:
                    RepositoryProviderError::AuthenticationRequired { .. }
                    | RepositoryProviderError::PermissionDenied { .. }
                    | RepositoryProviderError::SsoRequired { .. },
            } => (
                StatusCode::UNAUTHORIZED,
                "unauthorized",
                "GitHub OAuth callback was not authorized.",
                None,
            ),
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::RepositoryNotFound { .. },
            } => (
                StatusCode::NOT_FOUND,
                "not_found",
                "GitHub repository was not found.",
                None,
            ),
            ServerError::RepositoryProvider { .. } | ServerError::Storage { .. } => (
                StatusCode::BAD_GATEWAY,
                "upstream_error",
                "GitHub OAuth callback could not be completed.",
                None,
            ),
            ServerError::ConfigRead { .. }
            | ServerError::ConfigParse { .. }
            | ServerError::InvalidConfiguration { .. }
            | ServerError::MetadataDirectoryCreate { .. }
            | ServerError::MetadataOpen { .. }
            | ServerError::MetadataConfigure { .. }
            | ServerError::MetadataSchemaTooNew { .. }
            | ServerError::MetadataMigration { .. }
            | ServerError::MetadataOperation { .. }
            | ServerError::MetadataConnectionPoisoned { .. }
            | ServerError::MetadataTaskJoin { .. }
            | ServerError::Bind { .. }
            | ServerError::LocalAddress { .. }
            | ServerError::Serve { .. }
            | ServerError::RouteNotConfigured { .. }
            | ServerError::Internal { .. } => (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal_error",
                "GitHub OAuth callback could not be completed.",
                None,
            ),
        };
        let body = GitHubOAuthCallbackRouteErrorBody { error, message };
        let mut response = (status, Json(body)).into_response();
        if let Some(retry_after_seconds) = retry_after_seconds {
            response.headers_mut().insert(
                axum::http::header::RETRY_AFTER,
                HeaderValue::from_str(&retry_after_seconds.to_string())
                    .expect("u64 retry delay must be a valid HTTP header value"),
            );
        }

        response
    }
}

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

/// HTTP client for fetching the authenticated GitHub user identity.
///
/// This client uses the OAuth access token only for server-side GitHub API
/// calls. The returned [`RepositoryUser`] is the identity that later session and
/// permission-check code can store without exposing the GitHub token to Git LFS.
#[derive(Clone, Debug)]
pub struct GitHubUserClient {
    client: Client,
}

impl GitHubUserClient {
    /// Creates a GitHub user client with the default HTTP settings.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if the default HTTP client cannot be built.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::GitHubUserClient;
    ///
    /// let client = GitHubUserClient::new()?;
    ///
    /// assert!(format!("{client:?}").contains("GitHubUserClient"));
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn new() -> ServerResult<Self> {
        Ok(Self::with_client(default_github_api_http_client()?))
    }

    /// Creates a GitHub user client from an existing [`Client`].
    ///
    /// This is useful when server code shares one tuned provider HTTP client.
    #[must_use]
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Fetches the authenticated GitHub user's identity with an OAuth token.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the configured GitHub API URL is invalid,
    /// the token is rejected, the request fails, or GitHub returns malformed
    /// identity JSON.
    pub async fn fetch_authenticated_user(
        &self,
        provider: &GitHubProviderConfig,
        token: &GitHubOAuthAccessToken,
    ) -> ServerResult<RepositoryUser> {
        let endpoint = github_user_endpoint(provider)?;
        let response = github_api_request(self.client.get(endpoint))
            .bearer_auth(token.as_str())
            .timeout(GITHUB_USER_FETCH_TIMEOUT)
            .send()
            .await
            .map_err(|source| github_user_request_error(provider, None, source))?;
        let status = response.status();

        if !status.is_success() {
            if status == StatusCode::UNAUTHORIZED {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: provider.id.clone(),
                    },
                });
            }

            let body = read_github_error_body(response)
                .await
                .map_err(|source| github_user_request_error(provider, Some(status), source))?;
            if status == StatusCode::FORBIDDEN && github_forbidden_body_indicates_auth(&body) {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: provider.id.clone(),
                    },
                });
            }

            return Err(github_user_status_error(provider, status, &body, token));
        }

        let response = response
            .json::<GitHubUserResponse>()
            .await
            .map_err(|source| github_user_request_error(provider, Some(status), source))?;

        response.into_repository_user(provider)
    }
}

/// HTTP client for checking GitHub repository permissions.
///
/// GitHub's collaborator permission endpoint returns a legacy base permission
/// (`read`, `write`, `admin`, or `none`) after considering direct, team,
/// organization, and enterprise grants. LFS Cloud uses that base permission as
/// the conservative MVP authorization source for LFS downloads and uploads.
#[derive(Clone, Debug)]
pub struct GitHubRepositoryPermissionClient {
    client: Client,
}

/// GitHub repository-provider adapter used by production LFS authorization.
///
/// The adapter consumes the generic per-session authentication context and
/// keeps GitHub-specific token parsing and permission API behavior behind the
/// [`RepositoryProvider`] boundary.
#[derive(Clone, Debug)]
pub struct GitHubRepositoryProvider {
    provider: GitHubProviderConfig,
    permission_client: Option<GitHubRepositoryPermissionClient>,
}

impl GitHubRepositoryProvider {
    /// Creates a GitHub repository provider using the default API client.
    #[must_use]
    pub fn new(provider: GitHubProviderConfig) -> Self {
        Self {
            provider,
            permission_client: None,
        }
    }

    /// Creates a GitHub repository provider with an explicit permission client.
    ///
    /// This constructor lets tests and future server composition share a
    /// configured HTTP client without weakening the production provider trait.
    #[must_use]
    pub fn with_client(
        provider: GitHubProviderConfig,
        permission_client: GitHubRepositoryPermissionClient,
    ) -> Self {
        Self {
            provider,
            permission_client: Some(permission_client),
        }
    }
}

impl RepositoryProvider for GitHubRepositoryProvider {
    fn provider_id(&self) -> &str {
        &self.provider.id
    }

    fn check_permission<'a>(
        &'a self,
        repository: &'a RepositoryIdentity,
        authentication: &'a RepositoryAuthentication,
        required: RepositoryPermission,
    ) -> ProviderFuture<'a, ServerResult<RepositoryAuthorization>> {
        Box::pin(async move {
            let token =
                GitHubOAuthAccessToken::from_secret(authentication.access_token().to_owned())?;
            let client = match &self.permission_client {
                Some(client) => client.clone(),
                None => GitHubRepositoryPermissionClient::new()?,
            };

            client
                .check_permission(
                    &self.provider,
                    &token,
                    repository,
                    authentication.user(),
                    required,
                )
                .await
        })
    }
}

impl GitHubRepositoryPermissionClient {
    /// Creates a GitHub repository permission client with default HTTP settings.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if the default HTTP client cannot be built.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::GitHubRepositoryPermissionClient;
    ///
    /// let client = GitHubRepositoryPermissionClient::new()?;
    ///
    /// assert!(format!("{client:?}").contains("GitHubRepositoryPermissionClient"));
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn new() -> ServerResult<Self> {
        Ok(Self::with_client(default_github_api_http_client()?))
    }

    /// Creates a GitHub repository permission client from an existing client.
    ///
    /// This is useful for tests and for server code that shares one configured
    /// GitHub API client across identity and authorization calls.
    #[must_use]
    pub fn with_client(client: Client) -> Self {
        Self { client }
    }

    /// Checks whether `user` has `required` access to `repository`.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the GitHub API URL is invalid, the request
    /// fails, GitHub requires SSO authorization, or the returned permission does
    /// not satisfy the requested access.
    pub async fn check_permission(
        &self,
        provider: &GitHubProviderConfig,
        token: &GitHubOAuthAccessToken,
        repository: &RepositoryIdentity,
        user: &RepositoryUser,
        required: RepositoryPermission,
    ) -> ServerResult<RepositoryAuthorization> {
        let expected_user_id = validate_github_permission_request(provider, repository, user)?;
        self.verify_repository_identity(provider, token, repository)
            .await?;
        let endpoint = github_repository_permission_endpoint(provider, repository, user)?;
        let response = github_api_request(self.client.get(endpoint))
            .bearer_auth(token.as_str())
            .timeout(GITHUB_PERMISSION_CHECK_TIMEOUT)
            .send()
            .await
            .map_err(|source| github_permission_request_error(provider, None, source))?;
        let status = response.status();

        if !status.is_success() {
            if status == StatusCode::UNAUTHORIZED {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: provider.id.clone(),
                    },
                });
            }

            if status == StatusCode::NOT_FOUND {
                return Err(github_permission_denied(provider, repository, required));
            }

            if status == StatusCode::FORBIDDEN && response.headers().contains_key(GITHUB_SSO_HEADER)
            {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::SsoRequired {
                        provider: provider.id.clone(),
                        organization: repository.owner.clone(),
                    },
                });
            }

            let body = read_github_error_body(response).await.map_err(|source| {
                github_permission_request_error(provider, Some(status), source)
            })?;
            if status == StatusCode::FORBIDDEN && github_forbidden_body_indicates_auth(&body) {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: provider.id.clone(),
                    },
                });
            }

            return Err(github_permission_status_error(
                provider, status, &body, token,
            ));
        }

        let response = response
            .json::<GitHubRepositoryPermissionResponse>()
            .await
            .map_err(|source| github_permission_request_error(provider, Some(status), source))?;
        let granted = match response.base_permission() {
            GitHubBasePermission::Granted(granted) => granted,
            GitHubBasePermission::None => {
                return Err(github_permission_denied(provider, repository, required));
            }
            GitHubBasePermission::Unknown(permission) => {
                tracing::warn!(
                    provider = %provider.id,
                    owner = %repository.owner,
                    repo = %repository.name,
                    permission = %sanitize_oauth_diagnostic_value(&permission),
                    "github repository permission response contained an unknown base permission"
                );
                return Err(github_permission_denied(provider, repository, required));
            }
        };

        if !github_permission_satisfies(granted, required) {
            return Err(github_permission_denied(provider, repository, required));
        }

        let actual_user_id = response.stable_user_id().ok_or_else(|| {
            repository_provider_upstream_error(
                provider,
                Some(status.as_u16()),
                "malformed github repository permission user identity",
            )
        })?;
        if actual_user_id != expected_user_id {
            tracing::warn!(
                provider = %provider.id,
                owner = %repository.owner,
                repo = %repository.name,
                login = %user.login,
                "github repository permission user identity did not match the authenticated session"
            );
            return Err(github_permission_denied(provider, repository, required));
        }

        Ok(RepositoryAuthorization {
            user: user.clone(),
            repository: repository.clone(),
            required,
            granted,
        })
    }

    async fn verify_repository_identity(
        &self,
        provider: &GitHubProviderConfig,
        token: &GitHubOAuthAccessToken,
        repository: &RepositoryIdentity,
    ) -> ServerResult<()> {
        let expected_id = repository
            .stable_id
            .as_deref()
            .ok_or_else(|| ServerError::InvalidConfiguration {
                message: format!(
                    "github repository {}/{} is missing its stable repository ID",
                    repository.owner, repository.name
                ),
            })?
            .parse::<u64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| ServerError::InvalidConfiguration {
                message: format!(
                    "github repository {}/{} has an invalid stable repository ID",
                    repository.owner, repository.name
                ),
            })?;
        let endpoint = github_repository_identity_endpoint(provider, repository)?;
        let response = github_api_request(self.client.get(endpoint))
            .bearer_auth(token.as_str())
            .timeout(GITHUB_PERMISSION_CHECK_TIMEOUT)
            .send()
            .await
            .map_err(|source| github_permission_request_error(provider, None, source))?;
        let status = response.status();

        if !status.is_success() {
            if status == StatusCode::UNAUTHORIZED {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: provider.id.clone(),
                    },
                });
            }
            if status == StatusCode::NOT_FOUND {
                return Err(github_repository_not_found(provider, repository));
            }
            if status == StatusCode::FORBIDDEN && response.headers().contains_key(GITHUB_SSO_HEADER)
            {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::SsoRequired {
                        provider: provider.id.clone(),
                        organization: repository.owner.clone(),
                    },
                });
            }

            let body = read_github_error_body(response).await.map_err(|source| {
                github_permission_request_error(provider, Some(status), source)
            })?;
            if status == StatusCode::FORBIDDEN && github_forbidden_body_indicates_auth(&body) {
                return Err(ServerError::RepositoryProvider {
                    source: RepositoryProviderError::AuthenticationRequired {
                        provider: provider.id.clone(),
                    },
                });
            }

            return Err(github_permission_status_error(
                provider, status, &body, token,
            ));
        }

        let response = response
            .json::<GitHubRepositoryIdentityResponse>()
            .await
            .map_err(|source| github_permission_request_error(provider, Some(status), source))?;
        let actual_id = response.id.ok_or_else(|| {
            repository_provider_upstream_error(
                provider,
                Some(status.as_u16()),
                "malformed github repository identity response",
            )
        })?;

        if actual_id != expected_id {
            tracing::warn!(
                provider = %provider.id,
                owner = %repository.owner,
                repo = %repository.name,
                "github repository stable identity did not match configured mapping"
            );
            return Err(github_repository_not_found(provider, repository));
        }

        Ok(())
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
        Self::with_client_and_token_url(default_github_oauth_http_client()?, token_url)
    }

    /// Creates a token exchanger with an explicit HTTP client and token endpoint.
    ///
    /// This is useful when server code wants to share a tuned [`Client`] across
    /// provider components while tests point the exchange at a local token server.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when `token_url` is not a valid absolute URL.
    pub fn with_client_and_token_url(
        client: Client,
        token_url: impl AsRef<str>,
    ) -> ServerResult<Self> {
        let token_url = Url::parse(token_url.as_ref())
            .map_err(|source| invalid_oauth_url("github oauth token endpoint", source))?;

        Ok(Self { client, token_url })
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
        let raw_redirect_url = redirect_url.as_ref();
        let parsed_redirect_url = Url::parse(raw_redirect_url)
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        validate_github_transport_url(
            &parsed_redirect_url,
            "github oauth redirect_url",
            provider.allow_insecure_http,
        )?;
        validate_github_transport_url(
            &self.token_url,
            "github oauth token endpoint",
            provider.allow_insecure_http,
        )?;
        let redirect_url = RedirectUrl::new(raw_redirect_url.to_owned())
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        let request_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("client_id", provider.oauth_client_id.as_str())
            .append_pair("client_secret", provider.oauth_client_secret.as_str())
            .append_pair("code", callback.code.as_str())
            .append_pair("redirect_uri", redirect_url.as_str())
            .append_pair("code_verifier", callback.pkce_verifier.secret())
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
        let response_body =
            read_bounded_github_response_body(response, MAX_GITHUB_OAUTH_TOKEN_RESPONSE_BODY_LEN)
                .await
                .map_err(|source| {
                    token_exchange_upstream_error(provider, Some(status.as_u16()), source)
                })?;
        let response_body = match response_body {
            BoundedGitHubResponseBody::Complete(body) => body,
            BoundedGitHubResponseBody::ExceededLimit => {
                return Err(token_exchange_response_too_large_error(
                    provider,
                    status.as_u16(),
                ));
            }
        };

        if !status.is_success() {
            let error = String::from_utf8_lossy(&response_body);

            if let Some(error) = parse_token_error_response(&error) {
                return Err(oauth_token_exchange_error(redact_token_exchange_error(
                    error, provider, callback,
                )));
            }

            return Err(oauth_token_exchange_non_oauth_error(
                provider,
                status.as_u16(),
                &error,
                callback,
            ));
        }

        let response =
            serde_json::from_slice::<GitHubOAuthTokenResponse>(&response_body).map_err(|_| {
                repository_provider_upstream_error(
                    provider,
                    Some(status.as_u16()),
                    "malformed github oauth token response",
                )
            })?;
        if let Some(error) = response.oauth_error() {
            return Err(oauth_token_exchange_error(redact_token_exchange_error(
                error, provider, callback,
            )));
        }

        GitHubOAuthToken::try_from_response(provider, response)
    }
}

/// Browser URL plus one-time CSRF and PKCE secrets for an OAuth attempt.
pub struct GitHubOAuthAuthorization {
    /// URL the user should open in a browser to start GitHub OAuth login.
    pub authorization_url: Url,
    /// CSRF state that must match the callback's `state` query parameter.
    pub csrf_state: GitHubOAuthState,
    pkce_verifier: PkceCodeVerifier,
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
    ///     allow_insecure_http: false,
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
        let raw_redirect_url = redirect_url.into();
        let parsed_redirect_url = Url::parse(&raw_redirect_url)
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        validate_github_transport_url(
            &parsed_redirect_url,
            "github oauth redirect_url",
            provider.allow_insecure_http,
        )?;
        let redirect_url = RedirectUrl::new(raw_redirect_url)
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        let auth_url = AuthUrl::new(GITHUB_OAUTH_AUTHORIZE_URL.to_owned())
            .map_err(|source| invalid_oauth_url("github oauth authorization endpoint", source))?;
        let client = BasicClient::new(ClientId::new(provider.oauth_client_id.clone()))
            .set_auth_uri(auth_url)
            .set_redirect_uri(redirect_url);
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let mut request = client
            .authorize_url(state_fn)
            .set_pkce_challenge(pkce_challenge);
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
            pkce_verifier,
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

/// Shared one-time registry for GitHub OAuth authorization attempts.
///
/// Register each generated [`GitHubOAuthAuthorization`] before returning its
/// URL. The callback route consumes the matching CSRF state and its PKCE
/// verifier before exchanging the code, which lets one mounted router handle
/// many concurrent login attempts without accepting replayed callbacks.
/// Abandoned attempts expire, while capacity exhaustion rejects unrelated
/// attempts instead of evicting an active login. State digests provide direct
/// bounded-time lookup without retaining the raw state or scanning every
/// pending attempt.
#[derive(Clone)]
pub struct GitHubOAuthStateRegistry {
    states: Arc<Mutex<HashMap<[u8; 32], GitHubOAuthPendingAttempt>>>,
    clock: Arc<dyn Fn() -> Instant + Send + Sync>,
}

struct GitHubOAuthPendingAttempt {
    registered_at: Instant,
    pkce_verifier: PkceCodeVerifier,
}

impl Default for GitHubOAuthStateRegistry {
    fn default() -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            clock: Arc::new(Instant::now),
        }
    }
}

impl GitHubOAuthStateRegistry {
    /// Creates an empty pending-authorization registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_state(state: GitHubOAuthState) -> Self {
        let registry = Self::new();
        registry
            .register_state(state)
            .expect("a new OAuth state registry must have capacity");
        registry
    }

    /// Registers a generated authorization attempt for one future callback.
    ///
    /// The returned URL is safe to redirect to only after this method succeeds.
    /// Consuming the authorization value ensures its PKCE verifier has exactly
    /// one owner until the callback removes it from the registry.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::{
    ///     GitHubOAuthAuthorization, GitHubOAuthStateRegistry, GitHubProviderConfig,
    /// };
    ///
    /// let provider = GitHubProviderConfig {
    ///     id: "github-main".to_owned(),
    ///     api_url: "https://api.github.com".to_owned(),
    ///     oauth_client_id: "client-id".to_owned(),
    ///     oauth_client_secret: "client-secret".to_owned(),
    ///     allow_insecure_http: false,
    /// };
    /// let authorization = GitHubOAuthAuthorization::new(
    ///     &provider,
    ///     "http://127.0.0.1:8080/auth/github/callback",
    /// )?;
    /// let registry = GitHubOAuthStateRegistry::new();
    /// let authorization_url = registry.register(authorization)?;
    ///
    /// assert_eq!(authorization_url.host_str(), Some("github.com"));
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    ///
    /// # Errors
    ///
    /// Returns [`ServerError::RateLimited`] when the pending-state capacity is
    /// exhausted. Existing attempts are never evicted to admit a new request.
    pub fn register(&self, authorization: GitHubOAuthAuthorization) -> ServerResult<Url> {
        let GitHubOAuthAuthorization {
            authorization_url,
            csrf_state,
            pkce_verifier,
        } = authorization;
        self.register_attempt(csrf_state, pkce_verifier)?;
        Ok(authorization_url)
    }

    fn register_attempt(
        &self,
        state: GitHubOAuthState,
        pkce_verifier: PkceCodeVerifier,
    ) -> ServerResult<()> {
        let now = (self.clock)();
        let state_key = oauth_state_key(&state);
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_oauth_states(&mut states, now);
        if !states.contains_key(&state_key) && states.len() >= MAX_PENDING_GITHUB_OAUTH_STATES {
            return Err(ServerError::RateLimited {
                retry_after_seconds: GITHUB_OAUTH_STATE_TTL.as_secs(),
            });
        }
        states.insert(
            state_key,
            GitHubOAuthPendingAttempt {
                registered_at: now,
                pkce_verifier,
            },
        );
        Ok(())
    }

    fn consume(&self, state: &GitHubOAuthState) -> Option<PkceCodeVerifier> {
        let now = (self.clock)();
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_oauth_states(&mut states, now);
        states
            .remove(&oauth_state_key(state))
            .map(|attempt| attempt.pkce_verifier)
    }

    fn len(&self) -> usize {
        let now = (self.clock)();
        let mut states = self
            .states
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_oauth_states(&mut states, now);
        states.len()
    }

    #[cfg(test)]
    fn with_clock(clock: Arc<dyn Fn() -> Instant + Send + Sync>) -> Self {
        Self {
            states: Arc::new(Mutex::new(HashMap::new())),
            clock,
        }
    }

    #[cfg(test)]
    fn register_state(&self, state: GitHubOAuthState) -> ServerResult<()> {
        let (_, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        self.register_attempt(state, pkce_verifier)
    }
}

fn oauth_state_key(state: &GitHubOAuthState) -> [u8; 32] {
    Sha256::digest(state.as_str().as_bytes()).into()
}

fn prune_expired_oauth_states(
    states: &mut HashMap<[u8; 32], GitHubOAuthPendingAttempt>,
    now: Instant,
) {
    states.retain(|_, attempt| now.duration_since(attempt.registered_at) < GITHUB_OAUTH_STATE_TTL);
}

impl fmt::Debug for GitHubOAuthStateRegistry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubOAuthStateRegistry")
            .field("pending_states", &self.len())
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
pub struct GitHubOAuthCallback {
    /// Authorization code returned by GitHub.
    pub code: GitHubOAuthCode,
    /// CSRF state that matched the stored authorization attempt.
    pub state: GitHubOAuthState,
    pkce_verifier: PkceCodeVerifier,
}

impl GitHubOAuthCallback {
    /// Validates a callback against one generated OAuth authorization attempt.
    ///
    /// The returned callback contains the authorization code and one-time PKCE
    /// verifier only after the state matches. Callers should then exchange the
    /// code for a GitHub token.
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
    ///     GitHubOAuthAuthorization, GitHubOAuthCallback, GitHubOAuthCallbackQuery,
    ///     GitHubProviderConfig,
    /// };
    ///
    /// let provider = GitHubProviderConfig {
    ///     id: "github-main".to_owned(),
    ///     api_url: "https://api.github.com".to_owned(),
    ///     oauth_client_id: "client-id".to_owned(),
    ///     oauth_client_secret: "client-secret".to_owned(),
    ///     allow_insecure_http: false,
    /// };
    /// let authorization = GitHubOAuthAuthorization::new(
    ///     &provider,
    ///     "http://127.0.0.1:8080/auth/github/callback",
    /// )?;
    /// let state = authorization.csrf_state.as_str().to_owned();
    /// let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", state);
    ///
    /// let callback = GitHubOAuthCallback::validate(query, authorization)?;
    ///
    /// assert_eq!(callback.code.as_str(), "oauth-code");
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn validate(
        query: GitHubOAuthCallbackQuery,
        authorization: GitHubOAuthAuthorization,
    ) -> ServerResult<Self> {
        let GitHubOAuthAuthorization {
            csrf_state: expected_state,
            pkce_verifier,
            ..
        } = authorization;
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
            pkce_verifier,
        })
    }

    #[cfg(test)]
    fn validate_state(
        query: GitHubOAuthCallbackQuery,
        expected_state: &GitHubOAuthState,
    ) -> ServerResult<Self> {
        let (_, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        Self::validate(
            query,
            GitHubOAuthAuthorization {
                authorization_url: Url::parse(GITHUB_OAUTH_AUTHORIZE_URL)
                    .expect("built-in GitHub authorization URL must parse"),
                csrf_state: expected_state.clone(),
                pkce_verifier,
            },
        )
    }

    fn validate_registered(
        query: GitHubOAuthCallbackQuery,
        csrf_states: &GitHubOAuthStateRegistry,
    ) -> ServerResult<Self> {
        let state = GitHubOAuthState(required_callback_param(query.state, "state")?);
        let pkce_verifier =
            csrf_states
                .consume(&state)
                .ok_or_else(|| ServerError::Unauthorized {
                    reason: "github oauth csrf state mismatch".to_owned(),
                })?;

        // Consume the one-time state before honoring provider-denied callbacks so
        // denied OAuth redirects cannot be replayed with an authorization code.
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
            state,
            pkce_verifier,
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
///     allow_insecure_http: false,
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
/// Server code that performs repeated token exchanges should prefer holding a
/// reusable [`GitHubOAuthTokenExchanger`] so connection pooling remains warm.
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

/// Fetches the authenticated GitHub user using the default GitHub API client.
///
/// Server code that performs repeated provider calls should prefer holding a
/// reusable [`GitHubUserClient`] so connection pooling remains warm.
///
/// # Errors
///
/// Returns [`ServerError`] when the GitHub user lookup cannot complete.
pub async fn fetch_authenticated_github_user(
    provider: &GitHubProviderConfig,
    token: &GitHubOAuthAccessToken,
) -> ServerResult<RepositoryUser> {
    GitHubUserClient::new()?
        .fetch_authenticated_user(provider, token)
        .await
}

#[derive(Debug, Deserialize)]
struct GitHubOAuthTokenResponse {
    access_token: Option<String>,
    token_type: Option<String>,
    scope: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubUserResponse {
    login: Option<String>,
    id: Option<u64>,
}

impl GitHubUserResponse {
    fn into_repository_user(self, provider: &GitHubProviderConfig) -> ServerResult<RepositoryUser> {
        let login = self
            .login
            .ok_or_else(|| malformed_github_user_response_error(provider, "missing login"))?;
        let login = validate_sensitive_oauth_value(login, "github authenticated user login")
            .map_err(|error| {
                malformed_github_user_response_error(provider, &format!("invalid login: {error}"))
            })?;

        let id = self
            .id
            .filter(|id| *id > 0)
            .ok_or_else(|| malformed_github_user_response_error(provider, "missing id"))?;

        Ok(RepositoryUser::new(
            provider.id.clone(),
            login,
            Some(id.to_string()),
        ))
    }
}

#[derive(Debug, Deserialize)]
struct GitHubRepositoryPermissionResponse {
    permission: Option<String>,
    user: Option<GitHubRepositoryPermissionUserResponse>,
}

#[derive(Debug, Deserialize)]
struct GitHubRepositoryPermissionUserResponse {
    id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GitHubRepositoryIdentityResponse {
    id: Option<u64>,
}

enum GitHubBasePermission {
    Granted(RepositoryPermission),
    None,
    Unknown(String),
}

impl GitHubRepositoryPermissionResponse {
    fn base_permission(&self) -> GitHubBasePermission {
        match self.permission.as_deref() {
            Some("read") => GitHubBasePermission::Granted(RepositoryPermission::Read),
            Some("write") => GitHubBasePermission::Granted(RepositoryPermission::Write),
            Some("admin") => GitHubBasePermission::Granted(RepositoryPermission::Admin),
            Some("none") | None => GitHubBasePermission::None,
            Some(permission) => GitHubBasePermission::Unknown(permission.to_owned()),
        }
    }

    fn stable_user_id(&self) -> Option<u64> {
        self.user.as_ref()?.id.filter(|id| *id > 0)
    }
}

#[derive(Debug, Deserialize)]
struct GitHubApiErrorResponse {
    message: Option<String>,
    #[serde(default)]
    errors: Vec<serde_json::Value>,
}

impl GitHubOAuthTokenResponse {
    fn oauth_error(&self) -> Option<GitHubOAuthTokenErrorResponse> {
        let error = GitHubOAuthTokenErrorResponse {
            error: self.error.clone(),
            error_description: self.error_description.clone(),
        };

        error.has_diagnostic().then_some(error)
    }
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
    fn has_diagnostic(&self) -> bool {
        self.error
            .as_deref()
            .map(sanitize_oauth_diagnostic_value)
            .is_some_and(|value| !value.is_empty())
            || self
                .error_description
                .as_deref()
                .map(sanitize_oauth_diagnostic_value)
                .is_some_and(|value| !value.is_empty())
    }

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
    let token_type = validate_sensitive_oauth_value(token_type, "github oauth token type")?;
    if token_type.eq_ignore_ascii_case("bearer") {
        Ok(token_type)
    } else {
        Err(ServerError::InvalidRequest {
            message: "github oauth token type must be bearer".to_owned(),
        })
    }
}

fn parse_scope_list(scopes: &str) -> Vec<String> {
    scopes
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_token_error_response(body: &str) -> Option<GitHubOAuthTokenErrorResponse> {
    let json_error = serde_json::from_str::<GitHubOAuthTokenErrorResponse>(body).ok();
    if json_error
        .as_ref()
        .is_some_and(|error| error.has_diagnostic())
    {
        return json_error;
    }

    let mut form_error = GitHubOAuthTokenErrorResponse {
        error: None,
        error_description: None,
    };
    for (key, value) in url::form_urlencoded::parse(body.as_bytes()) {
        match key.as_ref() {
            "error" => form_error.error = Some(value.into_owned()),
            "error_description" => form_error.error_description = Some(value.into_owned()),
            _ => {}
        }
    }

    form_error.has_diagnostic().then_some(form_error)
}

fn token_exchange_upstream_error(
    provider: &GitHubProviderConfig,
    status: Option<u16>,
    source: reqwest::Error,
) -> ServerError {
    let message = if source.is_timeout() {
        "github oauth token exchange timed out"
    } else if source.is_connect() {
        "github oauth token endpoint connection failed"
    } else if source.is_decode() {
        "malformed github oauth token response"
    } else if source.is_body() {
        "github oauth token response body could not be read"
    } else {
        "github oauth token exchange request failed"
    };

    repository_provider_upstream_error(provider, status, message)
}

fn github_user_request_error(
    provider: &GitHubProviderConfig,
    status: Option<StatusCode>,
    source: reqwest::Error,
) -> ServerError {
    let message = if source.is_timeout() {
        "github authenticated user request timed out"
    } else if source.is_connect() {
        "github api connection failed while fetching authenticated user"
    } else if source.is_decode() {
        "malformed github authenticated user response"
    } else if source.is_body() {
        "github authenticated user response body could not be read"
    } else {
        "github authenticated user request failed"
    };

    repository_provider_upstream_error(provider, status.map(|status| status.as_u16()), message)
}

fn github_user_status_error(
    provider: &GitHubProviderConfig,
    status: StatusCode,
    body: &str,
    token: &GitHubOAuthAccessToken,
) -> ServerError {
    let message = github_api_error_message(body, token)
        .unwrap_or_else(|| "github authenticated user request failed".to_owned());

    repository_provider_upstream_error(provider, Some(status.as_u16()), &message)
}

fn github_permission_request_error(
    provider: &GitHubProviderConfig,
    status: Option<StatusCode>,
    source: reqwest::Error,
) -> ServerError {
    let message = if source.is_timeout() {
        "github repository permission request timed out"
    } else if source.is_connect() {
        "github api connection failed while checking repository permission"
    } else if source.is_decode() {
        "malformed github repository permission response"
    } else if source.is_body() {
        "github repository permission response body could not be read"
    } else {
        "github repository permission request failed"
    };

    repository_provider_upstream_error(provider, status.map(|status| status.as_u16()), message)
}

fn github_permission_status_error(
    provider: &GitHubProviderConfig,
    status: StatusCode,
    body: &str,
    token: &GitHubOAuthAccessToken,
) -> ServerError {
    let message = github_api_error_message(body, token)
        .unwrap_or_else(|| "github repository permission request failed".to_owned());

    repository_provider_upstream_error(provider, Some(status.as_u16()), &message)
}

fn github_permission_denied(
    provider: &GitHubProviderConfig,
    repository: &RepositoryIdentity,
    required: RepositoryPermission,
) -> ServerError {
    ServerError::RepositoryProvider {
        source: RepositoryProviderError::PermissionDenied {
            provider: provider.id.clone(),
            owner: repository.owner.clone(),
            repo: repository.name.clone(),
            required,
        },
    }
}

fn github_repository_not_found(
    provider: &GitHubProviderConfig,
    repository: &RepositoryIdentity,
) -> ServerError {
    ServerError::RepositoryProvider {
        source: RepositoryProviderError::RepositoryNotFound {
            provider: provider.id.clone(),
            owner: repository.owner.clone(),
            repo: repository.name.clone(),
        },
    }
}

#[derive(Debug, Eq, PartialEq)]
enum BoundedGitHubResponseBody {
    Complete(Vec<u8>),
    ExceededLimit,
}

async fn read_bounded_github_response_body(
    mut response: reqwest::Response,
    max_len: usize,
) -> Result<BoundedGitHubResponseBody, reqwest::Error> {
    if response
        .content_length()
        .is_some_and(|content_length| content_length > max_len as u64)
    {
        return Ok(BoundedGitHubResponseBody::ExceededLimit);
    }

    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if body.len().saturating_add(chunk.len()) > max_len {
            return Ok(BoundedGitHubResponseBody::ExceededLimit);
        }
        body.extend_from_slice(&chunk);
    }

    Ok(BoundedGitHubResponseBody::Complete(body))
}

async fn read_github_error_body(response: reqwest::Response) -> Result<String, reqwest::Error> {
    let mut response = response;
    let mut body = Vec::new();
    while body.len() < MAX_GITHUB_ERROR_BODY_LEN {
        let Some(chunk) = response.chunk().await? else {
            break;
        };
        let remaining = MAX_GITHUB_ERROR_BODY_LEN - body.len();
        if chunk.len() > remaining {
            body.extend_from_slice(&chunk[..remaining]);
            break;
        }
        body.extend_from_slice(&chunk);
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn malformed_github_user_response_error(
    provider: &GitHubProviderConfig,
    message: &str,
) -> ServerError {
    repository_provider_upstream_error(
        provider,
        None,
        &format!("malformed github authenticated user response: {message}"),
    )
}

fn malformed_token_response_error(provider: &GitHubProviderConfig, message: &str) -> ServerError {
    repository_provider_upstream_error(
        provider,
        None,
        &format!("malformed github oauth token response: {message}"),
    )
}

fn oauth_token_exchange_error(error: GitHubOAuthTokenErrorResponse) -> ServerError {
    ServerError::Unauthorized {
        reason: format!(
            "github oauth token exchange failed: {}",
            error.to_sanitized_reason()
        ),
    }
}

fn oauth_token_exchange_non_oauth_error(
    provider: &GitHubProviderConfig,
    status: u16,
    body: &str,
    callback: &GitHubOAuthCallback,
) -> ServerError {
    let body = redact_token_exchange_secrets(body, provider, callback);
    let body = sanitize_oauth_diagnostic_value(&body);
    let message = if body.is_empty() {
        "github oauth token endpoint returned a non-oauth error response".to_owned()
    } else {
        format!("github oauth token endpoint returned a non-oauth error response: {body}")
    };

    repository_provider_upstream_error(provider, Some(status), &message)
}

fn token_exchange_response_too_large_error(
    provider: &GitHubProviderConfig,
    status: u16,
) -> ServerError {
    repository_provider_upstream_error(
        provider,
        Some(status),
        &format!(
            "github oauth token response exceeded the \
             {MAX_GITHUB_OAUTH_TOKEN_RESPONSE_BODY_LEN}-byte limit"
        ),
    )
}

fn redact_token_exchange_error(
    error: GitHubOAuthTokenErrorResponse,
    provider: &GitHubProviderConfig,
    callback: &GitHubOAuthCallback,
) -> GitHubOAuthTokenErrorResponse {
    GitHubOAuthTokenErrorResponse {
        error: error
            .error
            .map(|value| redact_token_exchange_secrets(&value, provider, callback)),
        error_description: error
            .error_description
            .map(|value| redact_token_exchange_secrets(&value, provider, callback)),
    }
}

fn redact_token_exchange_secrets(
    value: &str,
    provider: &GitHubProviderConfig,
    callback: &GitHubOAuthCallback,
) -> String {
    let mut secrets = [
        provider.oauth_client_secret.as_str(),
        callback.code.as_str(),
        callback.pkce_verifier.secret(),
    ];
    secrets.sort_unstable_by_key(|secret| std::cmp::Reverse(secret.len()));

    secrets
        .into_iter()
        .filter(|secret| !secret.is_empty())
        .fold(value.to_owned(), |redacted, secret| {
            redacted.replace(secret, "<redacted>")
        })
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

fn validate_github_transport_url(
    url: &Url,
    path: &str,
    allow_insecure_http: bool,
) -> ServerResult<()> {
    if allow_insecure_http || uses_protected_http_transport(url) {
        return Ok(());
    }

    Err(ServerError::InvalidConfiguration {
        message: format!(
            "{path} must use HTTPS unless it targets an exact loopback IP; enable allow_insecure_http only for a trusted development network"
        ),
    })
}

fn github_user_endpoint(provider: &GitHubProviderConfig) -> ServerResult<Url> {
    let mut endpoint = Url::parse(&provider.api_url)
        .map_err(|source| invalid_oauth_url("github api_url", source))?;
    validate_github_transport_url(&endpoint, "github api_url", provider.allow_insecure_http)?;
    let base_path = endpoint.path().trim_end_matches('/');
    endpoint.set_path(&format!("{base_path}/user"));
    endpoint.set_query(None);
    endpoint.set_fragment(None);

    Ok(endpoint)
}

fn github_repository_permission_endpoint(
    provider: &GitHubProviderConfig,
    repository: &RepositoryIdentity,
    user: &RepositoryUser,
) -> ServerResult<Url> {
    let mut endpoint = Url::parse(&provider.api_url)
        .map_err(|source| invalid_oauth_url("github api_url", source))?;
    validate_github_transport_url(&endpoint, "github api_url", provider.allow_insecure_http)?;
    append_github_api_path(
        &mut endpoint,
        [
            "repos",
            repository.owner.as_str(),
            repository.name.as_str(),
            "collaborators",
            user.login.as_str(),
            "permission",
        ],
    )?;
    endpoint.set_query(None);
    endpoint.set_fragment(None);

    Ok(endpoint)
}

fn github_repository_identity_endpoint(
    provider: &GitHubProviderConfig,
    repository: &RepositoryIdentity,
) -> ServerResult<Url> {
    let mut endpoint = Url::parse(&provider.api_url)
        .map_err(|source| invalid_oauth_url("github api_url", source))?;
    validate_github_transport_url(&endpoint, "github api_url", provider.allow_insecure_http)?;
    append_github_api_path(
        &mut endpoint,
        ["repos", repository.owner.as_str(), repository.name.as_str()],
    )?;
    endpoint.set_query(None);
    endpoint.set_fragment(None);

    Ok(endpoint)
}

fn append_github_api_path<'a>(
    endpoint: &mut Url,
    segments: impl IntoIterator<Item = &'a str>,
) -> ServerResult<()> {
    let mut path = endpoint
        .path_segments_mut()
        .map_err(|_| ServerError::InvalidConfiguration {
            message: "github api_url cannot be used as a base URL".to_owned(),
        })?;
    path.pop_if_empty().extend(segments);

    Ok(())
}

fn validate_github_permission_request(
    provider: &GitHubProviderConfig,
    repository: &RepositoryIdentity,
    user: &RepositoryUser,
) -> ServerResult<u64> {
    if repository.provider_id != provider.id {
        return Err(ServerError::InvalidRequest {
            message: format!(
                "repository provider id {} does not match github provider {}",
                repository.provider_id, provider.id
            ),
        });
    }
    if user.provider_id != provider.id {
        return Err(ServerError::InvalidRequest {
            message: format!(
                "user provider id {} does not match github provider {}",
                user.provider_id, provider.id
            ),
        });
    }
    validate_github_permission_path_segment(&repository.owner, "repository owner")?;
    validate_github_permission_path_segment(&repository.name, "repository name")?;
    validate_github_permission_path_segment(&user.login, "repository user login")?;

    user.stable_id
        .as_deref()
        .and_then(|id| id.parse::<u64>().ok())
        .filter(|id| *id > 0)
        .ok_or_else(|| ServerError::InvalidRequest {
            message: format!(
                "github repository user {} is missing a valid stable numeric ID",
                user.login
            ),
        })
}

fn validate_github_permission_path_segment(value: &str, label: &str) -> ServerResult<()> {
    if value.trim().is_empty() {
        return Err(ServerError::InvalidRequest {
            message: format!("github permission {label} must not be blank"),
        });
    }

    Ok(())
}

fn github_permission_satisfies(
    granted: RepositoryPermission,
    required: RepositoryPermission,
) -> bool {
    match required {
        RepositoryPermission::Read => matches!(
            granted,
            RepositoryPermission::Read | RepositoryPermission::Write | RepositoryPermission::Admin
        ),
        RepositoryPermission::Write => {
            matches!(
                granted,
                RepositoryPermission::Write | RepositoryPermission::Admin
            )
        }
        RepositoryPermission::Admin => matches!(granted, RepositoryPermission::Admin),
    }
}

fn default_github_oauth_http_client() -> ServerResult<Client> {
    match DEFAULT_GITHUB_OAUTH_HTTP_CLIENT.get_or_init(|| {
        build_github_oauth_http_client()
            .map_err(|source| sanitize_oauth_diagnostic_value(&source.to_string()))
    }) {
        Ok(client) => Ok(client.clone()),
        Err(message) => Err(ServerError::InvalidConfiguration {
            message: format!("github oauth http client could not be built: {message}"),
        }),
    }
}

fn default_github_api_http_client() -> ServerResult<Client> {
    match DEFAULT_GITHUB_API_HTTP_CLIENT.get_or_init(|| {
        build_github_api_http_client()
            .map_err(|source| sanitize_oauth_diagnostic_value(&source.to_string()))
    }) {
        Ok(client) => Ok(client.clone()),
        Err(message) => Err(ServerError::InvalidConfiguration {
            message: format!("github api http client could not be built: {message}"),
        }),
    }
}

fn github_api_request(request: RequestBuilder) -> RequestBuilder {
    request
        .header(ACCEPT, HeaderValue::from_static(GITHUB_API_ACCEPT))
        .header(
            GITHUB_API_VERSION_HEADER,
            HeaderValue::from_static(GITHUB_API_VERSION),
        )
}

fn build_github_oauth_http_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .timeout(GITHUB_OAUTH_TOKEN_EXCHANGE_TIMEOUT)
        .user_agent(GITHUB_USER_AGENT)
        .redirect(Policy::none())
        .build()
}

fn build_github_api_http_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .user_agent(GITHUB_USER_AGENT)
        .redirect(Policy::none())
        .build()
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

fn github_api_error_message(body: &str, token: &GitHubOAuthAccessToken) -> Option<String> {
    serde_json::from_str::<GitHubApiErrorResponse>(body)
        .ok()
        .and_then(github_api_error_diagnostic)
        .map(|message| redact_oauth_secret(&message, token))
        .filter(|message| !message.trim().is_empty())
}

fn redact_oauth_secret(message: &str, token: &GitHubOAuthAccessToken) -> String {
    if token.as_str().is_empty() {
        return message.to_owned();
    }

    message.replace(token.as_str(), "<redacted>")
}

fn github_api_error_diagnostic(error: GitHubApiErrorResponse) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(message) = error.message.filter(|message| !message.trim().is_empty()) {
        parts.push(message);
    }
    parts.extend(
        error
            .errors
            .iter()
            .filter_map(github_api_error_detail)
            .take(3),
    );

    (!parts.is_empty()).then(|| parts.join(": "))
}

fn github_api_error_detail(error: &serde_json::Value) -> Option<String> {
    if let Some(message) = error.as_str().filter(|message| !message.trim().is_empty()) {
        return Some(message.to_owned());
    }

    let object = error.as_object()?;
    if let Some(message) = object
        .get("message")
        .and_then(serde_json::Value::as_str)
        .filter(|message| !message.trim().is_empty())
    {
        return Some(message.to_owned());
    }

    let mut detail = Vec::new();
    for key in ["resource", "field", "code"] {
        if let Some(value) = object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            detail.push(value);
        }
    }

    (!detail.is_empty()).then(|| detail.join("."))
}

fn github_forbidden_body_indicates_auth(body: &str) -> bool {
    let Some(message) = serde_json::from_str::<GitHubApiErrorResponse>(body)
        .ok()
        .and_then(github_api_error_diagnostic)
    else {
        return false;
    };
    let message = message.to_ascii_lowercase();

    message.contains("scope")
        || message.contains("resource not accessible")
        || message.contains("bad credentials")
        || message.contains("requires authentication")
        || message.contains("personal access token")
        || message.contains("oauth")
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        time::{Duration, Instant},
    };

    use axum::{
        Json, Router,
        body::Body,
        extract::{Path, State},
        http::{
            HeaderMap, HeaderValue, Request, StatusCode,
            header::{
                AUTHORIZATION, CACHE_CONTROL, CONTENT_TYPE, LOCATION, PRAGMA, REFERRER_POLICY,
                RETRY_AFTER,
            },
        },
        response::IntoResponse,
        routing::{get, post},
    };
    use oauth2::{CsrfToken, PkceCodeChallenge};
    use tokio::{sync::Mutex, task::JoinHandle};
    use tower::ServiceExt;
    use url::Url;

    use super::{
        DEFAULT_GITHUB_OAUTH_SCOPES, GITHUB_OAUTH_AUTHORIZE_URL, GITHUB_OAUTH_CALLBACK_PATH,
        GITHUB_OAUTH_LOGIN_PATH, GITHUB_OAUTH_STATE_TTL, GITHUB_OAUTH_TOKEN_URL,
        GitHubOAuthAccessToken, GitHubOAuthAuthorization, GitHubOAuthCallback,
        GitHubOAuthCallbackQuery, GitHubOAuthCallbackRouteState, GitHubOAuthState,
        GitHubOAuthStateRegistry, GitHubOAuthTokenExchanger, GitHubRepositoryPermissionClient,
        GitHubRepositoryProvider, GitHubUserClient, MAX_PENDING_GITHUB_OAUTH_STATES,
        exchange_github_oauth_code, fetch_authenticated_github_user,
        github_oauth_authorization_url, github_oauth_callback_router, github_oauth_login_router,
    };
    use crate::{
        GitHubProviderConfig, LfsSessionToken, LocalLfsSessionStore, RepositoryAuthentication,
        RepositoryIdentity, RepositoryPermission, RepositoryProvider, RepositoryProviderError,
        RepositoryUser, ServerError,
    };

    const REDIRECT_URL: &str = "http://127.0.0.1:8080/auth/github/callback";

    fn provider_config() -> GitHubProviderConfig {
        GitHubProviderConfig {
            id: "github-main".to_owned(),
            api_url: "https://api.github.com".to_owned(),
            oauth_client_id: "client-id".to_owned(),
            oauth_client_secret: "client-secret".to_owned(),
            allow_insecure_http: false,
        }
    }

    fn provider_config_with_api_url(api_url: impl Into<String>) -> GitHubProviderConfig {
        GitHubProviderConfig {
            api_url: api_url.into(),
            ..provider_config()
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
        assert_eq!(
            query.get("scope").map(String::as_str),
            Some("read:user repo")
        );
        assert_eq!(query.get("state").map(String::as_str), Some("csrf-state"));
        assert_eq!(
            query.get("code_challenge_method").map(String::as_str),
            Some("S256")
        );
        assert_eq!(query.get("code_challenge").map(String::len), Some(43));
        let expected_challenge =
            PkceCodeChallenge::from_code_verifier_sha256(&authorization.pkce_verifier);
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(expected_challenge.as_str())
        );
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
            Some("read:user repo")
        );
    }

    #[tokio::test]
    async fn token_exchange_posts_validated_callback_to_github_token_endpoint() {
        let (token_url, token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user,repo"}"#,
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
        let requests = token_server.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.accept.as_deref(), Some("application/json"));
        assert_eq!(
            request.content_type.as_deref(),
            Some("application/x-www-form-urlencoded")
        );
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
        assert!(
            request
                .body
                .get("code_verifier")
                .is_some_and(|value| value.len() >= 43)
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
    async fn token_exchange_redacts_secrets_reflected_in_json_error() {
        let callback = validated_callback("oauth-code", "csrf-state");
        let reflected_body = format!(
            r#"{{"error":"invalid_grant","error_description":"client-secret oauth-code {}"}}"#,
            callback.pkce_verifier.secret()
        );
        let (token_url, _token_server) =
            token_server(StatusCode::BAD_REQUEST, reflected_body).await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let pkce_verifier = callback.pkce_verifier.secret().to_owned();

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("reflected JSON OAuth denial should fail safely");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::Unauthorized { .. }));
        assert!(rendered.contains("invalid_grant"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("oauth-code"));
        assert!(!rendered.contains(&pkce_verifier));
    }

    #[tokio::test]
    async fn token_exchange_maps_form_encoded_github_oauth_error() {
        let (token_url, _token_server) = token_server(
            StatusCode::BAD_REQUEST,
            "error=bad_verification_code&error_description=The+code+passed+is+incorrect.",
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
        assert!(rendered.contains("The code passed is incorrect."));
        assert!(!rendered.contains("oauth-code"));
    }

    #[tokio::test]
    async fn token_exchange_redacts_secrets_reflected_in_form_error() {
        let (token_url, _token_server) = token_server(
            StatusCode::BAD_REQUEST,
            "error=invalid_grant&error_description=client-secret+oauth-code",
        )
        .await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("reflected form OAuth denial should fail safely");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::Unauthorized { .. }));
        assert!(rendered.contains("invalid_grant"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("oauth-code"));
    }

    #[tokio::test]
    async fn token_exchange_maps_non_oauth_error_body_to_upstream() {
        let (token_url, _token_server) =
            token_server(StatusCode::BAD_GATEWAY, r#"{"message":"try again later"}"#).await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("non-oauth upstream failure should fail");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(rendered.contains("non-oauth error response"));
        assert!(rendered.contains("502"));
        assert!(!rendered.contains("oauth-code"));
    }

    #[tokio::test]
    async fn token_exchange_redacts_secrets_reflected_in_unstructured_error() {
        let (token_url, _token_server) = token_server(
            StatusCode::BAD_GATEWAY,
            "upstream reflected client-secret and oauth-code",
        )
        .await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("reflected unstructured failure should fail safely");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(rendered.contains("upstream reflected"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("oauth-code"));
    }

    #[tokio::test]
    async fn token_exchange_rejects_oversized_error_response_body() {
        let oversized_body = format!(
            "{}client-secret oauth-code",
            "x".repeat(super::MAX_GITHUB_OAUTH_TOKEN_RESPONSE_BODY_LEN)
        );
        let (token_url, _token_server) =
            token_server(StatusCode::BAD_GATEWAY, oversized_body).await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("oversized token error response should be rejected");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(rendered.contains("exceeded"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("oauth-code"));
    }

    #[tokio::test]
    async fn token_exchange_rejects_oversized_success_response_body() {
        let oversized_body = format!(
            r#"{{"access_token":"{}","token_type":"bearer"}}"#,
            "x".repeat(super::MAX_GITHUB_OAUTH_TOKEN_RESPONSE_BODY_LEN)
        );
        let (token_url, _token_server) = token_server(StatusCode::OK, oversized_body).await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("oversized token success response should be rejected");

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(error.to_string().contains("exceeded"));
    }

    #[tokio::test]
    async fn token_exchange_maps_successful_github_oauth_error_body() {
        let (token_url, _token_server) = token_server(
            StatusCode::OK,
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
        assert!(!rendered.contains("oauth-code"));
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

    #[tokio::test]
    async fn token_exchange_rejects_unknown_token_type() {
        let (token_url, _token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"mac","scope":"read:user"}"#,
        )
        .await;
        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(token_url).expect("mock URL should parse");
        let callback = validated_callback("oauth-code", "csrf-state");

        let error = exchanger
            .exchange_code(&provider_config(), &callback, REDIRECT_URL)
            .await
            .expect_err("unsupported token type should fail");

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(error.to_string().contains("token_type"));
        assert!(!error.to_string().contains("gho_token"));
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

    #[tokio::test]
    async fn user_client_fetches_authenticated_github_identity() {
        let (api_url, user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":583231}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let user = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect("user lookup should succeed");

        assert_eq!(user.provider_id, "github-main");
        assert_eq!(user.login, "octocat");
        assert_eq!(user.stable_id.as_deref(), Some("583231"));
        let requests = user_server.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.accept.as_deref(), Some(super::GITHUB_API_ACCEPT));
        assert_eq!(request.api_version.as_deref(), Some("2022-11-28"));
        assert_eq!(request.authorization.as_deref(), Some("Bearer gho_token"));
    }

    #[tokio::test]
    async fn user_client_preserves_github_api_base_path() {
        let (api_url, user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":583231}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");

        let user = fetch_authenticated_github_user(&provider, &token)
            .await
            .expect("wrapper user lookup should succeed");

        assert_eq!(user.login, "octocat");
        assert_eq!(user_server.requests.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn user_client_rejects_invalid_api_url_before_network_io() {
        let provider = provider_config_with_api_url("not a url");
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");

        let error = fetch_authenticated_github_user(&provider, &token)
            .await
            .expect_err("invalid api_url should fail");

        assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
        assert!(error.to_string().contains("github api_url"));
    }

    #[tokio::test]
    async fn user_client_maps_bad_token_to_authentication_required() {
        let (api_url, _user_server) = user_server(
            StatusCode::UNAUTHORIZED,
            r#"{"message":"Bad credentials gho_secret"}"#,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_secret").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("unauthorized user lookup should fail");
        let rendered = error.to_string();

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::AuthenticationRequired { .. }
            }
        ));
        assert!(!rendered.contains("gho_secret"));
    }

    #[tokio::test]
    async fn user_client_redacts_token_from_upstream_error_message() {
        let (api_url, _user_server) =
            user_server(StatusCode::BAD_GATEWAY, r#"{"message":"echo gho_secret"}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_secret").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("upstream user lookup should fail");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(rendered.contains("502"));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("gho_secret"));
    }

    #[tokio::test]
    async fn user_client_maps_forbidden_scope_failure_to_authentication_required() {
        let (api_url, _user_server) = user_server(
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible by personal access token"}"#,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_secret").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("forbidden scope failure should fail as authentication");
        let rendered = error.to_string();

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::AuthenticationRequired { .. }
            }
        ));
        assert!(!rendered.contains("gho_secret"));
    }

    #[tokio::test]
    async fn user_client_includes_github_error_details_in_upstream_message() {
        let (api_url, _user_server) = user_server(
            StatusCode::BAD_GATEWAY,
            r#"{"message":"Validation failed","errors":[{"resource":"User","field":"login","code":"invalid"}]}"#,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_secret").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("upstream user lookup should fail");
        let rendered = error.to_string();

        assert!(rendered.contains("Validation failed"));
        assert!(rendered.contains("User.login.invalid"));
    }

    #[tokio::test]
    async fn user_client_caps_large_upstream_error_body() {
        let large_body = "x".repeat(super::MAX_GITHUB_ERROR_BODY_LEN * 2);
        let (api_url, _user_server) = user_server(StatusCode::BAD_GATEWAY, large_body).await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_secret").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("upstream user lookup should fail");
        let rendered = error.to_string();

        assert!(rendered.contains("github authenticated user request failed"));
        assert!(rendered.len() < 512);
    }

    #[tokio::test]
    async fn user_client_redacts_long_token_before_truncating_upstream_error_message() {
        let token_secret = format!("gho_{}", "a".repeat(240));
        let body = format!(r#"{{"message":"echo {token_secret}"}}"#);
        let (api_url, _user_server) = user_server(StatusCode::BAD_GATEWAY, &body).await;
        let provider = provider_config_with_api_url(api_url);
        let token =
            GitHubOAuthAccessToken::from_secret(token_secret.clone()).expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("upstream user lookup should fail");
        let rendered = error.to_string();

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(&token_secret));
        assert!(!rendered.contains(&token_secret[..200]));
    }

    #[tokio::test]
    async fn user_client_reports_invalid_login_validation_reason() {
        let (api_url, _user_server) =
            user_server(StatusCode::OK, r#"{"login":" bad-login","id":583231}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("invalid login should fail");
        let rendered = error.to_string();

        assert!(rendered.contains("invalid login"));
        assert!(rendered.contains("must not be blank or padded"));
        assert!(!rendered.contains("gho_token"));
    }

    #[tokio::test]
    async fn user_client_rejects_malformed_identity_response() {
        let (api_url, _user_server) = user_server(StatusCode::OK, r#"{"id":583231}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("missing login should fail");

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(error.to_string().contains("missing login"));
        assert!(!error.to_string().contains("gho_token"));
    }

    #[tokio::test]
    async fn user_client_rejects_identity_without_stable_id() {
        let (api_url, _user_server) = user_server(StatusCode::OK, r#"{"login":"octocat"}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let client = GitHubUserClient::new().expect("client should build");

        let error = client
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("missing stable user ID should fail");

        assert!(matches!(error, ServerError::RepositoryProvider { .. }));
        assert!(error.to_string().contains("missing id"));
        assert!(!error.to_string().contains("gho_token"));
    }

    #[tokio::test]
    async fn repository_provider_uses_authentication_context_for_public_read() {
        let (api_url, permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"read","role_name":"triage","user":{"login":"octocat","id":583231}}"#,
            None,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let repository = repository_identity("public-owner", "public-repo");
        let user = repository_user("octocat");
        let client = GitHubRepositoryPermissionClient::new().expect("client should build");
        let provider_adapter = GitHubRepositoryProvider::with_client(provider, client);
        let authentication = RepositoryAuthentication::new(user, "gho_token");

        let authorization = provider_adapter
            .check_permission(&repository, &authentication, RepositoryPermission::Read)
            .await
            .expect("read permission should authorize download");

        assert_eq!(authorization.required, RepositoryPermission::Read);
        assert_eq!(authorization.granted, RepositoryPermission::Read);
        let requests = permission_server.requests.lock().await;
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        assert_eq!(request.owner, "public-owner");
        assert_eq!(request.repo, "public-repo");
        assert_eq!(request.username, "octocat");
        assert_eq!(request.accept.as_deref(), Some(super::GITHUB_API_ACCEPT));
        assert_eq!(request.api_version.as_deref(), Some("2022-11-28"));
        assert_eq!(request.authorization.as_deref(), Some("Bearer gho_token"));
        let identity_requests = permission_server.identity_requests.lock().await;
        assert_eq!(identity_requests.len(), 1);
        let identity_request = &identity_requests[0];
        assert_eq!(
            identity_request.accept.as_deref(),
            Some(super::GITHUB_API_ACCEPT)
        );
        assert_eq!(identity_request.api_version.as_deref(), Some("2022-11-28"));
        assert_eq!(
            identity_request.authorization.as_deref(),
            Some("Bearer gho_token")
        );
        assert_eq!(provider_adapter.provider_id(), "github-main");
    }

    #[tokio::test]
    async fn permission_client_denies_reused_repository_name_with_a_different_stable_id() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"admin","role_name":"admin"}"#,
            None,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let mut repository = repository_identity("renamed-owner", "reused-repo");
        repository.stable_id = Some("9999999".to_owned());
        let user = repository_user("octocat");
        let client = GitHubRepositoryPermissionClient::new().expect("client should build");

        let error = client
            .check_permission(
                &provider,
                &token,
                &repository,
                &user,
                RepositoryPermission::Read,
            )
            .await
            .expect_err("a reused repository name must not authorize the replacement repo");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::RepositoryNotFound { .. }
            }
        ));
    }

    #[tokio::test]
    async fn permission_client_denies_reused_login_with_a_different_stable_id() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"admin","role_name":"admin","user":{"login":"octocat","id":999999}}"#,
            None,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let repository = repository_identity("private-owner", "private-repo");
        let user = repository_user("octocat");
        let client = GitHubRepositoryPermissionClient::new().expect("client should build");

        let error = client
            .check_permission(
                &provider,
                &token,
                &repository,
                &user,
                RepositoryPermission::Read,
            )
            .await
            .expect_err("a reused login must not authorize a different stable user");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::PermissionDenied { .. }
            }
        ));
    }

    #[tokio::test]
    async fn permission_client_rejects_grant_without_stable_user_identity() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"admin","role_name":"admin"}"#,
            None,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let repository = repository_identity("private-owner", "private-repo");
        let user = repository_user("octocat");
        let client = GitHubRepositoryPermissionClient::new().expect("client should build");

        let error = client
            .check_permission(
                &provider,
                &token,
                &repository,
                &user,
                RepositoryPermission::Read,
            )
            .await
            .expect_err("a granted permission must include stable user identity");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::Upstream { .. }
            }
        ));
        assert!(error.to_string().contains("permission user identity"));
    }

    #[tokio::test]
    async fn permission_client_allows_private_write_repo_upload() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"write","role_name":"write","user":{"login":"octocat","id":583231}}"#,
            None,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let repository = repository_identity("private-owner", "private-repo");
        let user = repository_user("octocat");
        let client = GitHubRepositoryPermissionClient::new().expect("client should build");

        let authorization = client
            .check_permission(
                &provider,
                &token,
                &repository,
                &user,
                RepositoryPermission::Write,
            )
            .await
            .expect("write permission should authorize upload");

        assert_eq!(authorization.granted, RepositoryPermission::Write);
    }

    #[tokio::test]
    async fn permission_client_allows_org_admin_repo_upload() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"admin","role_name":"admin","user":{"login":"octocat","id":583231}}"#,
            None,
        )
        .await;
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let repository = repository_identity("org-name", "org-repo");
        let user = repository_user("octocat");
        let client = GitHubRepositoryPermissionClient::new().expect("client should build");

        let authorization = client
            .check_permission(
                &provider,
                &token,
                &repository,
                &user,
                RepositoryPermission::Write,
            )
            .await
            .expect("admin permission should authorize upload");

        assert_eq!(authorization.granted, RepositoryPermission::Admin);
    }

    #[tokio::test]
    async fn permission_client_denies_read_only_repo_upload() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"read","role_name":"read"}"#,
            None,
        )
        .await;
        let error = mocked_permission_check(
            api_url,
            "private-owner",
            "read-only-repo",
            RepositoryPermission::Write,
        )
        .await
        .expect_err("read-only permission should not authorize upload");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::PermissionDenied { .. }
            }
        ));
    }

    #[tokio::test]
    async fn permission_client_denies_none_permission() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"none","role_name":"none"}"#,
            None,
        )
        .await;
        let error = mocked_permission_check(
            api_url,
            "private-owner",
            "denied-repo",
            RepositoryPermission::Read,
        )
        .await
        .expect_err("none permission should deny download");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::PermissionDenied { .. }
            }
        ));
    }

    #[tokio::test]
    async fn permission_client_denies_404_as_permission_denied() {
        let (api_url, _permission_server) =
            permission_server(StatusCode::NOT_FOUND, r#"{"message":"Not Found"}"#, None).await;
        let error = mocked_permission_check(
            api_url,
            "private-owner",
            "missing-or-denied-repo",
            RepositoryPermission::Read,
        )
        .await
        .expect_err("404 should deny without exposing repository existence");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::PermissionDenied { .. }
            }
        ));
        assert!(!matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::RepositoryNotFound { .. }
            }
        ));
    }

    #[tokio::test]
    async fn permission_client_maps_sso_header_to_sso_required() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource protected by organization SAML enforcement"}"#,
            Some("required; url=https://github.com/orgs/org-name/sso?authorization_request=1"),
        )
        .await;
        let error =
            mocked_permission_check(api_url, "org-name", "sso-repo", RepositoryPermission::Read)
                .await
                .expect_err("SSO-required response should deny access");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::SsoRequired { .. }
            }
        ));
    }

    #[tokio::test]
    async fn permission_client_maps_unauthorized_to_authentication_required() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::UNAUTHORIZED,
            r#"{"message":"Bad credentials gho_secret"}"#,
            None,
        )
        .await;
        let error = mocked_permission_check(
            api_url,
            "org-name",
            "private-repo",
            RepositoryPermission::Read,
        )
        .await
        .expect_err("unauthorized permission check should require authentication");
        let rendered = error.to_string();

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::AuthenticationRequired { .. }
            }
        ));
        assert!(!rendered.contains("gho_secret"));
    }

    #[tokio::test]
    async fn permission_client_maps_forbidden_scope_failure_to_authentication_required() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::FORBIDDEN,
            r#"{"message":"Resource not accessible by personal access token gho_secret"}"#,
            None,
        )
        .await;
        let error = mocked_permission_check(
            api_url,
            "org-name",
            "private-repo",
            RepositoryPermission::Read,
        )
        .await
        .expect_err("auth-related forbidden response should require authentication");
        let rendered = error.to_string();

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::AuthenticationRequired { .. }
            }
        ));
        assert!(!rendered.contains("gho_secret"));
    }

    #[tokio::test]
    async fn permission_client_keeps_generic_forbidden_response_as_upstream_error() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::FORBIDDEN,
            r#"{"message":"secondary rate limit rejected gho_token"}"#,
            None,
        )
        .await;
        let error = mocked_permission_check(
            api_url,
            "org-name",
            "private-repo",
            RepositoryPermission::Read,
        )
        .await
        .expect_err("generic forbidden response should stay upstream");
        let rendered = error.to_string();

        match error {
            ServerError::RepositoryProvider {
                source:
                    RepositoryProviderError::Upstream {
                        status: Some(403), ..
                    },
            } => {}
            other => panic!("expected upstream forbidden response, got {other:?}"),
        }
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains("gho_token"));
    }

    #[tokio::test]
    async fn permission_client_denies_unknown_permission_state() {
        let (api_url, _permission_server) = permission_server(
            StatusCode::OK,
            r#"{"permission":"custom-role","role_name":"custom-role"}"#,
            None,
        )
        .await;
        let error = mocked_permission_check(
            api_url,
            "org-name",
            "custom-role-repo",
            RepositoryPermission::Read,
        )
        .await
        .expect_err("unknown permission state should deny access");

        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::PermissionDenied { .. }
            }
        ));
    }

    #[tokio::test]
    async fn permission_client_rejects_empty_repository_path_segments() {
        let error = mocked_permission_check(
            "https://api.github.com".to_owned(),
            "",
            "private-repo",
            RepositoryPermission::Read,
        )
        .await
        .expect_err("empty repository owner should fail before building the GitHub URL");

        assert!(matches!(error, ServerError::InvalidRequest { .. }));
        assert!(error.to_string().contains("repository owner"));
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
    fn token_response_scopes_are_split_on_github_comma_delimiter() {
        assert_eq!(
            super::parse_scope_list("read:user, repo,workflow,,"),
            vec!["read:user", "repo", "workflow"]
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
    fn oauth_redirect_requires_protected_transport_or_explicit_opt_in() {
        let error = GitHubOAuthAuthorization::new(
            &provider_config(),
            "http://lfs.example.com/auth/github/callback",
        )
        .expect_err("remote plaintext callback should be rejected");
        assert!(error.to_string().contains("must use HTTPS"));

        let mut development_provider = provider_config();
        development_provider.allow_insecure_http = true;
        GitHubOAuthAuthorization::new(
            &development_provider,
            "http://192.168.1.25:8080/auth/github/callback",
        )
        .expect("explicit development opt-in should allow trusted LAN callback URL");
    }

    #[tokio::test]
    async fn default_oauth_client_does_not_follow_token_endpoint_redirects() {
        let redirected = Arc::new(AtomicBool::new(false));
        let redirected_for_handler = Arc::clone(&redirected);
        let app = Router::new()
            .route(
                "/token",
                post(|| async { (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, "/sink")]) }),
            )
            .route(
                "/sink",
                post(move || {
                    let redirected = Arc::clone(&redirected_for_handler);
                    async move {
                        redirected.store(true, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "access_token": "redirected-token",
                            "token_type": "bearer"
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("redirect server should bind");
        let address = listener
            .local_addr()
            .expect("redirect server address should be available");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("redirect server should run");
        });

        let exchanger =
            GitHubOAuthTokenExchanger::with_token_url(format!("http://{address}/token"))
                .expect("loopback token URL should be accepted");
        let error = exchanger
            .exchange_code(
                &provider_config(),
                &validated_callback("oauth-code", "csrf-state"),
                REDIRECT_URL,
            )
            .await
            .expect_err("redirect response must not be followed");

        assert!(!redirected.load(Ordering::SeqCst));
        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::Upstream {
                    status: Some(307),
                    ..
                }
            }
        ));
    }

    #[tokio::test]
    async fn default_api_client_does_not_follow_token_bearing_redirects() {
        let redirected = Arc::new(AtomicBool::new(false));
        let redirected_for_handler = Arc::clone(&redirected);
        let app = Router::new()
            .route(
                "/api/v3/user",
                get(|| async { (StatusCode::TEMPORARY_REDIRECT, [(LOCATION, "/sink")]) }),
            )
            .route(
                "/sink",
                get(move || {
                    let redirected = Arc::clone(&redirected_for_handler);
                    async move {
                        redirected.store(true, Ordering::SeqCst);
                        Json(serde_json::json!({ "login": "octocat", "id": 1 }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("redirect server should bind");
        let address = listener
            .local_addr()
            .expect("redirect server address should be available");
        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("redirect server should run");
        });

        let provider = provider_config_with_api_url(format!("http://{address}/api/v3"));
        let token = GitHubOAuthAccessToken::from_secret("oauth-token")
            .expect("OAuth token fixture should be valid");
        let error = GitHubUserClient::new()
            .expect("default API client should build")
            .fetch_authenticated_user(&provider, &token)
            .await
            .expect_err("token-bearing API redirect must not be followed");

        assert!(!redirected.load(Ordering::SeqCst));
        assert!(matches!(
            error,
            ServerError::RepositoryProvider {
                source: RepositoryProviderError::Upstream {
                    status: Some(307),
                    ..
                }
            }
        ));
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
        let pkce_verifier = authorization.pkce_verifier.secret().to_owned();

        let rendered = format!("{authorization:?}");

        assert!(rendered.contains("GitHubOAuthAuthorization"));
        assert!(!rendered.contains("csrf-state"));
        assert!(!rendered.contains(&pkce_verifier));
    }

    #[test]
    fn callback_route_state_debug_redacts_provider_secret_and_pending_states() {
        let csrf_states = GitHubOAuthStateRegistry::with_state(
            GitHubOAuthState::from_secret("csrf-state").expect("state should parse"),
        );
        let route_state = GitHubOAuthCallbackRouteState::with_clients(
            provider_config(),
            csrf_states,
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::with_token_url(
                "http://127.0.0.1:9/login/oauth/access_token",
            )
            .expect("token exchanger should build"),
            GitHubUserClient::new().expect("user client should build"),
        )
        .expect("callback route state should build");

        let rendered = format!("{route_state:?}");

        assert!(rendered.contains("GitHubOAuthCallbackRouteState"));
        assert!(rendered.contains("github-main"));
        assert!(!rendered.contains("client-secret"));
        assert!(!rendered.contains("csrf-state"));
        assert!(!rendered.contains("login/oauth/access_token"));
    }

    #[test]
    fn state_registry_rejects_overload_without_evicting_active_states() {
        let registry = GitHubOAuthStateRegistry::new();
        let active_state =
            GitHubOAuthState::from_secret("active-state").expect("state should parse");
        registry
            .register_state(active_state.clone())
            .expect("first state should register");

        for index in 1..MAX_PENDING_GITHUB_OAUTH_STATES {
            registry
                .register_state(
                    GitHubOAuthState::from_secret(format!("csrf-state-{index}"))
                        .expect("state should parse"),
                )
                .expect("state should register before capacity");
        }

        assert_eq!(registry.len(), MAX_PENDING_GITHUB_OAUTH_STATES);
        let error = registry
            .register_state(
                GitHubOAuthState::from_secret("unrelated-overload-state")
                    .expect("state should parse"),
            )
            .expect_err("capacity must reject an unrelated login attempt");
        assert!(matches!(error, ServerError::RateLimited { .. }));
        assert_eq!(registry.len(), MAX_PENDING_GITHUB_OAUTH_STATES);
        assert!(registry.consume(&active_state).is_some());
    }

    #[test]
    fn state_registry_expires_states_at_the_exact_ttl_boundary() {
        let elapsed_seconds = Arc::new(AtomicU64::new(0));
        let base = Instant::now();
        let clock = {
            let elapsed_seconds = Arc::clone(&elapsed_seconds);
            Arc::new(move || base + Duration::from_secs(elapsed_seconds.load(Ordering::SeqCst)))
        };
        let registry = GitHubOAuthStateRegistry::with_clock(clock);

        registry
            .register_state(
                GitHubOAuthState::from_secret("before-expiry").expect("state should parse"),
            )
            .expect("state should register");
        elapsed_seconds.store(GITHUB_OAUTH_STATE_TTL.as_secs() - 1, Ordering::SeqCst);
        assert_eq!(registry.len(), 1);

        elapsed_seconds.store(GITHUB_OAUTH_STATE_TTL.as_secs(), Ordering::SeqCst);
        assert_eq!(registry.len(), 0);

        elapsed_seconds.store(GITHUB_OAUTH_STATE_TTL.as_secs() + 1, Ordering::SeqCst);
        assert_eq!(registry.len(), 0);

        registry
            .register_state(
                GitHubOAuthState::from_secret("after-expiry").expect("state should parse"),
            )
            .expect("capacity should be reusable after expiry");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn state_registry_consumes_only_exact_digest_key() {
        let registry = GitHubOAuthStateRegistry::new();
        registry
            .register_state(
                GitHubOAuthState::from_secret("csrf-state-alpha").expect("state should parse"),
            )
            .expect("state should register");
        registry
            .register_state(
                GitHubOAuthState::from_secret("csrf-state-beta").expect("state should parse"),
            )
            .expect("state should register");

        assert!(
            registry
                .consume(
                    &GitHubOAuthState::from_secret("csrf-state-alphb").expect("state should parse")
                )
                .is_none()
        );
        assert_eq!(registry.len(), 2);
        assert!(
            registry
                .consume(
                    &GitHubOAuthState::from_secret("csrf-state-alpha").expect("state should parse")
                )
                .is_some()
        );
        assert_eq!(registry.len(), 1);
        assert!(
            registry
                .consume(
                    &GitHubOAuthState::from_secret("csrf-state-alpha").expect("state should parse")
                )
                .is_none()
        );
    }

    #[test]
    fn callback_path_matches_authorization_redirect_examples() {
        assert_eq!(GITHUB_OAUTH_CALLBACK_PATH, "/auth/github/callback");
        assert!(REDIRECT_URL.ends_with(GITHUB_OAUTH_CALLBACK_PATH));
    }

    #[tokio::test]
    async fn callback_route_exchanges_code_and_issues_lfs_token_without_github_token() {
        let (token_url, token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user,repo"}"#,
        )
        .await;
        let (api_url, user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":42}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let csrf_states = GitHubOAuthStateRegistry::with_state(
            GitHubOAuthState::from_secret("csrf-state").expect("state should parse"),
        );
        let session_store = LocalLfsSessionStore::new();
        let route_state = GitHubOAuthCallbackRouteState::with_clients_and_session_store(
            provider,
            csrf_states,
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::with_token_url(token_url)
                .expect("mock token URL should parse"),
            GitHubUserClient::new().expect("user client should build"),
            session_store.clone(),
        )
        .expect("callback route state should build");
        let callback_server = callback_server(route_state).await;

        let response = reqwest::Client::new()
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?code=oauth-code&state=csrf-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("callback request should complete");

        assert_eq!(response.status(), StatusCode::OK);
        assert_oauth_callback_response_is_protected(response.headers());
        let body = response.text().await.expect("response body should read");
        assert!(!body.contains("gho_token"));
        assert!(!body.contains("oauth-code"));
        assert!(!body.contains("csrf-state"));
        let body: serde_json::Value =
            serde_json::from_str(&body).expect("callback response should be JSON");
        assert_eq!(body["provider_id"], "github-main");
        assert_eq!(body["login"], "octocat");
        assert_eq!(body["stable_id"], "42");
        assert_eq!(
            body["granted_scopes"],
            serde_json::json!(["read:user", "repo"])
        );
        let lfs_token = body["lfs_token"]
            .as_str()
            .expect("callback should return local lfs token");
        assert!(!lfs_token.is_empty());
        let token = LfsSessionToken::from_secret(lfs_token).expect("lfs token should validate");
        let metadata = session_store
            .verify(&token)
            .expect("callback should store issued lfs session metadata");
        let record = session_store
            .verify_record(&token)
            .expect("callback should retain the GitHub token server-side");
        assert_eq!(metadata.provider_id, "github-main");
        assert_eq!(metadata.login, "octocat");
        assert_eq!(metadata.stable_id.as_deref(), Some("42"));
        assert_eq!(metadata.granted_scopes, vec!["read:user", "repo"]);
        assert_eq!(
            record
                .github_access_token()
                .expect("github token should be retained")
                .as_str(),
            "gho_token"
        );
        assert!(!format!("{record:?}").contains("gho_token"));
        assert!(
            body["lfs_token_expires_at_unix_seconds"]
                .as_u64()
                .expect("expiration should be a timestamp")
                >= metadata.expires_at_unix_seconds()
        );

        let token_requests = token_server.requests.lock().await;
        assert_eq!(token_requests.len(), 1);
        let user_requests = user_server.requests.lock().await;
        assert_eq!(user_requests.len(), 1);
        assert_eq!(
            user_requests[0].authorization.as_deref(),
            Some("Bearer gho_token")
        );
    }

    #[tokio::test]
    async fn login_route_registers_state_for_callback_completion() {
        let (token_url, _token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user,repo"}"#,
        )
        .await;
        let (api_url, _user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":42}"#).await;
        let provider = provider_config_with_api_url(api_url);
        let route_state = GitHubOAuthCallbackRouteState::with_clients_and_session_store(
            provider,
            GitHubOAuthStateRegistry::new(),
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::with_token_url(token_url)
                .expect("mock token URL should parse"),
            GitHubUserClient::new().expect("user client should build"),
            LocalLfsSessionStore::new(),
        )
        .expect("callback route state should build");
        let app = github_oauth_login_router(route_state.clone())
            .merge(github_oauth_callback_router(route_state));

        let login_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(GITHUB_OAUTH_LOGIN_PATH)
                    .body(Body::empty())
                    .expect("login request should build"),
            )
            .await
            .expect("login request should complete");

        assert_eq!(login_response.status(), StatusCode::TEMPORARY_REDIRECT);
        let location = login_response
            .headers()
            .get(axum::http::header::LOCATION)
            .expect("login response should redirect")
            .to_str()
            .expect("redirect location should be valid ASCII");
        let login_url = Url::parse(location).expect("redirect location should parse");
        let state = login_url
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .expect("redirect should contain a csrf state");

        let callback_response = app
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "{GITHUB_OAUTH_CALLBACK_PATH}?code=oauth-code&state={state}"
                    ))
                    .body(Body::empty())
                    .expect("callback request should build"),
            )
            .await
            .expect("callback request should complete");

        assert_eq!(callback_response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_route_returns_too_many_requests_without_evicting_active_state() {
        let csrf_states = GitHubOAuthStateRegistry::new();
        let active_state =
            GitHubOAuthState::from_secret("active-state").expect("state should parse");
        csrf_states
            .register_state(active_state.clone())
            .expect("active state should register");
        for index in 1..MAX_PENDING_GITHUB_OAUTH_STATES {
            csrf_states
                .register_state(
                    GitHubOAuthState::from_secret(format!("csrf-state-{index}"))
                        .expect("state should parse"),
                )
                .expect("state should register before capacity");
        }
        let route_state = GitHubOAuthCallbackRouteState::with_clients(
            provider_config(),
            csrf_states.clone(),
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::new().expect("token exchanger should build"),
            GitHubUserClient::new().expect("user client should build"),
        )
        .expect("callback route state should build");
        let app = github_oauth_login_router(route_state);

        let response = app
            .oneshot(
                Request::builder()
                    .uri(GITHUB_OAUTH_LOGIN_PATH)
                    .body(Body::empty())
                    .expect("login request should build"),
            )
            .await
            .expect("login request should complete");

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(
            response
                .headers()
                .get(RETRY_AFTER)
                .expect("overload response should include Retry-After"),
            GITHUB_OAUTH_STATE_TTL.as_secs().to_string().as_str()
        );
        assert!(csrf_states.consume(&active_state).is_some());
    }

    #[tokio::test]
    async fn callback_route_accepts_multiple_registered_states_once() {
        let (token_url, token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user"}"#,
        )
        .await;
        let (api_url, user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":42}"#).await;
        let csrf_states = GitHubOAuthStateRegistry::new();
        csrf_states
            .register_state(
                GitHubOAuthState::from_secret("first-state").expect("first state should parse"),
            )
            .expect("first state should register");
        csrf_states
            .register_state(
                GitHubOAuthState::from_secret("second-state").expect("second state should parse"),
            )
            .expect("second state should register");
        let route_state = GitHubOAuthCallbackRouteState::with_clients(
            provider_config_with_api_url(api_url),
            csrf_states,
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::with_token_url(token_url)
                .expect("mock token URL should parse"),
            GitHubUserClient::new().expect("user client should build"),
        )
        .expect("callback route state should build");
        let callback_server = callback_server(route_state).await;
        let client = reqwest::Client::new();

        for (code, state) in [
            ("first-code", "first-state"),
            ("second-code", "second-state"),
        ] {
            let response = client
                .get(format!(
                    "{}{GITHUB_OAUTH_CALLBACK_PATH}?code={code}&state={state}",
                    callback_server.url()
                ))
                .send()
                .await
                .expect("callback request should complete");

            assert_eq!(response.status(), StatusCode::OK);
        }

        let replay = client
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?code=replay-code&state=first-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("replay callback request should complete");

        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(token_server.requests.lock().await.len(), 2);
        assert_eq!(user_server.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn callback_route_binds_intercepted_codes_to_the_original_pkce_attempt() {
        let first_authorization = GitHubOAuthAuthorization::with_state(
            &provider_config(),
            REDIRECT_URL,
            DEFAULT_GITHUB_OAUTH_SCOPES.iter().copied(),
            || CsrfToken::new("first-state".to_owned()),
        )
        .expect("first authorization should build");
        let first_verifier = first_authorization.pkce_verifier.secret().to_owned();
        let second_authorization = GitHubOAuthAuthorization::with_state(
            &provider_config(),
            REDIRECT_URL,
            DEFAULT_GITHUB_OAUTH_SCOPES.iter().copied(),
            || CsrfToken::new("second-state".to_owned()),
        )
        .expect("second authorization should build");
        let second_verifier = second_authorization.pkce_verifier.secret().to_owned();
        assert_ne!(first_verifier, second_verifier);

        let expected_verifiers = BTreeMap::from([
            ("first-code".to_owned(), first_verifier),
            ("second-code".to_owned(), second_verifier),
        ]);
        let (token_url, token_server) = pkce_token_server(expected_verifiers).await;
        let (api_url, user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":42}"#).await;
        let csrf_states = GitHubOAuthStateRegistry::new();
        csrf_states
            .register(first_authorization)
            .expect("first authorization should register");
        csrf_states
            .register(second_authorization)
            .expect("second authorization should register");
        let route_state = GitHubOAuthCallbackRouteState::with_clients(
            provider_config_with_api_url(api_url),
            csrf_states,
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::with_token_url(token_url)
                .expect("mock token URL should parse"),
            GitHubUserClient::new().expect("user client should build"),
        )
        .expect("callback route state should build");
        let callback_server = callback_server(route_state).await;
        let client = reqwest::Client::new();

        let intercepted = client
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?code=first-code&state=second-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("intercepted callback should complete");
        assert_eq!(intercepted.status(), StatusCode::UNAUTHORIZED);
        assert!(user_server.requests.lock().await.is_empty());

        let original = client
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?code=first-code&state=first-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("original callback should complete");
        assert_eq!(original.status(), StatusCode::OK);
        assert_eq!(user_server.requests.lock().await.len(), 1);

        let replay = client
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?code=first-code&state=first-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("replayed callback should complete");
        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(token_server.requests.lock().await.len(), 2);
    }

    #[tokio::test]
    async fn callback_route_rejects_csrf_mismatch_before_token_exchange() {
        let (token_url, token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user"}"#,
        )
        .await;
        let (api_url, user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":42}"#).await;
        let csrf_states = GitHubOAuthStateRegistry::with_state(
            GitHubOAuthState::from_secret("expected-state").expect("state should parse"),
        );
        let route_state = GitHubOAuthCallbackRouteState::with_clients(
            provider_config_with_api_url(api_url),
            csrf_states,
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::with_token_url(token_url)
                .expect("mock token URL should parse"),
            GitHubUserClient::new().expect("user client should build"),
        )
        .expect("callback route state should build");
        let callback_server = callback_server(route_state).await;

        let response = reqwest::Client::new()
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?code=oauth-code&state=returned-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("callback request should complete");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_oauth_callback_response_is_protected(response.headers());
        let body = response.text().await.expect("response body should read");
        assert!(!body.contains("oauth-code"));
        assert!(!body.contains("returned-state"));
        assert!(!body.contains("expected-state"));
        assert!(token_server.requests.lock().await.is_empty());
        assert!(user_server.requests.lock().await.is_empty());
    }

    #[tokio::test]
    async fn callback_route_consumes_state_on_github_oauth_error_without_exchange() {
        let (token_url, token_server) = token_server(
            StatusCode::OK,
            r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user"}"#,
        )
        .await;
        let (api_url, user_server) =
            user_server(StatusCode::OK, r#"{"login":"octocat","id":42}"#).await;
        let csrf_states = GitHubOAuthStateRegistry::with_state(
            GitHubOAuthState::from_secret("csrf-state").expect("state should parse"),
        );
        let route_state = GitHubOAuthCallbackRouteState::with_clients(
            provider_config_with_api_url(api_url),
            csrf_states,
            REDIRECT_URL,
            GitHubOAuthTokenExchanger::with_token_url(token_url)
                .expect("mock token URL should parse"),
            GitHubUserClient::new().expect("user client should build"),
        )
        .expect("callback route state should build");
        let callback_server = callback_server(route_state).await;
        let client = reqwest::Client::new();

        let response = client
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?error=access_denied&error_description=User+denied&state=csrf-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("callback request should complete");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body: serde_json::Value = response.json().await.expect("error body should be JSON");
        assert_eq!(body["error"], "unauthorized");
        assert_eq!(body["message"], "GitHub OAuth callback was not authorized.");
        assert!(token_server.requests.lock().await.is_empty());
        assert!(user_server.requests.lock().await.is_empty());

        let replay = client
            .get(format!(
                "{}{GITHUB_OAUTH_CALLBACK_PATH}?code=oauth-code&state=csrf-state",
                callback_server.url()
            ))
            .send()
            .await
            .expect("replay callback request should complete");

        assert_eq!(replay.status(), StatusCode::UNAUTHORIZED);
        assert!(token_server.requests.lock().await.is_empty());
        assert!(user_server.requests.lock().await.is_empty());
    }

    #[test]
    fn callback_validation_accepts_matching_state_and_code() {
        let expected_state =
            GitHubOAuthState::from_secret("csrf-state").expect("state should be valid");
        let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", "csrf-state");

        let callback = GitHubOAuthCallback::validate_state(query, &expected_state)
            .expect("callback should match");

        assert_eq!(callback.code.as_str(), "oauth-code");
        assert_eq!(callback.state.as_str(), "csrf-state");
    }

    #[test]
    fn callback_validation_rejects_state_mismatch_without_echoing_values() {
        let expected_state =
            GitHubOAuthState::from_secret("expected-state").expect("state should be valid");
        let query = GitHubOAuthCallbackQuery::authorization_code("oauth-code", "returned-state");

        let error = GitHubOAuthCallback::validate_state(query, &expected_state).unwrap_err();

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
            let error = GitHubOAuthCallback::validate_state(query, &expected_state).unwrap_err();
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
            let error = GitHubOAuthCallback::validate_state(query, &expected_state).unwrap_err();
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

        let error = GitHubOAuthCallback::validate_state(query, &expected_state).unwrap_err();

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

        let error = GitHubOAuthCallback::validate_state(query, &expected_state).unwrap_err();

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
        let missing_error = GitHubOAuthCallback::validate_state(missing_state, &expected_state)
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
        let mismatch_error = GitHubOAuthCallback::validate_state(mismatched_state, &expected_state)
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

        let callback = GitHubOAuthCallback::validate_state(query.clone(), &expected_state)
            .expect("valid callback");
        let pkce_verifier = callback.pkce_verifier.secret().to_owned();

        let query_debug = format!("{query:?}");
        let callback_debug = format!("{callback:?}");

        assert!(!query_debug.contains("oauth-code"));
        assert!(!query_debug.contains("csrf-state"));
        assert!(!callback_debug.contains("oauth-code"));
        assert!(!callback_debug.contains("csrf-state"));
        assert!(!callback_debug.contains(&pkce_verifier));
    }

    #[derive(Debug, Default)]
    struct TokenServerState {
        status: StatusCode,
        body: String,
        expected_pkce_verifiers: Option<BTreeMap<String, String>>,
        requests: Mutex<Vec<CapturedTokenRequest>>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedTokenRequest {
        accept: Option<String>,
        content_type: Option<String>,
        body: BTreeMap<String, String>,
    }

    #[derive(Debug, Default)]
    struct UserServerState {
        status: StatusCode,
        body: String,
        requests: Mutex<Vec<CapturedUserRequest>>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedUserRequest {
        accept: Option<String>,
        api_version: Option<String>,
        authorization: Option<String>,
    }

    #[derive(Debug, Default)]
    struct PermissionServerState {
        status: StatusCode,
        body: String,
        sso_header: Option<String>,
        identity_requests: Mutex<Vec<CapturedRepositoryIdentityRequest>>,
        requests: Mutex<Vec<CapturedPermissionRequest>>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedRepositoryIdentityRequest {
        accept: Option<String>,
        api_version: Option<String>,
        authorization: Option<String>,
    }

    #[derive(Debug, Eq, PartialEq)]
    struct CapturedPermissionRequest {
        owner: String,
        repo: String,
        username: String,
        accept: Option<String>,
        api_version: Option<String>,
        authorization: Option<String>,
    }

    async fn token_server(
        status: StatusCode,
        body: impl Into<String>,
    ) -> (String, Arc<TokenServerState>) {
        let state = Arc::new(TokenServerState {
            status,
            body: body.into(),
            expected_pkce_verifiers: None,
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

    async fn pkce_token_server(
        expected_pkce_verifiers: BTreeMap<String, String>,
    ) -> (String, Arc<TokenServerState>) {
        let state = Arc::new(TokenServerState {
            status: StatusCode::OK,
            body: r#"{"access_token":"gho_token","token_type":"bearer","scope":"read:user"}"#
                .to_owned(),
            expected_pkce_verifiers: Some(expected_pkce_verifiers),
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/login/oauth/access_token", post(capture_token_request))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock PKCE token server should bind");
        let address = listener
            .local_addr()
            .expect("mock PKCE token server address should be available");

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock PKCE token server should run");
        });

        (format!("http://{address}/login/oauth/access_token"), state)
    }

    async fn user_server(
        status: StatusCode,
        body: impl Into<String>,
    ) -> (String, Arc<UserServerState>) {
        let state = Arc::new(UserServerState {
            status,
            body: body.into(),
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/api/v3/user", get(capture_user_request))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock user server should bind");
        let address = listener
            .local_addr()
            .expect("mock user server address should be available");

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock user server should run");
        });

        (format!("http://{address}/api/v3"), state)
    }

    async fn permission_server(
        status: StatusCode,
        body: impl Into<String>,
        sso_header: Option<&str>,
    ) -> (String, Arc<PermissionServerState>) {
        let state = Arc::new(PermissionServerState {
            status,
            body: body.into(),
            sso_header: sso_header.map(ToOwned::to_owned),
            identity_requests: Mutex::new(Vec::new()),
            requests: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route(
                "/api/v3/repos/{owner}/{repo}",
                get(capture_repository_identity_request),
            )
            .route(
                "/api/v3/repos/{owner}/{repo}/collaborators/{username}/permission",
                get(capture_permission_request),
            )
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock permission server should bind");
        let address = listener
            .local_addr()
            .expect("mock permission server address should be available");

        tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock permission server should run");
        });

        (format!("http://{address}/api/v3"), state)
    }

    struct CallbackServer {
        url: String,
        task: JoinHandle<()>,
    }

    impl CallbackServer {
        fn url(&self) -> &str {
            &self.url
        }
    }

    impl Drop for CallbackServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    async fn callback_server(state: GitHubOAuthCallbackRouteState) -> CallbackServer {
        let app = github_oauth_callback_router(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock callback server should bind");
        let address = listener
            .local_addr()
            .expect("mock callback server address should be available");

        let task = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("mock callback server should run");
        });

        CallbackServer {
            url: format!("http://{address}"),
            task,
        }
    }

    fn assert_oauth_callback_response_is_protected(headers: &HeaderMap) {
        assert_eq!(
            headers.get(CACHE_CONTROL),
            Some(&HeaderValue::from_static("no-store, private"))
        );
        assert_eq!(
            headers.get(PRAGMA),
            Some(&HeaderValue::from_static("no-cache"))
        );
        assert_eq!(
            headers.get(REFERRER_POLICY),
            Some(&HeaderValue::from_static("no-referrer"))
        );
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
        let content_type = headers
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let body: BTreeMap<String, String> = url::form_urlencoded::parse(body.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();

        let pkce_matches = state
            .expected_pkce_verifiers
            .as_ref()
            .is_none_or(|expected| {
                body.get("code")
                    .and_then(|code| expected.get(code))
                    .is_some_and(|verifier| body.get("code_verifier") == Some(verifier))
            });
        state.requests.lock().await.push(CapturedTokenRequest {
            accept,
            content_type,
            body,
        });

        if pkce_matches {
            (
                state.status,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                state.body.clone(),
            )
        } else {
            (
                StatusCode::BAD_REQUEST,
                [(axum::http::header::CONTENT_TYPE, "application/json")],
                r#"{"error":"incorrect_client_credentials"}"#.to_owned(),
            )
        }
    }

    async fn capture_user_request(
        State(state): State<Arc<UserServerState>>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let accept = headers
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let api_version = headers
            .get("x-github-api-version")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let authorization = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        state.requests.lock().await.push(CapturedUserRequest {
            accept,
            api_version,
            authorization,
        });

        (
            state.status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            state.body.clone(),
        )
    }

    async fn capture_repository_identity_request(
        State(state): State<Arc<PermissionServerState>>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let accept = headers
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let api_version = headers
            .get("x-github-api-version")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let authorization = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        state
            .identity_requests
            .lock()
            .await
            .push(CapturedRepositoryIdentityRequest {
                accept,
                api_version,
                authorization,
            });

        Json(serde_json::json!({ "id": 8675309_u64 }))
    }

    async fn capture_permission_request(
        State(state): State<Arc<PermissionServerState>>,
        Path((owner, repo, username)): Path<(String, String, String)>,
        headers: HeaderMap,
    ) -> impl IntoResponse {
        let accept = headers
            .get(axum::http::header::ACCEPT)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let api_version = headers
            .get("x-github-api-version")
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);
        let authorization = headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(ToOwned::to_owned);

        state.requests.lock().await.push(CapturedPermissionRequest {
            owner,
            repo,
            username,
            accept,
            api_version,
            authorization,
        });

        let mut response = (
            state.status,
            [(CONTENT_TYPE, "application/json")],
            state.body.clone(),
        )
            .into_response();
        if let Some(sso_header) = &state.sso_header {
            response.headers_mut().insert(
                super::GITHUB_SSO_HEADER,
                HeaderValue::from_str(sso_header).expect("test SSO header should parse"),
            );
        }

        response
    }

    async fn mocked_permission_check(
        api_url: String,
        owner: &str,
        repo: &str,
        required: RepositoryPermission,
    ) -> Result<crate::RepositoryAuthorization, ServerError> {
        let provider = provider_config_with_api_url(api_url);
        let token = GitHubOAuthAccessToken::from_secret("gho_token").expect("token should parse");
        let repository = repository_identity(owner, repo);
        let user = repository_user("octocat");
        let client = GitHubRepositoryPermissionClient::new().expect("client should build");

        client
            .check_permission(&provider, &token, &repository, &user, required)
            .await
    }

    fn repository_identity(owner: &str, repo: &str) -> RepositoryIdentity {
        RepositoryIdentity {
            provider_id: "github-main".to_owned(),
            stable_id: Some("8675309".to_owned()),
            host: "github.com".to_owned(),
            owner: owner.to_owned(),
            name: repo.to_owned(),
        }
    }

    fn repository_user(login: &str) -> RepositoryUser {
        RepositoryUser::new("github-main", login, Some("583231".to_owned()))
    }

    fn validated_callback(code: &str, state: &str) -> GitHubOAuthCallback {
        let expected_state = GitHubOAuthState::from_secret(state).expect("state should be valid");
        let query = GitHubOAuthCallbackQuery::authorization_code(code, state);

        GitHubOAuthCallback::validate_state(query, &expected_state)
            .expect("callback should validate")
    }

    fn query_pairs(authorization: &GitHubOAuthAuthorization) -> BTreeMap<String, String> {
        authorization
            .authorization_url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }
}
