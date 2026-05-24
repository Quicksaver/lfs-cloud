//! Local LFS session token metadata.
//!
//! GitHub OAuth tokens are used only for repository-provider API calls. This
//! module issues separate short-lived LFS Cloud bearer tokens that Git LFS can
//! use without receiving the upstream GitHub token.

use std::{
    collections::BTreeMap,
    fmt,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use oauth2::CsrfToken;
use sha2::{Digest, Sha256};

use crate::{GitHubOAuthAccessToken, RepositoryUser, ServerError, ServerResult};

/// Default lifetime for an issued local LFS Cloud session token.
pub const DEFAULT_LFS_SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);

const MAX_LFS_SESSION_TOKEN_LEN: usize = 1024;
const MAX_LOCAL_LFS_SESSIONS: usize = 1024;
const MAX_LFS_SESSION_TOKEN_GENERATION_ATTEMPTS: usize = 8;

/// Opaque bearer token issued by LFS Cloud for Git LFS clients.
///
/// This token is distinct from any upstream GitHub OAuth token. It is the only
/// token value that should be stored in Git's credential helper for LFS routes.
#[derive(Clone, Eq, Hash, PartialEq)]
pub struct LfsSessionToken(String);

impl LfsSessionToken {
    /// Generates a fresh opaque local LFS session token.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::LfsSessionToken;
    ///
    /// let token = LfsSessionToken::generate();
    ///
    /// assert!(!token.as_str().is_empty());
    /// ```
    #[must_use]
    pub fn generate() -> Self {
        Self::from_secret(CsrfToken::new_random().secret().clone())
            .expect("oauth2-generated CSRF token should be a valid local LFS token")
    }

    /// Restores an existing local LFS session token secret.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the token is blank, padded, too long, or
    /// contains whitespace/control characters.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::LfsSessionToken;
    ///
    /// let token = LfsSessionToken::from_secret("lfs-token")?;
    ///
    /// assert_eq!(token.as_str(), "lfs-token");
    /// # Ok::<(), lfs_cloud::ServerError>(())
    /// ```
    pub fn from_secret(secret: impl Into<String>) -> ServerResult<Self> {
        validate_lfs_session_secret(secret.into()).map(Self)
    }

    /// Returns the raw token secret for bearer-auth and credential-helper use.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for LfsSessionToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("LfsSessionToken(<redacted>)")
    }
}

/// Non-secret metadata associated with an issued LFS Cloud session.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsSessionMetadata {
    /// Repository provider that authenticated the user.
    pub provider_id: String,
    /// Authenticated repository-provider login.
    pub login: String,
    /// Provider-specific stable user ID, when available.
    pub stable_id: Option<String>,
    /// OAuth scopes granted by the repository provider during login.
    pub granted_scopes: Vec<String>,
    /// Time the local LFS session token was issued.
    pub issued_at: SystemTime,
    /// Time the local LFS session token expires.
    pub expires_at: SystemTime,
}

/// Private server-side state associated with a local LFS session.
///
/// Git LFS clients receive only [`LfsSessionToken`]. The optional GitHub token
/// is retained server-side so batch requests can re-check repository
/// permissions without handing the upstream token to Git.
#[derive(Clone, Eq, PartialEq)]
pub(crate) struct LfsSessionRecord {
    /// Non-secret metadata for the local LFS session.
    metadata: LfsSessionMetadata,
    github_access_token: Option<GitHubOAuthAccessToken>,
}

impl LfsSessionRecord {
    fn new(
        metadata: LfsSessionMetadata,
        github_access_token: Option<GitHubOAuthAccessToken>,
    ) -> Self {
        Self {
            metadata,
            github_access_token,
        }
    }

    /// Returns the non-secret metadata for this local LFS session.
    #[must_use]
    pub(crate) fn metadata(&self) -> &LfsSessionMetadata {
        &self.metadata
    }

    /// Returns the private GitHub OAuth token kept server-side for permission checks.
    #[must_use]
    pub(crate) fn github_access_token(&self) -> Option<&GitHubOAuthAccessToken> {
        self.github_access_token.as_ref()
    }
}

