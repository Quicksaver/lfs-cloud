//! GitHub OAuth helpers for repository-provider login.
//!
//! This module owns the browser-facing authorization URL construction used at
//! the start of the GitHub login flow. Later callback and token-exchange code
//! should validate the returned CSRF state before accepting an OAuth code.

use oauth2::{
    AuthUrl, ClientId, CsrfToken, RedirectUrl, Scope, basic::BasicClient,
    url::ParseError as UrlParseError,
};
use url::Url;

use crate::{GitHubProviderConfig, ServerError, ServerResult};

/// GitHub's OAuth authorization endpoint for the initial GitHub.com provider.
pub const GITHUB_OAUTH_AUTHORIZE_URL: &str = "https://github.com/login/oauth/authorize";

/// Default GitHub OAuth scopes for the initial login URL.
///
/// `read:user` lets the server identify the authenticated GitHub account
/// without putting the broader repository scope decision into this URL helper.
pub const DEFAULT_GITHUB_OAUTH_SCOPES: &[&str] = &["read:user"];

/// Browser URL plus CSRF state for a GitHub OAuth authorization attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
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
    pub fn with_scopes<'a>(
        provider: &GitHubProviderConfig,
        redirect_url: impl Into<String>,
        scopes: impl IntoIterator<Item = &'a str>,
    ) -> ServerResult<Self> {
        Self::with_state(provider, redirect_url, scopes, CsrfToken::new_random)
    }

    fn with_state<'a>(
        provider: &GitHubProviderConfig,
        redirect_url: impl Into<String>,
        scopes: impl IntoIterator<Item = &'a str>,
        state_fn: impl FnOnce() -> CsrfToken,
    ) -> ServerResult<Self> {
        let redirect_url = RedirectUrl::new(redirect_url.into())
            .map_err(|source| invalid_oauth_url("github oauth redirect_url", source))?;
        let client =
            BasicClient::new(ClientId::new(provider.oauth_client_id.clone()))
                .set_auth_uri(AuthUrl::new(GITHUB_OAUTH_AUTHORIZE_URL.to_owned()).expect(
                    "static GitHub OAuth authorization endpoint must be a valid absolute URL",
                ))
                .set_redirect_uri(redirect_url);
        let request = scopes
            .into_iter()
            .filter(|scope| !scope.trim().is_empty())
            .fold(client.authorize_url(state_fn), |request, scope| {
                request.add_scope(Scope::new(scope.to_owned()))
            });
        let (authorization_url, csrf_state) = request.url();

        Ok(Self {
            authorization_url,
            csrf_state: GitHubOAuthState::from(csrf_state),
        })
    }
}

/// CSRF state generated for a GitHub OAuth authorization attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitHubOAuthState(String);

impl GitHubOAuthState {
    /// Returns the state value that must match the OAuth callback.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<CsrfToken> for GitHubOAuthState {
    fn from(value: CsrfToken) -> Self {
        Self(value.secret().clone())
    }
}

/// Creates a GitHub OAuth authorization URL using default LFS Cloud scopes.
///
/// # Errors
///
/// Returns [`ServerError`] when the configured redirect URL is invalid.
pub fn github_oauth_authorization_url(
    provider: &GitHubProviderConfig,
    redirect_url: impl Into<String>,
) -> ServerResult<GitHubOAuthAuthorization> {
    GitHubOAuthAuthorization::new(provider, redirect_url)
}

fn invalid_oauth_url(path: &str, source: UrlParseError) -> ServerError {
    ServerError::InvalidConfiguration {
        message: format!("{path} must be a valid absolute URL: {source}"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use oauth2::CsrfToken;

    use super::{
        DEFAULT_GITHUB_OAUTH_SCOPES, GITHUB_OAUTH_AUTHORIZE_URL, GitHubOAuthAuthorization,
        github_oauth_authorization_url,
    };
    use crate::GitHubProviderConfig;

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

        assert_eq!(
            authorization.authorization_url.as_str(),
            "https://github.com/login/oauth/authorize?response_type=code&client_id=client-id&state=csrf-state&redirect_uri=http%3A%2F%2F127.0.0.1%3A8080%2Fauth%2Fgithub%2Fcallback&scope=read%3Auser"
        );
        assert_eq!(authorization.csrf_state.as_str(), "csrf-state");
    }

    #[test]
    fn csrf_state_matches_url_query_parameter() {
        let authorization = github_oauth_authorization_url(&provider_config(), REDIRECT_URL)
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
        let first = github_oauth_authorization_url(&provider_config(), REDIRECT_URL)
            .expect("first authorization URL should build");
        let second = github_oauth_authorization_url(&provider_config(), REDIRECT_URL)
            .expect("second authorization URL should build");

        assert_ne!(first.csrf_state, second.csrf_state);
    }

    #[test]
    fn authorization_url_does_not_expose_client_secret() {
        let authorization = github_oauth_authorization_url(&provider_config(), REDIRECT_URL)
            .expect("authorization URL should build");
        let rendered = authorization.authorization_url.as_str();

        assert!(!rendered.contains("client-secret"));
        assert_eq!(query_pairs(&authorization).get("client_secret"), None);
    }

    #[test]
    fn custom_scopes_are_encoded_as_space_separated_github_scope() {
        let authorization = GitHubOAuthAuthorization::with_state(
            &provider_config(),
            REDIRECT_URL,
            ["read:user", "repo"],
            || CsrfToken::new("csrf-state".to_owned()),
        )
        .expect("authorization URL should build");

        assert_eq!(
            query_pairs(&authorization).get("scope").map(String::as_str),
            Some("read:user repo")
        );
    }

    #[test]
    fn invalid_redirect_url_is_rejected() {
        let error = github_oauth_authorization_url(&provider_config(), "not a url").unwrap_err();

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

    fn query_pairs(authorization: &GitHubOAuthAuthorization) -> BTreeMap<String, String> {
        authorization
            .authorization_url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect()
    }
}
