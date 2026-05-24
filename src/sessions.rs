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

use crate::{RepositoryUser, ServerError, ServerResult};

/// Default lifetime for an issued local LFS Cloud session token.
pub const DEFAULT_LFS_SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);

const MAX_LFS_SESSION_TOKEN_LEN: usize = 1024;
const MAX_LOCAL_LFS_SESSIONS: usize = 1024;

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
        Self(CsrfToken::new_random().secret().clone())
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
    sessions: Arc<Mutex<BTreeMap<String, LfsSessionMetadata>>>,
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
        self.issue_session_with_ttl(user, granted_scopes, DEFAULT_LFS_SESSION_TTL)
    }

    /// Issues a new local LFS session using an explicit token lifetime.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when `ttl` is zero or the expiration timestamp
    /// cannot be represented.
    pub fn issue_session_with_ttl(
        &self,
        user: &RepositoryUser,
        granted_scopes: impl IntoIterator<Item = impl Into<String>>,
        ttl: Duration,
    ) -> ServerResult<IssuedLfsSession> {
        if ttl.is_zero() {
            return Err(ServerError::InvalidConfiguration {
                message: "lfs session ttl must be greater than zero".to_owned(),
            });
        }

        let issued_at = SystemTime::now();
        let expires_at =
            issued_at
                .checked_add(ttl)
                .ok_or_else(|| ServerError::InvalidConfiguration {
                    message: "lfs session expiration timestamp overflowed".to_owned(),
                })?;
        let token = LfsSessionToken::generate();
        let metadata = LfsSessionMetadata::new(user, granted_scopes, issued_at, expires_at);

        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_sessions(&mut sessions, issued_at);
        if !sessions.contains_key(token.as_str()) && sessions.len() >= MAX_LOCAL_LFS_SESSIONS {
            evict_session_expiring_soonest(&mut sessions);
        }
        sessions.insert(token.as_str().to_owned(), metadata.clone());

        Ok(IssuedLfsSession { token, metadata })
    }

    /// Returns non-secret metadata for a valid, unexpired token.
    #[must_use]
    pub fn verify(&self, token: &LfsSessionToken) -> Option<LfsSessionMetadata> {
        let now = SystemTime::now();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_sessions(&mut sessions, now);

        sessions
            .iter()
            .find(|(stored_token, _)| constant_time_str_eq(token.as_str(), stored_token))
            .map(|(_, metadata)| metadata.clone())
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

        let before = sessions.len();
        sessions.retain(|stored_token, _| !constant_time_str_eq(token.as_str(), stored_token));
        sessions.len() != before
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

fn prune_expired_sessions(sessions: &mut BTreeMap<String, LfsSessionMetadata>, now: SystemTime) {
    sessions.retain(|_, metadata| metadata.expires_at > now);
}

fn evict_session_expiring_soonest(sessions: &mut BTreeMap<String, LfsSessionMetadata>) {
    if let Some(token) = sessions
        .iter()
        .min_by_key(|(_, metadata)| metadata.expires_at)
        .map(|(token, _)| token.clone())
    {
        sessions.remove(&token);
    }
}

fn constant_time_str_eq(candidate: &str, expected: &str) -> bool {
    let candidate = candidate.as_bytes();
    let expected = expected.as_bytes();
    let mut diff = candidate.len() ^ expected.len();

    for index in 0..MAX_LFS_SESSION_TOKEN_LEN {
        let candidate_byte = candidate.get(index).copied().unwrap_or_default();
        let expected_byte = expected.get(index).copied().unwrap_or_default();
        diff |= usize::from(candidate_byte ^ expected_byte);
    }

    diff == 0
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
    use crate::{RepositoryUser, ServerError};

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
