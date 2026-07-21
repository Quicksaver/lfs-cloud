//! GitHub personal-access-token authentication and repository authorization.
//!
//! LFS Cloud accepts one configured GitHub PAT, exchanges it for a short-lived
//! local LFS session, and retains the PAT only in protected server-side session
//! state for identity and repository-permission checks.

use std::{fmt, sync::OnceLock, time::Duration};

use axum::{
    Json, Router,
    extract::State,
    http::{
        HeaderMap, StatusCode as AxumStatusCode,
        header::{AUTHORIZATION, HeaderValue, RETRY_AFTER},
    },
    middleware,
    response::{IntoResponse, Response},
    routing::post,
};
use reqwest::{Client, RequestBuilder, StatusCode, header::ACCEPT, redirect::Policy};
use serde::{Deserialize, Serialize};
use url::{ParseError as UrlParseError, Url};

use crate::{
    GitHubProviderConfig, IssuedLfsSession, LocalLfsSessionStore, ProviderFuture,
    RepositoryAuthentication, RepositoryAuthorization, RepositoryIdentity, RepositoryPermission,
    RepositoryProvider, RepositoryProviderError, RepositoryUser, SanitizedMessage, ServerError,
    ServerResult, http_transport::uses_protected_http_transport,
};

const MAX_GITHUB_SENSITIVE_VALUE_LEN: usize = 1024;
const MAX_GITHUB_ERROR_BODY_LEN: usize = 16 * 1024;
const GITHUB_USER_FETCH_TIMEOUT: Duration = Duration::from_secs(30);
const GITHUB_PERMISSION_CHECK_TIMEOUT: Duration = Duration::from_secs(30);
const GITHUB_USER_AGENT: &str = concat!("lfscloud/", env!("CARGO_PKG_VERSION"));
const GITHUB_API_ACCEPT: &str = "application/vnd.github+json";
const GITHUB_API_VERSION_HEADER: &str = "x-github-api-version";
const GITHUB_API_VERSION: &str = "2022-11-28";
const GITHUB_SSO_HEADER: &str = "x-github-sso";

static DEFAULT_GITHUB_API_HTTP_CLIENT: OnceLock<Result<Client, String>> = OnceLock::new();

/// Login path that exchanges the configured GitHub personal access token for a
/// short-lived local LFS Cloud session.
pub const GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH: &str = "/auth/github/pat";

/// Route state for GitHub personal-access-token login.
#[derive(Clone)]
pub struct GitHubPersonalAccessTokenLoginRouteState {
    provider: GitHubProviderConfig,
    configured_token: GitHubPersonalAccessToken,
    user_client: GitHubUserClient,
    session_store: LocalLfsSessionStore,
}

impl GitHubPersonalAccessTokenLoginRouteState {
    /// Creates PAT login state with an explicit GitHub user client and shared
    /// local-session store.
    ///
    /// # Errors
    ///
    /// Returns ServerError when the configured PAT is malformed.
    pub fn with_client_and_session_store(
        provider: GitHubProviderConfig,
        user_client: GitHubUserClient,
        session_store: LocalLfsSessionStore,
    ) -> ServerResult<Self> {
        let configured_token = GitHubPersonalAccessToken::from_secret(
            provider.authentication.personal_access_token().to_owned(),
        )?;

        Ok(Self {
            provider,
            configured_token,
            user_client,
            session_store,
        })
    }
}

impl fmt::Debug for GitHubPersonalAccessTokenLoginRouteState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubPersonalAccessTokenLoginRouteState")
            .field("provider_id", &self.provider.id)
            .field("configured_token", &"<redacted>")
            .field("user_client", &"<redacted>")
            .field("session_store", &self.session_store)
            .finish()
    }
}

/// Response returned after a PAT is exchanged for a local LFS session.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct GitHubLoginRouteResponse {
    /// Configured repository provider that authenticated the user.
    pub provider_id: String,
    /// Authenticated GitHub login.
    pub login: String,
    /// Stable numeric GitHub user ID.
    pub stable_id: Option<String>,
    /// Opaque local LFS Cloud token to store for the Git LFS URL.
    pub lfs_token: String,
    /// Expiration time for the local token, as seconds since the Unix epoch.
    pub lfs_token_expires_at_unix_seconds: u64,
}

