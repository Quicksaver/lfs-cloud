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
use ring::{
    aead,
    rand::{SecureRandom, SystemRandom},
};
use sha2::{Digest, Sha256};

use crate::{
    GitHubOAuthAccessToken, MetadataDatabase, RepositoryUser, ServerError, ServerResult,
    metadata::MetadataSessionRecord,
};

/// Default lifetime for an issued local LFS Cloud session token.
pub const DEFAULT_LFS_SESSION_TTL: Duration = Duration::from_secs(8 * 60 * 60);

const MAX_LFS_SESSION_TOKEN_LEN: usize = 1024;
const MAX_LOCAL_LFS_SESSIONS: usize = 1024;
const MAX_LFS_SESSION_TOKEN_GENERATION_ATTEMPTS: usize = 8;
const SESSION_ENCRYPTION_CONTEXT: &[u8] = b"lfs-cloud durable session encryption v1\0";
const SESSION_TOKEN_NONCE_LEN: usize = 12;

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

    fn restore(
        provider_id: String,
        login: String,
        stable_id: Option<String>,
        granted_scopes: Vec<String>,
        issued_at: SystemTime,
        expires_at: SystemTime,
    ) -> Self {
        Self {
            provider_id,
            login,
            stable_id,
            granted_scopes,
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

/// Bounded store for local LFS Cloud sessions.
///
/// [`Self::new`] provides isolated in-process storage for tests and injected
/// routers. Production can use [`Self::open_durable`] to restore unexpired
/// sessions from SQLite. Durable storage retains only the local token's SHA-256
/// digest and authenticated-encrypted provider access-token state.
#[derive(Clone)]
pub struct LocalLfsSessionStore {
    sessions: Arc<Mutex<BTreeMap<LfsSessionTokenKey, LfsSessionRecord>>>,
    durable: Option<Arc<DurableLfsSessionStore>>,
}

impl Default for LocalLfsSessionStore {
    fn default() -> Self {
        Self {
            sessions: Arc::new(Mutex::new(BTreeMap::new())),
            durable: None,
        }
    }
}

impl LocalLfsSessionStore {
    /// Creates an empty local LFS session store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a session store backed by the server metadata database.
    ///
    /// `encryption_secret` must be stable across server restarts. The durable
    /// store derives a dedicated AEAD key from it and never writes the secret,
    /// the local bearer token, or a plaintext provider token to SQLite.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when durable rows cannot be read, decoded, or
    /// decrypted with the supplied secret.
    pub fn open_durable(
        database: Arc<MetadataDatabase>,
        encryption_secret: impl AsRef<[u8]>,
    ) -> ServerResult<Self> {
        let durable = Arc::new(DurableLfsSessionStore::new(
            database,
            encryption_secret.as_ref(),
        )?);
        let sessions = durable.load_sessions()?;
        if sessions.len() > MAX_LOCAL_LFS_SESSIONS {
            return Err(ServerError::Internal {
                message: "durable lfs session count exceeds the configured in-process limit"
                    .to_owned(),
            });
        }

        Ok(Self {
            sessions: Arc::new(Mutex::new(sessions)),
            durable: Some(durable),
        })
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
        if let Some(durable) = &self.durable {
            durable
                .database
                .delete_expired_sessions(unix_timestamp_seconds_i64(issued_at)?)?;
        }

        for _ in 0..MAX_LFS_SESSION_TOKEN_GENERATION_ATTEMPTS {
            let token = LfsSessionToken::generate();
            let token_key = LfsSessionTokenKey::from_token(&token);

            if sessions.contains_key(&token_key) {
                continue;
            }

            if sessions.len() >= MAX_LOCAL_LFS_SESSIONS {
                let evicted =
                    session_expiring_soonest(&sessions).ok_or_else(|| ServerError::Internal {
                        message: "failed to select an lfs session for bounded-store eviction"
                            .to_owned(),
                    })?;
                if let Some(durable) = &self.durable {
                    durable.database.delete_session(&evicted.to_hex())?;
                }
                sessions.remove(&evicted);
            }

            if let Some(durable) = &self.durable {
                durable.record_session(&token_key, &record)?;
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
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when durable session storage is configured and
    /// SQLite cannot delete the matching row.
    pub fn revoke(&self, token: &LfsSessionToken) -> ServerResult<bool> {
        let now = SystemTime::now();
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        prune_expired_sessions(&mut sessions, now);

        let token_key = LfsSessionTokenKey::from_token(token);
        if let Some(durable) = &self.durable {
            durable.database.delete_session(&token_key.to_hex())?;
        }

        Ok(sessions.remove(&token_key).is_some())
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
            .field("durable", &self.durable.is_some())
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

    fn from_hex(value: &str) -> ServerResult<Self> {
        if value.len() != 64 {
            return Err(invalid_durable_session(
                "stored token digest has an invalid length",
            ));
        }
        let mut digest = [0_u8; 32];
        for (index, chunk) in value.as_bytes().chunks_exact(2).enumerate() {
            let encoded = std::str::from_utf8(chunk)
                .map_err(|_| invalid_durable_session("stored token digest is not valid UTF-8"))?;
            digest[index] = u8::from_str_radix(encoded, 16)
                .map_err(|_| invalid_durable_session("stored token digest is not hexadecimal"))?;
        }
        Ok(Self(digest))
    }

    fn to_hex(self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}

fn prune_expired_sessions(
    sessions: &mut BTreeMap<LfsSessionTokenKey, LfsSessionRecord>,
    now: SystemTime,
) {
    sessions.retain(|_, record| record.metadata.expires_at > now);
}

fn session_expiring_soonest(
    sessions: &BTreeMap<LfsSessionTokenKey, LfsSessionRecord>,
) -> Option<LfsSessionTokenKey> {
    sessions
        .iter()
        .min_by_key(|(_, record)| record.metadata.expires_at)
        .map(|(token, _)| *token)
}

fn unix_timestamp_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or_default()
}

fn unix_timestamp_seconds_i64(time: SystemTime) -> ServerResult<i64> {
    i64::try_from(unix_timestamp_seconds(time)).map_err(|_| ServerError::Internal {
        message: "lfs session timestamp exceeds SQLite integer range".to_owned(),
    })
}

fn system_time_from_unix_seconds(value: i64) -> ServerResult<SystemTime> {
    let seconds = u64::try_from(value)
        .map_err(|_| invalid_durable_session("stored session timestamp is negative"))?;
    UNIX_EPOCH
        .checked_add(Duration::from_secs(seconds))
        .ok_or_else(|| invalid_durable_session("stored session timestamp overflowed"))
}

fn invalid_durable_session(message: &str) -> ServerError {
    ServerError::Internal {
        message: format!("durable lfs session state is invalid: {message}"),
    }
}

struct DurableLfsSessionStore {
    database: Arc<MetadataDatabase>,
    protector: SessionTokenProtector,
}

impl DurableLfsSessionStore {
    fn new(database: Arc<MetadataDatabase>, encryption_secret: &[u8]) -> ServerResult<Self> {
        if encryption_secret.is_empty() {
            return Err(ServerError::InvalidConfiguration {
                message: "durable lfs session encryption secret must not be empty".to_owned(),
            });
        }

        let now = unix_timestamp_seconds_i64(SystemTime::now())?;
        database.delete_expired_sessions(now)?;
        Ok(Self {
            database,
            protector: SessionTokenProtector::new(encryption_secret)?,
        })
    }

    fn load_sessions(&self) -> ServerResult<BTreeMap<LfsSessionTokenKey, LfsSessionRecord>> {
        let now = unix_timestamp_seconds_i64(SystemTime::now())?;
        self.database
            .load_active_sessions(now)?
            .into_iter()
            .map(|record| self.restore_session(record))
            .collect()
    }

    fn restore_session(
        &self,
        stored: MetadataSessionRecord,
    ) -> ServerResult<(LfsSessionTokenKey, LfsSessionRecord)> {
        let token_key = LfsSessionTokenKey::from_hex(&stored.token_sha256)?;
        let granted_scopes = serde_json::from_str::<Vec<String>>(&stored.granted_scopes_json)
            .map_err(|_| invalid_durable_session("stored granted scopes are not valid JSON"))?;
        let issued_at = system_time_from_unix_seconds(stored.issued_at_unix_seconds)?;
        let expires_at = system_time_from_unix_seconds(stored.expires_at_unix_seconds)?;
        if expires_at <= issued_at {
            return Err(invalid_durable_session(
                "stored session expiration does not follow its issue time",
            ));
        }
        let metadata = LfsSessionMetadata::restore(
            stored.provider_id,
            stored.login,
            stored.stable_id,
            granted_scopes,
            issued_at,
            expires_at,
        );
        let github_access_token = match (
            stored.provider_access_token_ciphertext,
            stored.provider_access_token_nonce,
        ) {
            (Some(ciphertext), Some(nonce)) => Some(
                self.protector
                    .decrypt(&token_key, &metadata, ciphertext, &nonce)?,
            ),
            (None, None) => None,
            _ => {
                return Err(invalid_durable_session(
                    "stored provider token protection fields are incomplete",
                ));
            }
        };

        Ok((
            token_key,
            LfsSessionRecord::new(metadata, github_access_token),
        ))
    }

    fn record_session(
        &self,
        token_key: &LfsSessionTokenKey,
        record: &LfsSessionRecord,
    ) -> ServerResult<()> {
        let protected_token = record
            .github_access_token
            .as_ref()
            .map(|token| self.protector.encrypt(token_key, &record.metadata, token))
            .transpose()?;
        let (provider_access_token_ciphertext, provider_access_token_nonce) = protected_token
            .map(|(ciphertext, nonce)| (Some(ciphertext), Some(nonce.to_vec())))
            .unwrap_or((None, None));
        let stored = MetadataSessionRecord {
            token_sha256: token_key.to_hex(),
            provider_id: record.metadata.provider_id.clone(),
            login: record.metadata.login.clone(),
            stable_id: record.metadata.stable_id.clone(),
            granted_scopes_json: serde_json::to_string(&record.metadata.granted_scopes).map_err(
                |_| ServerError::Internal {
                    message: "failed to encode lfs session scopes for durable storage".to_owned(),
                },
            )?,
            issued_at_unix_seconds: unix_timestamp_seconds_i64(record.metadata.issued_at)?,
            expires_at_unix_seconds: unix_timestamp_seconds_i64(record.metadata.expires_at)?,
            provider_access_token_ciphertext,
            provider_access_token_nonce,
        };

        self.database.record_session(&stored)
    }
}

struct SessionTokenProtector {
    key: aead::LessSafeKey,
    random: SystemRandom,
}

impl SessionTokenProtector {
    fn new(encryption_secret: &[u8]) -> ServerResult<Self> {
        let mut digest = Sha256::new();
        digest.update(SESSION_ENCRYPTION_CONTEXT);
        digest.update(encryption_secret);
        let key_bytes = digest.finalize();
        let unbound_key =
            aead::UnboundKey::new(&aead::AES_256_GCM, key_bytes.as_slice()).map_err(|_| {
                ServerError::Internal {
                    message: "failed to initialize durable lfs session encryption".to_owned(),
                }
            })?;
        Ok(Self {
            key: aead::LessSafeKey::new(unbound_key),
            random: SystemRandom::new(),
        })
    }

    fn encrypt(
        &self,
        token_key: &LfsSessionTokenKey,
        metadata: &LfsSessionMetadata,
        access_token: &GitHubOAuthAccessToken,
    ) -> ServerResult<(Vec<u8>, [u8; SESSION_TOKEN_NONCE_LEN])> {
        let mut nonce_bytes = [0_u8; SESSION_TOKEN_NONCE_LEN];
        self.random
            .fill(&mut nonce_bytes)
            .map_err(|_| ServerError::Internal {
                message: "failed to generate durable lfs session encryption nonce".to_owned(),
            })?;
        let nonce = aead::Nonce::assume_unique_for_key(nonce_bytes);
        let mut ciphertext = access_token.as_str().as_bytes().to_vec();
        let associated_data = session_associated_data(token_key, metadata)?;
        self.key
            .seal_in_place_append_tag(nonce, aead::Aad::from(associated_data), &mut ciphertext)
            .map_err(|_| ServerError::Internal {
                message: "failed to encrypt durable lfs session provider token".to_owned(),
            })?;

        Ok((ciphertext, nonce_bytes))
    }

    fn decrypt(
        &self,
        token_key: &LfsSessionTokenKey,
        metadata: &LfsSessionMetadata,
        mut ciphertext: Vec<u8>,
        nonce: &[u8],
    ) -> ServerResult<GitHubOAuthAccessToken> {
        let nonce_bytes: [u8; SESSION_TOKEN_NONCE_LEN] = nonce.try_into().map_err(|_| {
            invalid_durable_session("stored provider token nonce has invalid length")
        })?;
        let associated_data = session_associated_data(token_key, metadata)?;
        let plaintext = self
            .key
            .open_in_place(
                aead::Nonce::assume_unique_for_key(nonce_bytes),
                aead::Aad::from(associated_data),
                &mut ciphertext,
            )
            .map_err(|_| {
                invalid_durable_session(
                    "stored provider token could not be authenticated or decrypted",
                )
            })?;
        let token = String::from_utf8(plaintext.to_vec())
            .map_err(|_| invalid_durable_session("decrypted provider token is not valid UTF-8"))?;
        GitHubOAuthAccessToken::from_secret(token)
            .map_err(|_| invalid_durable_session("decrypted provider token is invalid"))
    }
}

fn session_associated_data(
    token_key: &LfsSessionTokenKey,
    metadata: &LfsSessionMetadata,
) -> ServerResult<Vec<u8>> {
    serde_json::to_vec(&(
        token_key.to_hex(),
        metadata.provider_id.as_str(),
        metadata.login.as_str(),
        metadata.stable_id.as_deref(),
        metadata.granted_scopes.as_slice(),
        unix_timestamp_seconds_i64(metadata.issued_at)?,
        unix_timestamp_seconds_i64(metadata.expires_at)?,
    ))
    .map_err(|_| ServerError::Internal {
        message: "failed to encode durable lfs session authenticated metadata".to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use std::{fs, sync::Arc, thread, time::Duration};

    use super::{LfsSessionToken, LocalLfsSessionStore, MAX_LFS_SESSION_TOKEN_LEN};
    use crate::{GitHubOAuthAccessToken, MetadataDatabase, RepositoryUser, ServerError};

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

        assert!(
            store
                .revoke(&issued.token)
                .expect("session revocation should succeed")
        );
        assert!(
            !store
                .revoke(&issued.token)
                .expect("repeated session revocation should succeed")
        );
        assert_eq!(store.verify(&issued.token), None);
    }

    #[test]
    fn durable_session_store_restores_protected_session_after_reopen() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database_path = directory.path().join("metadata.sqlite3");
        let encryption_secret = b"stable-server-side-session-encryption-secret";
        let github_token_secret = "gho_restart_authorization";

        let issued = {
            let database =
                Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should open"));
            let store = LocalLfsSessionStore::open_durable(database, encryption_secret)
                .expect("durable session store should open");
            let github_token = GitHubOAuthAccessToken::from_secret(github_token_secret)
                .expect("GitHub token should parse");

            store
                .issue_session_with_github_token(&user(), ["read:user", "repo"], github_token)
                .expect("durable session should be issued")
        };

        let database_bytes = fs::read(&database_path).expect("metadata DB should be readable");
        assert!(
            !database_bytes
                .windows(issued.token.as_str().len())
                .any(|window| window == issued.token.as_str().as_bytes())
        );
        assert!(
            !database_bytes
                .windows(github_token_secret.len())
                .any(|window| window == github_token_secret.as_bytes())
        );

        let wrong_key_database =
            Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should reopen"));
        let wrong_key_error = LocalLfsSessionStore::open_durable(
            wrong_key_database,
            b"different-session-encryption-secret",
        )
        .expect_err("a different encryption secret must not restore sessions");
        let wrong_key_message = wrong_key_error.to_string();
        assert!(wrong_key_message.contains("could not be authenticated or decrypted"));
        assert!(!wrong_key_message.contains(github_token_secret));
        assert!(!wrong_key_message.contains(issued.token.as_str()));

        let reopened_database =
            Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should reopen"));
        let reopened = LocalLfsSessionStore::open_durable(reopened_database, encryption_secret)
            .expect("durable session store should restore sessions");
        let restored = reopened
            .verify_record(&issued.token)
            .expect("session should survive metadata DB reopen");

        assert_eq!(restored.metadata().provider_id, "github-main");
        assert_eq!(restored.metadata().login, "octocat");
        assert_eq!(restored.metadata().stable_id.as_deref(), Some("42"));
        assert_eq!(restored.metadata().granted_scopes, ["read:user", "repo"]);
        assert_eq!(
            restored
                .github_access_token()
                .expect("restored session should retain the GitHub token")
                .as_str(),
            github_token_secret
        );
    }

    #[test]
    fn durable_session_store_authenticates_persisted_identity_metadata() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database_path = directory.path().join("metadata.sqlite3");
        let encryption_secret = b"stable-server-side-session-encryption-secret";
        let issued = {
            let database =
                Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should open"));
            let store = LocalLfsSessionStore::open_durable(database, encryption_secret)
                .expect("durable session store should open");
            let github_token = GitHubOAuthAccessToken::from_secret("gho_protected_metadata")
                .expect("GitHub token should parse");

            store
                .issue_session_with_github_token(&user(), ["repo"], github_token)
                .expect("durable session should be issued")
        };

        let connection = rusqlite::Connection::open(&database_path)
            .expect("metadata DB should open for tampering fixture");
        connection
            .execute("UPDATE sessions SET login = 'attacker'", [])
            .expect("session fixture should be tampered");
        drop(connection);

        let reopened_database =
            Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should reopen"));
        let error = LocalLfsSessionStore::open_durable(reopened_database, encryption_secret)
            .expect_err("tampered session metadata must not authenticate");
        let message = error.to_string();

        assert!(message.contains("could not be authenticated or decrypted"));
        assert!(!message.contains("gho_protected_metadata"));
        assert!(!message.contains(issued.token.as_str()));
    }
}