impl fmt::Debug for LfsSessionRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LfsSessionRecord")
            .field("metadata", &self.metadata)
            .field(
                "github_access_token",
                &self.github_access_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

impl LfsSessionMetadata {
    fn new(
        user: &RepositoryUser,
        granted_scopes: impl IntoIterator<Item = impl Into<String>>,
        issued_at: SystemTime,
        expires_at: SystemTime,
    ) -> Self {
        Self {
            provider_id: user.provider_id.clone(),
            login: user.login.clone(),
            stable_id: user.stable_id.clone(),
            granted_scopes: granted_scopes
                .into_iter()
                .map(Into::into)
                .map(|scope| scope.trim().to_owned())
                .filter(|scope| !scope.is_empty())
                .collect(),
            issued_at,
            expires_at,
        }
    }

    /// Returns the expiration time as seconds since the Unix epoch.
    ///
    /// This is stable for JSON responses and credential-helper handoff code.
    #[must_use]
    pub fn expires_at_unix_seconds(&self) -> u64 {
        unix_timestamp_seconds(self.expires_at)
    }
}

/// Newly issued local LFS session containing the bearer token and metadata.
#[derive(Clone, Eq, PartialEq)]
pub struct IssuedLfsSession {
    /// Opaque LFS Cloud token suitable for Git credential storage.
    pub token: LfsSessionToken,
    /// Non-secret session metadata stored by the local LFS Cloud server.
    pub metadata: LfsSessionMetadata,
}

impl fmt::Debug for IssuedLfsSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedLfsSession")
            .field("token", &self.token)
            .field("metadata", &self.metadata)
            .finish()
    }
}

/// Bounded in-process store for local LFS Cloud sessions.
///
/// This store is intentionally local process state. The later metadata database
/// work can provide durable storage without changing the token boundary: Git
/// LFS receives only [`LfsSessionToken`], never the GitHub OAuth token.
#[derive(Clone, Default)]
pub struct LocalLfsSessionStore {
    sessions: Arc<Mutex<BTreeMap<LfsSessionTokenKey, LfsSessionRecord>>>,
}

impl LocalLfsSessionStore {
    /// Creates an empty local LFS session store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Issues a new local LFS session using the default token lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if the default lifetime cannot be represented.
    pub fn issue_session(
        &self,
        user: &RepositoryUser,
        granted_scopes: impl IntoIterator<Item = impl Into<String>>,
    ) -> ServerResult<IssuedLfsSession> {
        self.issue_session_record(user, granted_scopes, DEFAULT_LFS_SESSION_TTL, None)
    }

    /// Issues a local LFS session that retains a GitHub token server-side.
    ///
    /// The returned local token is still the only secret intended for Git LFS
    /// and Git credential-helper storage. The GitHub token remains private
    /// process state used for repository permission checks.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if the default lifetime cannot be represented.
    pub fn issue_session_with_github_token(
        &self,
        user: &RepositoryUser,
        granted_scopes: impl IntoIterator<Item = impl Into<String>>,
        github_access_token: GitHubOAuthAccessToken,
    ) -> ServerResult<IssuedLfsSession> {
        self.issue_session_record(
            user,
            granted_scopes,
            DEFAULT_LFS_SESSION_TTL,
            Some(github_access_token),
        )
    }

    /// Issues a new local LFS session using an explicit token lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when `ttl` is zero, the expiration timestamp
    /// cannot be represented, or a unique token cannot be generated.
    pub fn issue_session_with_ttl(
        &self,
        user: &RepositoryUser,
        granted_scopes: impl IntoIterator<Item = impl Into<String>>,
        ttl: Duration,
    ) -> ServerResult<IssuedLfsSession> {
        self.issue_session_record(user, granted_scopes, ttl, None)
    }