impl GitHubLoginRouteResponse {
    fn new(
        provider: &GitHubProviderConfig,
        user: RepositoryUser,
        session: IssuedLfsSession,
    ) -> Self {
        Self {
            provider_id: provider.id.clone(),
            login: user.login,
            stable_id: user.stable_id,
            lfs_token: session.token.as_str().to_owned(),
            lfs_token_expires_at_unix_seconds: session.metadata.expires_at_unix_seconds(),
        }
    }
}

impl fmt::Debug for GitHubLoginRouteResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GitHubLoginRouteResponse")
            .field("provider_id", &self.provider_id)
            .field("login", &self.login)
            .field("stable_id", &self.stable_id)
            .field("lfs_token", &"<redacted>")
            .field(
                "lfs_token_expires_at_unix_seconds",
                &self.lfs_token_expires_at_unix_seconds,
            )
            .finish()
    }
}

#[derive(Serialize)]
struct GitHubLoginRouteErrorBody {
    error: &'static str,
    message: &'static str,
}

/// Creates the PAT-to-local-session login router.
pub fn github_personal_access_token_login_router(
    state: GitHubPersonalAccessTokenLoginRouteState,
) -> Router {
    Router::new()
        .route(
            GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH,
            post(github_personal_access_token_login_route),
        )
        .layer(middleware::map_response(protect_github_login_response))
        .with_state(state)
}

async fn github_personal_access_token_login_route(
    State(state): State<GitHubPersonalAccessTokenLoginRouteState>,
    headers: HeaderMap,
) -> Result<Json<GitHubLoginRouteResponse>, GitHubPersonalAccessTokenLoginRouteError> {
    let presented_token = bearer_token(&headers).ok_or_else(personal_access_token_denied)?;
    if !constant_time_str_eq(presented_token, state.configured_token.as_str()) {
        return Err(personal_access_token_denied());
    }

    let user = state
        .user_client
        .fetch_authenticated_user(&state.provider, &state.configured_token)
        .await?;
    let session = state.session_store.issue_session_with_github_pat(
        &user,
        ["personal_access_token"],
        state.configured_token.clone(),
    )?;

    Ok(Json(GitHubLoginRouteResponse::new(
        &state.provider,
        user,
        session,
    )))
}

