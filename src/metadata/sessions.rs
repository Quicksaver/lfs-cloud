//! Durable, protected local LFS session records.

use rusqlite::{Row, params};

#[allow(unused_imports)]
use crate::{ServerError, ServerResult};

use super::MetadataDatabase;

/// Durable local-session state stored in the metadata database.
///
/// The local bearer token is represented only by its SHA-256 digest. Provider
/// access-token bytes are authenticated-encrypted by the session layer before
/// crossing this persistence boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct MetadataSessionRecord {
    /// Hex-encoded SHA-256 digest of the local LFS bearer token.
    pub(crate) token_sha256: String,
    /// Repository provider that authenticated the user.
    pub(crate) provider_id: String,
    /// Authenticated repository-provider login.
    pub(crate) login: String,
    /// Provider-specific stable user ID.
    pub(crate) stable_id: Option<String>,
    /// JSON-encoded provider permission/scope list.
    pub(crate) granted_scopes_json: String,
    /// Session issue time as Unix seconds.
    pub(crate) issued_at_unix_seconds: i64,
    /// Session expiry time as Unix seconds.
    pub(crate) expires_at_unix_seconds: i64,
    /// Authenticated-encrypted provider access token, when one is required.
    pub(crate) provider_access_token_ciphertext: Option<Vec<u8>>,
    /// Unique AEAD nonce paired with the encrypted provider access token.
    pub(crate) provider_access_token_nonce: Option<Vec<u8>>,
}

impl MetadataDatabase {
    /// Loads all persisted local sessions that have not expired.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot read durable session rows.
    pub(crate) fn load_active_sessions(
        &self,
        now_unix_seconds: i64,
    ) -> ServerResult<Vec<MetadataSessionRecord>> {
        let connection = self.lock_connection()?;
        let mut statement = connection
            .prepare(
                "SELECT
                    token_sha256,
                    provider_id,
                    login,
                    stable_id,
                    granted_scopes_json,
                    issued_at_unix_seconds,
                    expires_at_unix_seconds,
                    provider_access_token_ciphertext,
                    provider_access_token_nonce
                 FROM sessions
                 WHERE expires_at_unix_seconds > ?1",
            )
            .map_err(|source| self.operation_error(source))?;
        let rows = statement
            .query_map([now_unix_seconds], metadata_session_record_from_row)
            .map_err(|source| self.operation_error(source))?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| self.operation_error(source))
    }

    /// Inserts or replaces one protected durable local-session row.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot persist the session.
    pub(crate) fn record_session(&self, record: &MetadataSessionRecord) -> ServerResult<()> {
        self.lock_connection()?
            .execute(
                "INSERT INTO sessions(
                    token_sha256,
                    provider_id,
                    login,
                    stable_id,
                    granted_scopes_json,
                    issued_at_unix_seconds,
                    expires_at_unix_seconds,
                    provider_access_token_ciphertext,
                    provider_access_token_nonce
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
                ON CONFLICT(token_sha256)
                DO UPDATE SET
                    provider_id = excluded.provider_id,
                    login = excluded.login,
                    stable_id = excluded.stable_id,
                    granted_scopes_json = excluded.granted_scopes_json,
                    issued_at_unix_seconds = excluded.issued_at_unix_seconds,
                    expires_at_unix_seconds = excluded.expires_at_unix_seconds,
                    provider_access_token_ciphertext = excluded.provider_access_token_ciphertext,
                    provider_access_token_nonce = excluded.provider_access_token_nonce",
                params![
                    record.token_sha256,
                    record.provider_id,
                    record.login,
                    record.stable_id,
                    record.granted_scopes_json,
                    record.issued_at_unix_seconds,
                    record.expires_at_unix_seconds,
                    record.provider_access_token_ciphertext,
                    record.provider_access_token_nonce,
                ],
            )
            .map(|_| ())
            .map_err(|source| self.operation_error(source))
    }

    /// Deletes one durable local session by token digest.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot delete the session.
    pub(crate) fn delete_session(&self, token_sha256: &str) -> ServerResult<bool> {
        self.lock_connection()?
            .execute(
                "DELETE FROM sessions WHERE token_sha256 = ?1",
                [token_sha256],
            )
            .map(|deleted| deleted > 0)
            .map_err(|source| self.operation_error(source))
    }

    /// Removes expired durable local sessions.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot prune expired rows.
    pub(crate) fn delete_expired_sessions(&self, now_unix_seconds: i64) -> ServerResult<usize> {
        self.lock_connection()?
            .execute(
                "DELETE FROM sessions WHERE expires_at_unix_seconds <= ?1",
                [now_unix_seconds],
            )
            .map_err(|source| self.operation_error(source))
    }

    /// Counts every durable session row, including rows that have just
    /// expired but have not yet been pruned.
    pub(crate) fn session_count(&self) -> ServerResult<usize> {
        let count = self
            .lock_connection()?
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|source| self.operation_error(source))?;
        usize::try_from(count).map_err(|_| ServerError::Internal {
            message: "durable session count exceeds this platform's addressable range".to_owned(),
        })
    }

    /// Invalidates every durable local session.
    pub(crate) fn delete_all_sessions(&self) -> ServerResult<usize> {
        self.lock_connection()?
            .execute("DELETE FROM sessions", [])
            .map_err(|source| self.operation_error(source))
    }
}

fn metadata_session_record_from_row(row: &Row<'_>) -> rusqlite::Result<MetadataSessionRecord> {
    Ok(MetadataSessionRecord {
        token_sha256: row.get("token_sha256")?,
        provider_id: row.get("provider_id")?,
        login: row.get("login")?,
        stable_id: row.get("stable_id")?,
        granted_scopes_json: row.get("granted_scopes_json")?,
        issued_at_unix_seconds: row.get("issued_at_unix_seconds")?,
        expires_at_unix_seconds: row.get("expires_at_unix_seconds")?,
        provider_access_token_ciphertext: row.get("provider_access_token_ciphertext")?,
        provider_access_token_nonce: row.get("provider_access_token_nonce")?,
    })
}