    fn issue_session_record(
        &self,
        user: &RepositoryUser,
        granted_scopes: impl IntoIterator<Item = impl Into<String>>,
        ttl: Duration,
        github_access_token: Option<GitHubOAuthAccessToken>,
    ) -> ServerResult<IssuedLfsSession> {
        if ttl.is_zero() {
            return Err(ServerError::InvalidRequest {
                message: "lfs session ttl must be greater than zero".to_owned(),
            });
        }

        let issued_at = SystemTime::now();
        let expires_at = issued_at
            .checked_add(ttl)
            .ok_or_else(|| ServerError::InvalidRequest {
                message: "lfs session expiration timestamp overflowed".to_owned(),
            })?;
        let metadata = LfsSessionMetadata::new(user, granted_scopes, issued_at, expires_at);
        let record = LfsSessionRecord::new(metadata, github_access_token);

        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_sessions(&mut sessions, issued_at);

        for _ in 0..MAX_LFS_SESSION_TOKEN_GENERATION_ATTEMPTS {
            let token = LfsSessionToken::generate();
            let token_key = LfsSessionTokenKey::from_token(&token);

            if sessions.contains_key(&token_key) {
                continue;
            }

            if sessions.len() >= MAX_LOCAL_LFS_SESSIONS {
                evict_session_expiring_soonest(&mut sessions);
            }

            sessions.insert(token_key, record.clone());
            return Ok(IssuedLfsSession {
                token,
                metadata: record.metadata,
            });
        }

        Err(ServerError::Internal {
            message: "failed to generate a unique lfs session token".to_owned(),
        })
    }

    /// Returns non-secret metadata for a valid, unexpired token.
    #[must_use]
    pub fn verify(&self, token: &LfsSessionToken) -> Option<LfsSessionMetadata> {
        self.verify_record(token).map(|record| record.metadata)
    }

    /// Returns private server-side state for a valid, unexpired token.
    #[must_use]
    pub(crate) fn verify_record(&self, token: &LfsSessionToken) -> Option<LfsSessionRecord> {
        let now = SystemTime::now();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_sessions(&mut sessions, now);

        sessions
            .get(&LfsSessionTokenKey::from_token(token))
            .cloned()
    }

    /// Revokes a token if it is currently stored.
    ///
    /// Returns `true` when a matching token was removed.
    pub fn revoke(&self, token: &LfsSessionToken) -> bool {
        let now = SystemTime::now();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_sessions(&mut sessions, now);

        sessions
            .remove(&LfsSessionTokenKey::from_token(token))
            .is_some()
    }

    fn len(&self) -> usize {
        let now = SystemTime::now();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_sessions(&mut sessions, now);
        sessions.len()
    }
}

impl fmt::Debug for LocalLfsSessionStore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalLfsSessionStore")
            .field("sessions", &self.len())
            .finish()
    }
}