async fn protect_github_login_response(mut response: Response) -> Response {
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

fn bearer_token(headers: &HeaderMap) -> Option<&str> {
    let authorization = headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = authorization.split_once(' ')?;
    (scheme.eq_ignore_ascii_case("Bearer") && !token.is_empty()).then_some(token)
}

fn personal_access_token_denied() -> GitHubPersonalAccessTokenLoginRouteError {
    GitHubPersonalAccessTokenLoginRouteError(ServerError::Unauthorized {
        reason: "GitHub personal access token did not match the configured account".to_owned(),
    })
}

struct GitHubPersonalAccessTokenLoginRouteError(ServerError);

impl From<ServerError> for GitHubPersonalAccessTokenLoginRouteError {
    fn from(error: ServerError) -> Self {
        Self(error)
    }
}

impl IntoResponse for GitHubPersonalAccessTokenLoginRouteError {
    fn into_response(self) -> Response {
        let (status, retry_after_seconds) = match self.0 {
            ServerError::RateLimited {
                retry_after_seconds,
            } => (AxumStatusCode::TOO_MANY_REQUESTS, Some(retry_after_seconds)),
            ServerError::Unauthorized { .. }
            | ServerError::RepositoryProvider {
                source:
                    RepositoryProviderError::AuthenticationRequired { .. }
                    | RepositoryProviderError::PermissionDenied { .. }
                    | RepositoryProviderError::SsoRequired { .. },
            } => (AxumStatusCode::UNAUTHORIZED, None),
            _ => (AxumStatusCode::BAD_GATEWAY, None),
        };
        let body = GitHubLoginRouteErrorBody {
            error: if status == AxumStatusCode::UNAUTHORIZED {
                "unauthorized"
            } else {
                "github_authentication_failed"
            },
            message: "GitHub personal-access-token login could not be completed.",
        };
        let mut response = (status, Json(body)).into_response();
        if let Some(retry_after_seconds) = retry_after_seconds {
            let retry_after = HeaderValue::from_str(&retry_after_seconds.to_string())
                .expect("an integer Retry-After value should be a valid HTTP header");
            response.headers_mut().insert(RETRY_AFTER, retry_after);
        }
        response
    }
}

/// Validated GitHub personal access token used only for server-side API calls.
#[derive(Clone, Eq, PartialEq)]
pub struct GitHubPersonalAccessToken(String);

impl GitHubPersonalAccessToken {
    /// Restores a configured GitHub PAT secret.
    ///
    /// # Errors
    ///
    /// Returns ServerError when the token is blank, padded, too long, or
    /// contains whitespace or control characters.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::GitHubPersonalAccessToken;
    ///
    /// let token = GitHubPersonalAccessToken::from_secret("github_pat_example")?;
    /// assert_eq!(token.as_str(), "github_pat_example");
    /// # Ok::<(), lfscloud::ServerError>(())
    /// ```
    pub fn from_secret(secret: impl Into<String>) -> ServerResult<Self> {
        validate_sensitive_github_value(secret.into(), "github personal access token").map(Self)
    }

    /// Returns the raw PAT for GitHub API requests.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for GitHubPersonalAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("GitHubPersonalAccessToken(<redacted>)")
    }
}
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
    /// use lfscloud::GitHubUserClient;
    ///
    /// let client = GitHubUserClient::new()?;
    ///
    /// assert!(format!("{client:?}").contains("GitHubUserClient"));
    /// # Ok::<(), lfscloud::ServerError>(())
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

    /// Fetches the authenticated GitHub user's identity with a PAT.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the configured GitHub API URL is invalid,
    /// the token is rejected, the request fails, or GitHub returns malformed
    /// identity JSON.
    pub async fn fetch_authenticated_user(
        &self,
        provider: &GitHubProviderConfig,
        token: &GitHubPersonalAccessToken,
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
                GitHubPersonalAccessToken::from_secret(authentication.access_token().to_owned())?;
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
    /// use lfscloud::GitHubRepositoryPermissionClient;
    ///
    /// let client = GitHubRepositoryPermissionClient::new()?;
    ///
    /// assert!(format!("{client:?}").contains("GitHubRepositoryPermissionClient"));
    /// # Ok::<(), lfscloud::ServerError>(())
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
        token: &GitHubPersonalAccessToken,
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
                    permission = %sanitize_github_diagnostic_value(&permission),
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
        token: &GitHubPersonalAccessToken,
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

/// Fetches the authenticated GitHub user using the default GitHub API client.
///
/// # Errors
///
/// Returns ServerError when the GitHub user lookup cannot complete.
pub async fn fetch_authenticated_github_user(
    provider: &GitHubProviderConfig,
    token: &GitHubPersonalAccessToken,
) -> ServerResult<RepositoryUser> {
    GitHubUserClient::new()?
        .fetch_authenticated_user(provider, token)
        .await
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
        let login = validate_sensitive_github_value(login, "github authenticated user login")
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
    token: &GitHubPersonalAccessToken,
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
    token: &GitHubPersonalAccessToken,
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

fn repository_provider_upstream_error(
    provider: &GitHubProviderConfig,
    status: Option<u16>,
    message: &str,
) -> ServerError {
    ServerError::RepositoryProvider {
        source: RepositoryProviderError::Upstream {
            provider: provider.id.clone(),
            status,
            message: SanitizedMessage::new(sanitize_github_diagnostic_value(message)),
        },
    }
}

fn invalid_github_url(path: &str, source: UrlParseError) -> ServerError {
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
        .map_err(|source| invalid_github_url("github api_url", source))?;
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
        .map_err(|source| invalid_github_url("github api_url", source))?;
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
        .map_err(|source| invalid_github_url("github api_url", source))?;
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

fn default_github_api_http_client() -> ServerResult<Client> {
    match DEFAULT_GITHUB_API_HTTP_CLIENT.get_or_init(|| {
        build_github_api_http_client()
            .map_err(|source| sanitize_github_diagnostic_value(&source.to_string()))
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

fn build_github_api_http_client() -> Result<Client, reqwest::Error> {
    Client::builder()
        .user_agent(GITHUB_USER_AGENT)
        .redirect(Policy::none())
        .build()
}

fn validate_sensitive_github_value(value: String, label: &str) -> ServerResult<String> {
    if value.len() > MAX_GITHUB_SENSITIVE_VALUE_LEN {
        return Err(ServerError::InvalidRequest {
            message: format!("{label} must not exceed {MAX_GITHUB_SENSITIVE_VALUE_LEN} bytes"),
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
fn constant_time_str_eq(candidate: &str, expected: &str) -> bool {
    let candidate = candidate.as_bytes();
    let expected = expected.as_bytes();
    let mut diff = candidate.len() ^ expected.len();

    for index in 0..MAX_GITHUB_SENSITIVE_VALUE_LEN {
        let candidate_byte = candidate.get(index).copied().unwrap_or_default();
        let expected_byte = expected.get(index).copied().unwrap_or_default();
        diff |= usize::from(candidate_byte ^ expected_byte);
    }

    diff == 0
}

fn sanitize_github_diagnostic_value(value: &str) -> String {
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

fn github_api_error_message(body: &str, token: &GitHubPersonalAccessToken) -> Option<String> {
    serde_json::from_str::<GitHubApiErrorResponse>(body)
        .ok()
        .and_then(github_api_error_diagnostic)
        .map(|message| redact_github_secret(&message, token))
        .filter(|message| !message.trim().is_empty())
}

fn redact_github_secret(message: &str, token: &GitHubPersonalAccessToken) -> String {
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
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::{
        Json, Router,
        body::Body,
        http::{
            Request, StatusCode,
            header::{AUTHORIZATION, RETRY_AFTER},
        },
        response::IntoResponse,
        routing::get,
    };
    use tokio::sync::Mutex;
    use tower::ServiceExt;

    use super::{
        GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH, GitHubPersonalAccessToken,
        GitHubPersonalAccessTokenLoginRouteError, GitHubPersonalAccessTokenLoginRouteState,
        GitHubRepositoryPermissionClient, GitHubUserClient,
        github_personal_access_token_login_router,
    };
    use crate::{
        GitHubAuthenticationConfig, GitHubProviderConfig, LfsSessionToken, LocalLfsSessionStore,
        RepositoryIdentity, RepositoryPermission, RepositoryUser, ServerError,
    };

    fn provider(api_url: impl Into<String>) -> GitHubProviderConfig {
        GitHubProviderConfig {
            id: "github-main".to_owned(),
            api_url: api_url.into(),
            authentication: GitHubAuthenticationConfig::new("github_pat_configured"),
            allow_insecure_http: true,
        }
    }

    #[test]
    fn personal_access_token_validation_and_debug_are_secret_safe() {
        let token =
            GitHubPersonalAccessToken::from_secret("github_pat_secret").expect("PAT should parse");
        let rendered = format!("{token:?}");

        assert!(rendered.contains("GitHubPersonalAccessToken"));
        assert!(!rendered.contains("github_pat_secret"));

        for invalid in ["", " padded", "padded ", "two words"] {
            assert!(
                GitHubPersonalAccessToken::from_secret(invalid).is_err(),
                "{invalid:?} should be rejected"
            );
        }
    }

    #[test]
    fn rate_limited_login_response_preserves_retry_after_delay() {
        let response = GitHubPersonalAccessTokenLoginRouteError(ServerError::RateLimited {
            retry_after_seconds: 37,
        })
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers()[RETRY_AFTER], "37");
    }

    #[tokio::test]
    async fn personal_access_token_login_issues_local_session_without_returning_pat() {
        let requests = Arc::new(Mutex::new(0_usize));
        let requests_for_route = Arc::clone(&requests);
        let api = Router::new().route(
            "/api/v3/user",
            get(move || {
                let requests = Arc::clone(&requests_for_route);
                async move {
                    *requests.lock().await += 1;
                    Json(serde_json::json!({"login":"octocat","id":42}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock GitHub API should bind");
        let address = listener.local_addr().expect("address should be available");
        tokio::spawn(async move {
            axum::serve(listener, api)
                .await
                .expect("mock API should run");
        });

        let session_store = LocalLfsSessionStore::new();
        let state = GitHubPersonalAccessTokenLoginRouteState::with_client_and_session_store(
            provider(format!("http://{address}/api/v3")),
            GitHubUserClient::new().expect("user client should build"),
            session_store.clone(),
        )
        .expect("PAT login state should build");
        let app = github_personal_access_token_login_router(state);

        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH)
                    .header(AUTHORIZATION, "Bearer github_pat_configured")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*requests.lock().await, 1);
        let body = axum::body::to_bytes(response.into_body(), 16 * 1024)
            .await
            .expect("body should read");
        let body_text = String::from_utf8(body.to_vec()).expect("body should be UTF-8");
        assert!(!body_text.contains("github_pat_configured"));
        let body: serde_json::Value =
            serde_json::from_str(&body_text).expect("body should be JSON");
        let local_token = LfsSessionToken::from_secret(
            body["lfs_token"]
                .as_str()
                .expect("local token should exist"),
        )
        .expect("local token should validate");
        let record = session_store
            .verify_record(&local_token)
            .expect("session should be stored");
        assert_eq!(record.metadata().login, "octocat");
        assert_eq!(
            record
                .github_personal_access_token()
                .expect("PAT should remain server-side")
                .as_str(),
            "github_pat_configured"
        );
    }

    #[tokio::test]
    async fn personal_access_token_login_rejects_mismatch_without_calling_github() {
        let state = GitHubPersonalAccessTokenLoginRouteState::with_client_and_session_store(
            provider("http://127.0.0.1:9/api/v3"),
            GitHubUserClient::new().expect("user client should build"),
            LocalLfsSessionStore::new(),
        )
        .expect("PAT login state should build");
        let response = github_personal_access_token_login_router(state)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(GITHUB_PERSONAL_ACCESS_TOKEN_LOGIN_PATH)
                    .header(AUTHORIZATION, "Bearer github_pat_wrong")
                    .body(Body::empty())
                    .expect("request should build"),
            )
            .await
            .expect("request should complete");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn permission_client_accepts_matching_repository_and_user_ids() {
        let api = Router::new()
            .route(
                "/api/v3/repos/{owner}/{repo}",
                get(|| async { Json(serde_json::json!({"id":8675309_u64})) }),
            )
            .route(
                "/api/v3/repos/{owner}/{repo}/collaborators/{user}/permission",
                get(|| async {
                    Json(serde_json::json!({
                        "permission":"write",
                        "user":{"id":583231_u64}
                    }))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock GitHub API should bind");
        let address = listener.local_addr().expect("address should be available");
        tokio::spawn(async move {
            axum::serve(listener, api)
                .await
                .expect("mock API should run");
        });

        let repository = RepositoryIdentity {
            provider_id: "github-main".to_owned(),
            stable_id: Some("8675309".to_owned()),
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
        };
        let user = RepositoryUser::new("github-main", "octocat", Some("583231".to_owned()));
        let token =
            GitHubPersonalAccessToken::from_secret("github_pat_secret").expect("PAT should parse");

        let authorization = GitHubRepositoryPermissionClient::new()
            .expect("permission client should build")
            .check_permission(
                &provider(format!("http://{address}/api/v3")),
                &token,
                &repository,
                &user,
                RepositoryPermission::Write,
            )
            .await
            .expect("permission should be granted");

        assert_eq!(authorization.granted, RepositoryPermission::Write);
    }

    #[tokio::test]
    async fn user_client_rejects_invalid_api_url_without_exposing_pat() {
        let token =
            GitHubPersonalAccessToken::from_secret("github_pat_secret").expect("PAT should parse");
        let error = GitHubUserClient::new()
            .expect("user client should build")
            .fetch_authenticated_user(&provider("not a url"), &token)
            .await
            .expect_err("invalid API URL should fail");

        assert!(matches!(error, ServerError::InvalidConfiguration { .. }));
        assert!(!error.to_string().contains("github_pat_secret"));
    }
}