fn validate_lfs_session_secret(value: String) -> ServerResult<String> {
    if value.len() > MAX_LFS_SESSION_TOKEN_LEN {
        return Err(ServerError::InvalidRequest {
            message: format!("lfs session token must not exceed {MAX_LFS_SESSION_TOKEN_LEN} bytes"),
        });
    }

    let trimmed = value.trim();
    if trimmed.is_empty() || trimmed.len() != value.len() {
        return Err(ServerError::InvalidRequest {
            message: "lfs session token must not be blank or padded".to_owned(),
        });
    }

    if value
        .chars()
        .any(|character| character.is_whitespace() || character.is_control())
    {
        return Err(ServerError::InvalidRequest {
            message: "lfs session token must not contain whitespace or control characters"
                .to_owned(),
        });
    }

    Ok(value)
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct LfsSessionTokenKey([u8; 32]);

impl LfsSessionTokenKey {
    fn from_token(token: &LfsSessionToken) -> Self {
        let digest: [u8; 32] = Sha256::digest(token.as_str().as_bytes()).into();
        Self(digest)
    }
}

fn prune_expired_sessions(
    sessions: &mut BTreeMap<LfsSessionTokenKey, LfsSessionRecord>,
    now: SystemTime,
) {
    sessions.retain(|_, record| record.metadata.expires_at > now);
}

fn evict_session_expiring_soonest(sessions: &mut BTreeMap<LfsSessionTokenKey, LfsSessionRecord>) {
    if let Some(token) = sessions
        .iter()
        .min_by_key(|(_, record)| record.metadata.expires_at)
        .map(|(token, _)| *token)
    {
        sessions.remove(&token);
    }
}

fn unix_timestamp_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use std::{thread, time::Duration};

    use super::{LfsSessionToken, LocalLfsSessionStore, MAX_LFS_SESSION_TOKEN_LEN};
    use crate::{GitHubOAuthAccessToken, RepositoryUser, ServerError};

    fn user() -> RepositoryUser {
        RepositoryUser::new("github-main", "octocat", Some("42".to_owned()))
    }

    #[test]
    fn session_token_validates_restored_secret_and_redacts_debug() {
        for invalid in ["", "  ", " token", "token ", "token\nvalue"] {
            let error = LfsSessionToken::from_secret(invalid).unwrap_err();
            assert!(matches!(error, ServerError::InvalidRequest { .. }));
        }

        let oversized = "x".repeat(MAX_LFS_SESSION_TOKEN_LEN + 1);
        let error = LfsSessionToken::from_secret(oversized).unwrap_err();
        assert!(matches!(error, ServerError::InvalidRequest { .. }));

        let token = LfsSessionToken::from_secret("local-lfs-token").expect("token should parse");
        assert_eq!(token.as_str(), "local-lfs-token");
        assert!(!format!("{token:?}").contains("local-lfs-token"));
    }

    #[test]
    fn generated_session_token_round_trips_through_validation() {
        let generated = LfsSessionToken::generate();
        let restored =
            LfsSessionToken::from_secret(generated.as_str()).expect("generated token should parse");

        assert_eq!(restored.as_str(), generated.as_str());
    }

    #[test]
    fn session_store_issues_metadata_for_repository_user() {
        let store = LocalLfsSessionStore::new();

        let issued = store
            .issue_session(&user(), ["read:user", " repo ", ""])
            .expect("session should be issued");
        let metadata = store
            .verify(&issued.token)
            .expect("issued token should verify");

        assert_eq!(metadata.provider_id, "github-main");
        assert_eq!(metadata.login, "octocat");
        assert_eq!(metadata.stable_id.as_deref(), Some("42"));
        assert_eq!(metadata.granted_scopes, vec!["read:user", "repo"]);
        assert!(metadata.expires_at > metadata.issued_at);
        assert_eq!(metadata, issued.metadata);
        assert!(!format!("{issued:?}").contains(issued.token.as_str()));
    }

    #[test]
    fn session_store_keeps_github_token_private_for_server_side_authorization() {
        let store = LocalLfsSessionStore::new();
        let github_token =
            GitHubOAuthAccessToken::from_secret("gho_authorization").expect("token should parse");
        let issued = store
            .issue_session_with_github_token(&user(), ["read:user", "repo"], github_token)
            .expect("session should be issued");
        let record = store
            .verify_record(&issued.token)
            .expect("session record should verify");

        assert_eq!(record.metadata().login, "octocat");
        assert_eq!(
            record
                .github_access_token()
                .expect("github token should be retained")
                .as_str(),
            "gho_authorization"
        );
        assert!(!format!("{record:?}").contains("gho_authorization"));
    }

    #[test]
    fn session_store_rejects_zero_ttl_as_invalid_request() {
        let store = LocalLfsSessionStore::new();
        let error = store
            .issue_session_with_ttl(&user(), ["read:user"], Duration::ZERO)
            .unwrap_err();

        assert!(matches!(error, ServerError::InvalidRequest { .. }));
    }

    #[test]
    fn session_store_expires_tokens() {
        let store = LocalLfsSessionStore::new();
        let issued = store
            .issue_session_with_ttl(&user(), ["read:user"], Duration::from_millis(1))
            .expect("session should be issued");

        thread::sleep(Duration::from_millis(10));

        assert_eq!(store.verify(&issued.token), None);
        assert_eq!(store.len(), 0);
    }

    #[test]
    fn session_store_revokes_tokens() {
        let store = LocalLfsSessionStore::new();
        let issued = store
            .issue_session(&user(), ["read:user"])
            .expect("session should be issued");

        assert!(store.revoke(&issued.token));
        assert!(!store.revoke(&issued.token));
        assert_eq!(store.verify(&issued.token), None);
    }
}
