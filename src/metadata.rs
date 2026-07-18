//! SQLite metadata database setup and schema migrations.
//!
//! The metadata database is server-owned state. It records repository routing,
//! storage-provider records, object mappings, local LFS sessions, and transfer
//! attempts without becoming part of any Git repository's committed config.

use std::{
    collections::HashMap,
    fmt, fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};

use crate::{
    LfsObject, LfsObjectSize, LfsOid, RepositoryMapping, RepositoryUser, ServerConfig, ServerError,
    ServerResult, StorageProviderConfig,
};

/// Current metadata schema version installed by the migration runner.
pub const METADATA_SCHEMA_VERSION: u32 = 4;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
const OBJECT_RECORD_BY_IDENTITY_SQL: &str = "SELECT
        id,
        repo_id,
        storage_provider_id,
        oid,
        size_bytes,
        backend_id,
        created_by_provider_id,
        created_by_login,
        created_by_stable_id,
        verification_status,
        created_at_unix_seconds,
        last_verified_at_unix_seconds
     FROM objects
     WHERE repo_id = ?1
       AND storage_provider_id = ?2
       AND oid = ?3
       AND size_bytes = ?4";

const INITIAL_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE IF NOT EXISTS storage_providers (
    id TEXT PRIMARY KEY,
    provider_type TEXT NOT NULL,
    backend_root_id TEXT NOT NULL,
    display_name TEXT,
    created_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE IF NOT EXISTS repository_mappings (
    id TEXT PRIMARY KEY,
    repo_provider_id TEXT NOT NULL,
    host TEXT NOT NULL,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    storage_provider_id TEXT NOT NULL REFERENCES storage_providers(id) ON DELETE RESTRICT,
    route_path TEXT NOT NULL UNIQUE,
    provider_stable_id TEXT,
    created_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE IF NOT EXISTS objects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL REFERENCES repository_mappings(id) ON DELETE CASCADE,
    storage_provider_id TEXT NOT NULL REFERENCES storage_providers(id) ON DELETE RESTRICT,
    oid TEXT NOT NULL CHECK (length(oid) = 64),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    backend_id TEXT NOT NULL,
    created_by_provider_id TEXT NOT NULL,
    created_by_login TEXT NOT NULL,
    created_by_stable_id TEXT,
    verification_status TEXT NOT NULL CHECK (
        verification_status IN ('verified', 'stale', 'failed')
    ),
    created_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    last_verified_at_unix_seconds INTEGER,
    UNIQUE (repo_id, storage_provider_id, oid, size_bytes)
) STRICT;

CREATE INDEX IF NOT EXISTS objects_lookup_idx
    ON objects(repo_id, storage_provider_id, oid, size_bytes);
CREATE INDEX IF NOT EXISTS objects_backend_idx
    ON objects(storage_provider_id, backend_id);

CREATE TABLE IF NOT EXISTS sessions (
    token_sha256 TEXT PRIMARY KEY CHECK (length(token_sha256) = 64),
    provider_id TEXT NOT NULL,
    login TEXT NOT NULL,
    stable_id TEXT,
    granted_scopes_json TEXT NOT NULL,
    issued_at_unix_seconds INTEGER NOT NULL,
    expires_at_unix_seconds INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS sessions_expiry_idx
    ON sessions(expires_at_unix_seconds);

CREATE TABLE IF NOT EXISTS transfer_attempts (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL REFERENCES repository_mappings(id) ON DELETE CASCADE,
    storage_provider_id TEXT NOT NULL REFERENCES storage_providers(id) ON DELETE RESTRICT,
    oid TEXT NOT NULL CHECK (length(oid) = 64),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    operation TEXT NOT NULL CHECK (operation IN ('upload', 'download', 'delete')),
    status TEXT NOT NULL CHECK (status IN ('started', 'succeeded', 'failed')),
    user_provider_id TEXT NOT NULL,
    user_login TEXT NOT NULL,
    backend_id TEXT,
    error_category TEXT,
    error_message TEXT,
    started_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    finished_at_unix_seconds INTEGER
) STRICT;

CREATE INDEX IF NOT EXISTS transfer_attempts_object_idx
    ON transfer_attempts(repo_id, storage_provider_id, oid, size_bytes);
CREATE INDEX IF NOT EXISTS transfer_attempts_started_idx
    ON transfer_attempts(started_at_unix_seconds);

INSERT OR IGNORE INTO schema_migrations(version, name)
VALUES (1, 'initial_metadata_schema');
"#;

const NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION: &str = r#"
DROP INDEX IF EXISTS objects_lookup_idx;
DROP INDEX IF EXISTS objects_backend_idx;

CREATE TABLE objects_v2 (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL REFERENCES repository_mappings(id) ON DELETE CASCADE,
    storage_provider_id TEXT NOT NULL REFERENCES storage_providers(id) ON DELETE RESTRICT,
    oid TEXT NOT NULL CHECK (length(oid) = 64),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    backend_id TEXT NOT NULL,
    created_by_provider_id TEXT NOT NULL,
    created_by_login TEXT NOT NULL,
    created_by_stable_id TEXT,
    verification_status TEXT NOT NULL CHECK (
        verification_status IN ('verified', 'stale', 'failed')
    ),
    created_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    last_verified_at_unix_seconds INTEGER,
    UNIQUE (repo_id, storage_provider_id, oid, size_bytes)
) STRICT;

INSERT INTO objects_v2(
    id,
    repo_id,
    storage_provider_id,
    oid,
    size_bytes,
    backend_id,
    created_by_provider_id,
    created_by_login,
    created_by_stable_id,
    verification_status,
    created_at_unix_seconds,
    last_verified_at_unix_seconds
)
SELECT
    id,
    repo_id,
    storage_provider_id,
    oid,
    size_bytes,
    backend_id,
    created_by_provider_id,
    created_by_login,
    created_by_stable_id,
    verification_status,
    created_at_unix_seconds,
    last_verified_at_unix_seconds
FROM objects;

DROP TABLE objects;
ALTER TABLE objects_v2 RENAME TO objects;

CREATE INDEX IF NOT EXISTS objects_lookup_idx
    ON objects(repo_id, storage_provider_id, oid, size_bytes);
CREATE INDEX IF NOT EXISTS objects_backend_idx
    ON objects(storage_provider_id, backend_id);

INSERT OR IGNORE INTO schema_migrations(version, name)
VALUES (2, 'nullable_object_verification_timestamp');

PRAGMA user_version = 2;
"#;

const PROTECTED_SESSION_TOKEN_MIGRATION: &str = r#"
ALTER TABLE sessions
    ADD COLUMN provider_access_token_ciphertext BLOB;
ALTER TABLE sessions
    ADD COLUMN provider_access_token_nonce BLOB;

INSERT OR IGNORE INTO schema_migrations(version, name)
VALUES (3, 'protected_session_tokens');

PRAGMA user_version = 3;
"#;

const ACTIVE_REPOSITORY_MAPPING_MIGRATION: &str = r#"
ALTER TABLE repository_mappings
    ADD COLUMN is_active INTEGER NOT NULL DEFAULT 1
    CHECK (is_active IN (0, 1));

INSERT OR IGNORE INTO schema_migrations(version, name)
VALUES (4, 'active_repository_mappings');

PRAGMA user_version = 4;
"#;

/// Verification state recorded for a repository-scoped object mapping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataObjectVerificationStatus {
    /// The backend object has been verified against the requested OID and size.
    Verified,
    /// The backend identifier is known but should be repaired before use.
    Stale,
    /// The last verification attempt failed and the object should not be served.
    Failed,
}

impl MetadataObjectVerificationStatus {
    fn as_database_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Stale => "stale",
            Self::Failed => "failed",
        }
    }

    fn from_database_str(value: &str) -> Result<Self, MetadataDecodeError> {
        match value {
            "verified" => Ok(Self::Verified),
            "stale" => Ok(Self::Stale),
            "failed" => Ok(Self::Failed),
            _ => Err(MetadataDecodeError::InvalidVerificationStatus(
                value.to_owned(),
            )),
        }
    }
}

impl fmt::Display for MetadataObjectVerificationStatus {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_database_str())
    }
}

/// Repository-scoped object metadata loaded from the SQLite database.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MetadataObjectRecord {
    /// Internal SQLite row identifier.
    pub id: i64,
    /// Configured repository mapping ID.
    pub repo_id: String,
    /// Configured storage provider ID.
    pub storage_provider_id: String,
    /// Provider-independent Git LFS object identity.
    pub object: LfsObject,
    /// Backend file ID or object key returned by the storage provider.
    pub backend_id: String,
    /// Repository-provider user that originally created this object row.
    pub created_by: RepositoryUser,
    /// Unix timestamp for the first time this metadata row was created.
    pub created_at_unix_seconds: i64,
    /// Unix timestamp for the most recent successful object verification.
    pub last_verified_at_unix_seconds: Option<i64>,
    /// Current verification state for this backend mapping.
    pub verification_status: MetadataObjectVerificationStatus,
}

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
    /// JSON-encoded OAuth scope list.
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

/// SQLite metadata database connection.
///
/// The wrapper keeps database setup at the server boundary: callers provide a
/// private server-side path, and the database is opened with foreign-key checks
/// plus the current schema migrations applied.
pub struct MetadataDatabase {
    path: PathBuf,
    connection: Mutex<Connection>,
}

impl MetadataDatabase {
    /// Opens a metadata database file and applies all known migrations.
    ///
    /// Parent directories are created before opening the SQLite database.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the parent directory cannot be created, the
    /// SQLite database cannot be opened, or schema migration fails.
    pub fn open(path: impl AsRef<Path>) -> ServerResult<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent).map_err(|source| ServerError::MetadataDirectoryCreate {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let connection = Connection::open(&path).map_err(|source| ServerError::MetadataOpen {
            path: path.clone(),
            source,
        })?;
        let database = Self {
            path,
            connection: Mutex::new(connection),
        };
        database.configure_connection()?;
        database.run_migrations()?;
        Ok(database)
    }

    /// Opens an in-memory metadata database for tests and ephemeral tools.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot open or configure the
    /// in-memory connection, or when schema migration fails.
    pub fn open_in_memory() -> ServerResult<Self> {
        let path = PathBuf::from(":memory:");
        let connection =
            Connection::open_in_memory().map_err(|source| ServerError::MetadataOpen {
                path: path.clone(),
                source,
            })?;
        let database = Self {
            path,
            connection: Mutex::new(connection),
        };
        database.configure_connection()?;
        database.run_migrations()?;
        Ok(database)
    }

    /// Returns the filesystem path used for this metadata connection.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn run_migrations(&self) -> ServerResult<()> {
        let mut connection = self.lock_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| ServerError::MetadataMigration {
                    path: self.path.clone(),
                    source,
                })?;
        transaction
            .execute_batch(INITIAL_SCHEMA)
            .map_err(|source| ServerError::MetadataMigration {
                path: self.path.clone(),
                source,
            })?;
        let schema_version = transaction
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .map_err(|source| ServerError::MetadataMigration {
                path: self.path.clone(),
                source,
            })?;
        if schema_version < 2 {
            transaction
                .execute_batch(NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION)
                .map_err(|source| ServerError::MetadataMigration {
                    path: self.path.clone(),
                    source,
                })?;
        }
        if schema_version < 3 {
            transaction
                .execute_batch(PROTECTED_SESSION_TOKEN_MIGRATION)
                .map_err(|source| ServerError::MetadataMigration {
                    path: self.path.clone(),
                    source,
                })?;
        }
        if schema_version < 4 {
            transaction
                .execute_batch(ACTIVE_REPOSITORY_MAPPING_MIGRATION)
                .map_err(|source| ServerError::MetadataMigration {
                    path: self.path.clone(),
                    source,
                })?;
        }
        transaction
            .commit()
            .map_err(|source| ServerError::MetadataMigration {
                path: self.path.clone(),
                source,
            })
    }

    /// Returns the SQLite `user_version` set by the migration runner.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] if SQLite cannot read the schema version.
    pub fn schema_version(&self) -> ServerResult<u32> {
        self.lock_connection()?
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .map_err(|source| ServerError::MetadataConfigure {
                path: self.path.clone(),
                source,
            })
    }

    /// Synchronizes validated server configuration into metadata parent rows.
    ///
    /// Object metadata references repository and storage-provider rows through
    /// foreign keys. The server calls this during startup before transfer
    /// handlers can record verified uploads. Removed repository mappings remain
    /// as inactive history, but their route keys are released for the current
    /// configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot upsert the current server
    /// configuration.
    pub fn sync_config(&self, config: &ServerConfig) -> ServerResult<()> {
        let mut connection = self.lock_connection()?;
        let transaction =
            connection
                .transaction()
                .map_err(|source| ServerError::MetadataOperation {
                    path: self.path.clone(),
                    source,
                })?;

        release_inactive_repository_routes(&transaction, config, &self.path)?;

        for storage in config.storage_providers.values() {
            upsert_storage_provider(&transaction, storage, &self.path)?;
        }
        for repository in &config.repositories {
            upsert_repository_mapping(&transaction, repository, &self.path)?;
        }

        transaction
            .commit()
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })
    }

    /// Looks up repository-scoped object metadata by exact object identity.
    ///
    /// The lookup key includes the configured repository and storage provider
    /// IDs so the same SHA-256 object can be tracked independently for
    /// different repositories or backend accounts.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot query the metadata table or
    /// the stored row cannot be decoded into typed object metadata.
    pub fn lookup_object(
        &self,
        repo_id: &str,
        storage_provider_id: &str,
        object: &LfsObject,
    ) -> ServerResult<Option<MetadataObjectRecord>> {
        let size_bytes = sqlite_size_bytes(object)?;
        let connection = self.lock_connection()?;
        query_optional_object_record(
            &connection,
            repo_id,
            storage_provider_id,
            object,
            size_bytes,
            &self.path,
        )
    }

    /// Inserts or repairs object metadata after a successful verified upload.
    ///
    /// The operation is idempotent for the repository/storage/object key: it
    /// creates the object row when missing, or updates the existing row to point
    /// at the newly verified backend ID and marks it `verified`. Repairing an
    /// existing row preserves its original creator attribution.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite rejects the parent repository or
    /// storage IDs, cannot write the row, or cannot reload the typed record.
    pub fn record_verified_object(
        &self,
        repo_id: &str,
        storage_provider_id: &str,
        object: &LfsObject,
        backend_id: &str,
        creator_if_missing: &RepositoryUser,
    ) -> ServerResult<MetadataObjectRecord> {
        let size_bytes = sqlite_size_bytes(object)?;
        let verified_status = MetadataObjectVerificationStatus::Verified.as_database_str();
        let connection = self.lock_connection()?;
        // Keep the same locked connection through the write and RETURNING decode.
        connection
            .query_row(
                "INSERT INTO objects(
                    repo_id,
                    storage_provider_id,
                    oid,
                    size_bytes,
                    backend_id,
                    created_by_provider_id,
                    created_by_login,
                    created_by_stable_id,
                    verification_status,
                    last_verified_at_unix_seconds
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, unixepoch())
                ON CONFLICT(repo_id, storage_provider_id, oid, size_bytes)
                DO UPDATE SET
                    backend_id = excluded.backend_id,
                    verification_status = excluded.verification_status,
                    last_verified_at_unix_seconds = excluded.last_verified_at_unix_seconds
                RETURNING
                    id,
                    repo_id,
                    storage_provider_id,
                    oid,
                    size_bytes,
                    backend_id,
                    created_by_provider_id,
                    created_by_login,
                    created_by_stable_id,
                    verification_status,
                    created_at_unix_seconds,
                    last_verified_at_unix_seconds",
                params![
                    repo_id,
                    storage_provider_id,
                    object.oid.as_hex(),
                    size_bytes,
                    backend_id,
                    creator_if_missing.provider_id.as_str(),
                    creator_if_missing.login.as_str(),
                    creator_if_missing.stable_id.as_deref(),
                    verified_status,
                ],
                metadata_object_record_from_row,
            )
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })
    }

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
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })?;
        let rows = statement
            .query_map([now_unix_seconds], metadata_session_record_from_row)
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })?;

        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })
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
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })
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
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })
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
            .map_err(|source| ServerError::MetadataOperation {
                path: self.path.clone(),
                source,
            })
    }

    fn configure_connection(&self) -> ServerResult<()> {
        self.lock_connection()?
            .busy_timeout(SQLITE_BUSY_TIMEOUT)
            .map_err(|source| ServerError::MetadataConfigure {
                path: self.path.clone(),
                source,
            })?;
        self.lock_connection()?
            .pragma_update(None, "foreign_keys", "ON")
            .map_err(|source| ServerError::MetadataConfigure {
                path: self.path.clone(),
                source,
            })?;
        Ok(())
    }

    fn lock_connection(&self) -> ServerResult<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| ServerError::MetadataConnectionPoisoned {
                path: self.path.clone(),
            })
    }
}

#[derive(Debug, thiserror::Error)]
enum MetadataDecodeError {
    #[error("invalid metadata object verification status: {0}")]
    InvalidVerificationStatus(String),
    #[error("invalid metadata object size: {0}")]
    InvalidObjectSize(i64),
}

fn sqlite_size_bytes(object: &LfsObject) -> ServerResult<i64> {
    i64::try_from(object.size.bytes()).map_err(|_| ServerError::InvalidRequest {
        message: format!(
            "LFS object {} size {} exceeds SQLite metadata integer range",
            object.oid, object.size
        ),
    })
}

fn upsert_storage_provider(
    connection: &Connection,
    storage: &StorageProviderConfig,
    path: &Path,
) -> ServerResult<()> {
    let (id, provider_type, backend_root_id, display_name) = match storage {
        StorageProviderConfig::GoogleDrive(storage) => (
            storage.id.as_str(),
            "google_drive",
            storage.root_folder_id.as_str(),
            storage.display_name.as_deref(),
        ),
    };

    connection
        .execute(
            "INSERT INTO storage_providers(
                id,
                provider_type,
                backend_root_id,
                display_name
            ) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id)
            DO UPDATE SET
                provider_type = excluded.provider_type,
                backend_root_id = excluded.backend_root_id,
                display_name = excluded.display_name,
                updated_at_unix_seconds = unixepoch()",
            params![id, provider_type, backend_root_id, display_name],
        )
        .map(|_| ())
        .map_err(|source| ServerError::MetadataOperation {
            path: path.to_path_buf(),
            source,
        })
}

fn upsert_repository_mapping(
    connection: &Connection,
    repository: &RepositoryMapping,
    path: &Path,
) -> ServerResult<()> {
    connection
        .execute(
            "INSERT INTO repository_mappings(
                id,
                repo_provider_id,
                host,
                owner,
                name,
                storage_provider_id,
                route_path,
                is_active
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 1)
            ON CONFLICT(id)
            DO UPDATE SET
                repo_provider_id = excluded.repo_provider_id,
                host = excluded.host,
                owner = excluded.owner,
                name = excluded.name,
                storage_provider_id = excluded.storage_provider_id,
                route_path = excluded.route_path,
                is_active = 1,
                updated_at_unix_seconds = unixepoch()",
            params![
                repository.id.as_str(),
                repository.repo_provider.as_str(),
                repository.host.as_str(),
                repository.owner.as_str(),
                repository.name.as_str(),
                repository.storage_provider.as_str(),
                repository.route_path(),
            ],
        )
        .map(|_| ())
        .map_err(|source| ServerError::MetadataOperation {
            path: path.to_path_buf(),
            source,
        })
}

fn release_inactive_repository_routes(
    connection: &Connection,
    config: &ServerConfig,
    path: &Path,
) -> ServerResult<()> {
    let configured_routes = config
        .repositories
        .iter()
        .map(|repository| (repository.id.as_str(), repository.route_path()))
        .collect::<HashMap<_, _>>();
    let persisted_mappings = {
        let mut statement = connection
            .prepare("SELECT id, route_path FROM repository_mappings")
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?
    };

    for (id, persisted_route) in persisted_mappings {
        let retains_route = configured_routes
            .get(id.as_str())
            .is_some_and(|configured_route| *configured_route == persisted_route);
        if retains_route {
            continue;
        }

        connection
            .execute(
                "UPDATE repository_mappings
                 SET route_path = 'inactive:' || id,
                     is_active = 0,
                     updated_at_unix_seconds = unixepoch()
                 WHERE id = ?1
                   AND (route_path != 'inactive:' || id OR is_active != 0)",
                [&id],
            )
            .map_err(|source| ServerError::MetadataOperation {
                path: path.to_path_buf(),
                source,
            })?;
    }

    Ok(())
}

fn query_optional_object_record(
    connection: &Connection,
    repo_id: &str,
    storage_provider_id: &str,
    object: &LfsObject,
    size_bytes: i64,
    path: &Path,
) -> ServerResult<Option<MetadataObjectRecord>> {
    connection
        .query_row(
            OBJECT_RECORD_BY_IDENTITY_SQL,
            params![
                repo_id,
                storage_provider_id,
                object.oid.as_hex(),
                size_bytes
            ],
            metadata_object_record_from_row,
        )
        .optional()
        .map_err(|source| ServerError::MetadataOperation {
            path: path.to_path_buf(),
            source,
        })
}

fn metadata_object_record_from_row(row: &Row<'_>) -> rusqlite::Result<MetadataObjectRecord> {
    let oid_column_index = row.as_ref().column_index("oid")?;
    let size_column_index = row.as_ref().column_index("size_bytes")?;
    let verification_status_column_index = row.as_ref().column_index("verification_status")?;
    let oid_text = row.get::<_, String>("oid")?;
    let oid = LfsOid::new(&oid_text).map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(oid_column_index, Type::Text, Box::new(source))
    })?;
    let size_bytes = row.get::<_, i64>("size_bytes")?;
    let size = u64::try_from(size_bytes)
        .map(LfsObjectSize::new)
        .map_err(|_| {
            rusqlite::Error::FromSqlConversionFailure(
                size_column_index,
                Type::Integer,
                Box::new(MetadataDecodeError::InvalidObjectSize(size_bytes)),
            )
        })?;
    let verification_status_text = row.get::<_, String>("verification_status")?;
    let verification_status = MetadataObjectVerificationStatus::from_database_str(
        &verification_status_text,
    )
    .map_err(|source| {
        rusqlite::Error::FromSqlConversionFailure(
            verification_status_column_index,
            Type::Text,
            Box::new(source),
        )
    })?;

    Ok(MetadataObjectRecord {
        id: row.get("id")?,
        repo_id: row.get("repo_id")?,
        storage_provider_id: row.get("storage_provider_id")?,
        object: LfsObject::new(oid, size),
        backend_id: row.get("backend_id")?,
        created_by: RepositoryUser::new(
            row.get::<_, String>("created_by_provider_id")?,
            row.get::<_, String>("created_by_login")?,
            row.get("created_by_stable_id")?,
        ),
        created_at_unix_seconds: row.get("created_at_unix_seconds")?,
        last_verified_at_unix_seconds: row.get("last_verified_at_unix_seconds")?,
        verification_status,
    })
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

impl std::fmt::Debug for MetadataDatabase {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MetadataDatabase")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::{LfsObject, LfsObjectSize, LfsOid, RepositoryUser, ServerConfig, ServerError};

    use super::{
        INITIAL_SCHEMA, METADATA_SCHEMA_VERSION, MetadataDatabase,
        MetadataObjectVerificationStatus, NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION,
        PROTECTED_SESSION_TOKEN_MIGRATION,
    };

    const LEGACY_SCHEMA_WITH_NON_NULL_VERIFICATION_TIMESTAMP: &str = r#"
CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    applied_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE storage_providers (
    id TEXT PRIMARY KEY,
    provider_type TEXT NOT NULL,
    backend_root_id TEXT NOT NULL,
    display_name TEXT,
    created_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE repository_mappings (
    id TEXT PRIMARY KEY,
    repo_provider_id TEXT NOT NULL,
    host TEXT NOT NULL,
    owner TEXT NOT NULL,
    name TEXT NOT NULL,
    storage_provider_id TEXT NOT NULL REFERENCES storage_providers(id) ON DELETE RESTRICT,
    route_path TEXT NOT NULL UNIQUE,
    provider_stable_id TEXT,
    created_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    updated_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch())
) STRICT;

CREATE TABLE objects (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id TEXT NOT NULL REFERENCES repository_mappings(id) ON DELETE CASCADE,
    storage_provider_id TEXT NOT NULL REFERENCES storage_providers(id) ON DELETE RESTRICT,
    oid TEXT NOT NULL CHECK (length(oid) = 64),
    size_bytes INTEGER NOT NULL CHECK (size_bytes >= 0),
    backend_id TEXT NOT NULL,
    created_by_provider_id TEXT NOT NULL,
    created_by_login TEXT NOT NULL,
    created_by_stable_id TEXT,
    verification_status TEXT NOT NULL CHECK (
        verification_status IN ('verified', 'stale', 'failed')
    ),
    created_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    last_verified_at_unix_seconds INTEGER NOT NULL DEFAULT (unixepoch()),
    UNIQUE (repo_id, storage_provider_id, oid, size_bytes)
) STRICT;

CREATE INDEX objects_lookup_idx
    ON objects(repo_id, storage_provider_id, oid, size_bytes);
CREATE INDEX objects_backend_idx
    ON objects(storage_provider_id, backend_id);

INSERT INTO schema_migrations(version, name)
VALUES (1, 'initial_metadata_schema');

PRAGMA user_version = 1;
"#;

    #[test]
    fn open_creates_parent_directory_and_runs_initial_schema() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let db_path = directory
            .path()
            .join("state")
            .join(".lfs-cloud")
            .join("metadata.sqlite3");

        let database = MetadataDatabase::open(&db_path).expect("metadata DB should open");

        assert_eq!(database.path(), db_path.as_path());
        assert!(db_path.is_file());
        assert_eq!(
            database
                .schema_version()
                .expect("schema version should load"),
            METADATA_SCHEMA_VERSION
        );
        assert_eq!(
            table_names(&database),
            BTreeSet::from([
                "objects".to_owned(),
                "repository_mappings".to_owned(),
                "schema_migrations".to_owned(),
                "sessions".to_owned(),
                "storage_providers".to_owned(),
                "transfer_attempts".to_owned(),
            ])
        );
    }

    #[test]
    fn migration_runner_is_idempotent() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");

        database
            .run_migrations()
            .expect("second migration run should succeed");

        let migration_count: u32 = database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .query_row("SELECT COUNT(*) FROM schema_migrations", [], |row| {
                row.get(0)
            })
            .expect("migration count should load");
        assert_eq!(migration_count, 4);
        assert_eq!(
            database
                .schema_version()
                .expect("schema version should load"),
            METADATA_SCHEMA_VERSION
        );
    }

    #[test]
    fn migration_upgrades_v1_object_verification_timestamp_to_nullable() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let db_path = directory.path().join("metadata.sqlite3");
        let legacy_connection =
            rusqlite::Connection::open(&db_path).expect("legacy metadata DB should open");
        legacy_connection
            .execute_batch(LEGACY_SCHEMA_WITH_NON_NULL_VERIFICATION_TIMESTAMP)
            .expect("legacy schema should be created");
        drop(legacy_connection);

        let database = MetadataDatabase::open(&db_path).expect("metadata DB should migrate");

        assert_eq!(
            database
                .schema_version()
                .expect("schema version should load"),
            METADATA_SCHEMA_VERSION
        );
        let connection = database
            .connection
            .lock()
            .expect("metadata connection should lock");
        assert!(object_last_verified_column_is_nullable(&connection));
        insert_storage_provider_and_repository_mapping(&connection);
        insert_object_without_verification_timestamp(&connection, 'c', "stale");

        let last_verified_at: Option<i64> = connection
            .query_row(
                "SELECT last_verified_at_unix_seconds
                 FROM objects
                 WHERE oid = ?1",
                ["cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"],
                |row| row.get(0),
            )
            .expect("verification timestamp should load");
        assert_eq!(last_verified_at, None);
    }

    #[test]
    fn migration_upgrades_v3_repository_mappings_as_active() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let db_path = directory.path().join("metadata.sqlite3");
        let legacy_connection =
            rusqlite::Connection::open(&db_path).expect("legacy metadata DB should open");
        legacy_connection
            .execute_batch(INITIAL_SCHEMA)
            .expect("initial schema should be created");
        legacy_connection
            .execute_batch(NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION)
            .expect("version 2 migration should apply");
        legacy_connection
            .execute_batch(PROTECTED_SESSION_TOKEN_MIGRATION)
            .expect("version 3 migration should apply");
        insert_storage_provider_and_repository_mapping(&legacy_connection);
        drop(legacy_connection);

        let database = MetadataDatabase::open(&db_path).expect("metadata DB should migrate");
        assert_eq!(
            database
                .schema_version()
                .expect("schema version should load"),
            METADATA_SCHEMA_VERSION
        );
        let is_active: bool = database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .query_row(
                "SELECT is_active FROM repository_mappings WHERE id = ?1",
                ["github-main:owner/repo"],
                |row| row.get(0),
            )
            .expect("migrated repository activity should load");
        assert!(is_active);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");

        let error = database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .execute(
                "INSERT INTO objects(
                    repo_id,
                    storage_provider_id,
                    oid,
                    size_bytes,
                    backend_id,
                    created_by_provider_id,
                    created_by_login,
                    verification_status
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'verified')",
                (
                    "missing-repo",
                    "missing-storage",
                    "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    42_i64,
                    "drive-file-id",
                    "github-main",
                    "octocat",
                ),
            )
            .expect_err("foreign key enforcement should reject missing parents");

        assert_eq!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
    }

    #[test]
    fn repository_mappings_require_existing_storage_provider() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");

        let missing_storage_error = database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .execute(
                "INSERT INTO repository_mappings(
                    id,
                    repo_provider_id,
                    host,
                    owner,
                    name,
                    storage_provider_id,
                    route_path
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    "github-main:owner/repo",
                    "github-main",
                    "github.com",
                    "owner",
                    "repo",
                    "missing-storage",
                    "/github.com/owner/repo.git/info/lfs",
                ),
            )
            .expect_err("repository mapping should require an existing storage provider");

        assert_eq!(
            missing_storage_error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );

        let connection = database
            .connection
            .lock()
            .expect("metadata connection should lock");
        connection
            .execute(
                "INSERT INTO storage_providers(
                    id,
                    provider_type,
                    backend_root_id
                ) VALUES (?1, ?2, ?3)",
                ("drive-user-a", "google_drive", "drive-root"),
            )
            .expect("storage provider should insert");
        connection
            .execute(
                "INSERT INTO repository_mappings(
                    id,
                    repo_provider_id,
                    host,
                    owner,
                    name,
                    storage_provider_id,
                    route_path
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    "github-main:owner/repo",
                    "github-main",
                    "github.com",
                    "owner",
                    "repo",
                    "drive-user-a",
                    "/github.com/owner/repo.git/info/lfs",
                ),
            )
            .expect("repository mapping should insert after storage provider exists");

        let delete_error = connection
            .execute(
                "DELETE FROM storage_providers WHERE id = ?1",
                ["drive-user-a"],
            )
            .expect_err("mapped storage provider should be delete-restricted");

        assert_eq!(
            delete_error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
    }

    #[test]
    fn newly_inserted_objects_start_without_verification_timestamp() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        let connection = database
            .connection
            .lock()
            .expect("metadata connection should lock");

        insert_storage_provider_and_repository_mapping(&connection);
        insert_object_without_verification_timestamp(&connection, 'b', "stale");

        let last_verified_at: Option<i64> = connection
            .query_row(
                "SELECT last_verified_at_unix_seconds FROM objects",
                [],
                |row| row.get(0),
            )
            .expect("verification timestamp should load");
        assert_eq!(last_verified_at, None);
    }

    #[test]
    fn object_lookup_returns_none_for_missing_metadata() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        insert_storage_provider_and_repository_mapping(
            &database
                .connection
                .lock()
                .expect("metadata connection should lock"),
        );

        let object = lfs_object('d', 42);
        let record = database
            .lookup_object("github-main:owner/repo", "drive-user-a", &object)
            .expect("object lookup should succeed");

        assert_eq!(record, None);
    }

    #[test]
    fn sync_config_upserts_storage_and_repository_parent_rows() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        let config = ServerConfig::load_from_str(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: test-client
    oauth_client_secret: test-secret
storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: google-drive-user-a
    root_folder_id: drive-root
repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
"#,
        )
        .expect("test config should load");
        let object = lfs_object('e', 42);
        let user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));

        database
            .sync_config(&config)
            .expect("metadata config sync should succeed");
        let record = database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-verified",
                &user,
            )
            .expect("verified object should record after config sync");

        assert_eq!(record.repo_id, "github-main:owner/repo");
        assert_eq!(record.storage_provider_id, "drive-user-a");
    }

    #[test]
    fn sync_config_releases_removed_routes_without_deleting_object_history() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        let original_config =
            server_config_with_repository("github-main:owner/archived", "owner", "repo", "8675309");
        database
            .sync_config(&original_config)
            .expect("original metadata config sync should succeed");
        let object = lfs_object('e', 42);
        database
            .record_verified_object(
                "github-main:owner/archived",
                "drive-user-a",
                &object,
                "drive-file-verified",
                &RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned())),
            )
            .expect("verified object should record for original mapping");

        let replacement_config = server_config_with_repository(
            "github-main:owner/replacement",
            "owner",
            "repo",
            "97531",
        );
        database
            .sync_config(&replacement_config)
            .expect("replacement mapping should claim the released route");

        let connection = database
            .connection
            .lock()
            .expect("metadata connection should lock");
        let original_mapping: (String, bool) = connection
            .query_row(
                "SELECT route_path, is_active
                 FROM repository_mappings
                 WHERE id = ?1",
                ["github-main:owner/archived"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("original mapping should remain as history");
        let replacement_mapping: (String, bool) = connection
            .query_row(
                "SELECT route_path, is_active
                 FROM repository_mappings
                 WHERE id = ?1",
                ["github-main:owner/replacement"],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("replacement mapping should be persisted");
        drop(connection);

        assert_eq!(
            original_mapping,
            ("inactive:github-main:owner/archived".to_owned(), false)
        );
        assert_eq!(
            replacement_mapping,
            ("/github.com/owner/repo.git/info/lfs".to_owned(), true)
        );
        assert!(
            database
                .lookup_object("github-main:owner/archived", "drive-user-a", &object)
                .expect("historical object lookup should succeed")
                .is_some()
        );
    }

    #[test]
    fn record_verified_object_inserts_and_loads_metadata() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        insert_storage_provider_and_repository_mapping(
            &database
                .connection
                .lock()
                .expect("metadata connection should lock"),
        );
        let object = lfs_object('e', 42);
        let user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));

        let record = database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-verified",
                &user,
            )
            .expect("verified object should record");

        assert_eq!(record.repo_id, "github-main:owner/repo");
        assert_eq!(record.storage_provider_id, "drive-user-a");
        assert_eq!(record.object, object);
        assert_eq!(record.backend_id, "drive-file-verified");
        assert_eq!(record.created_by, user);
        assert_eq!(
            record.verification_status,
            MetadataObjectVerificationStatus::Verified
        );
        assert!(record.created_at_unix_seconds > 0);
        assert!(record.last_verified_at_unix_seconds.is_some());
        assert_eq!(
            database
                .lookup_object("github-main:owner/repo", "drive-user-a", &object)
                .expect("object lookup should succeed"),
            Some(record)
        );
    }

    #[test]
    fn duplicate_verified_upload_preserves_original_creator() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        insert_storage_provider_and_repository_mapping(
            &database
                .connection
                .lock()
                .expect("metadata connection should lock"),
        );
        let object = lfs_object('f', 42);
        let first_user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));
        let second_user = RepositoryUser::new("github-main", "mona", Some("user-2".to_owned()));
        let first = database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-first",
                &first_user,
            )
            .expect("first verified object should record");

        let second = database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-second",
                &second_user,
            )
            .expect("duplicate verified object should update");

        assert_eq!(first.id, second.id);
        assert_eq!(
            first.created_at_unix_seconds,
            second.created_at_unix_seconds
        );
        assert_eq!(second.backend_id, "drive-file-second");
        assert_eq!(second.created_by, first_user);
        assert_eq!(
            second.verification_status,
            MetadataObjectVerificationStatus::Verified
        );
        assert_eq!(object_row_count(&database), 1);
    }

    #[test]
    fn stale_backend_id_is_repaired_by_verified_upload() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        {
            let connection = database
                .connection
                .lock()
                .expect("metadata connection should lock");
            insert_storage_provider_and_repository_mapping(&connection);
            insert_object_without_verification_timestamp(&connection, 'a', "stale");
        }
        let object = lfs_object('a', 42);
        let stale = database
            .lookup_object("github-main:owner/repo", "drive-user-a", &object)
            .expect("stale object lookup should succeed")
            .expect("stale object should exist");
        assert_eq!(stale.backend_id, "drive-file-id");
        assert_eq!(
            stale.verification_status,
            MetadataObjectVerificationStatus::Stale
        );
        assert_eq!(stale.last_verified_at_unix_seconds, None);

        let user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));
        let repaired = database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-repaired",
                &user,
            )
            .expect("verified upload should repair stale object");

        assert_eq!(repaired.id, stale.id);
        assert_eq!(repaired.backend_id, "drive-file-repaired");
        assert_eq!(
            repaired.created_by,
            RepositoryUser::new("github-main", "octocat", None)
        );
        assert_eq!(
            repaired.verification_status,
            MetadataObjectVerificationStatus::Verified
        );
        assert!(repaired.last_verified_at_unix_seconds.is_some());
        assert_eq!(object_row_count(&database), 1);
    }

    #[test]
    fn object_lookup_decodes_failed_verification_status() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        {
            let connection = database
                .connection
                .lock()
                .expect("metadata connection should lock");
            insert_storage_provider_and_repository_mapping(&connection);
            insert_object_without_verification_timestamp(&connection, '1', "failed");
        }

        let record = database
            .lookup_object(
                "github-main:owner/repo",
                "drive-user-a",
                &lfs_object('1', 42),
            )
            .expect("failed object lookup should decode")
            .expect("failed object should exist");

        assert_eq!(
            record.verification_status,
            MetadataObjectVerificationStatus::Failed
        );
    }

    #[test]
    fn object_lookup_rejects_invalid_verification_status() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        {
            let connection = database
                .connection
                .lock()
                .expect("metadata connection should lock");
            insert_storage_provider_and_repository_mapping(&connection);
            connection
                .pragma_update(None, "ignore_check_constraints", "ON")
                .expect("constraint checks should be disabled for corrupt fixture");
            insert_object_without_verification_timestamp(&connection, '2', "corrupt");
            connection
                .pragma_update(None, "ignore_check_constraints", "OFF")
                .expect("constraint checks should be restored");
        }

        let error = database
            .lookup_object(
                "github-main:owner/repo",
                "drive-user-a",
                &lfs_object('2', 42),
            )
            .expect_err("invalid status should fail decode");

        match error {
            ServerError::MetadataOperation { source, .. } => match source {
                rusqlite::Error::FromSqlConversionFailure(_, rusqlite::types::Type::Text, _) => {}
                other => panic!("expected status conversion failure, got {other:?}"),
            },
            other => panic!("expected metadata operation error, got {other:?}"),
        }
    }

    #[test]
    fn metadata_database_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<MetadataDatabase>();
    }

    fn table_names(database: &MetadataDatabase) -> BTreeSet<String> {
        let connection = database
            .connection
            .lock()
            .expect("metadata connection should lock");
        let mut statement = connection
            .prepare(
                "SELECT name
                 FROM sqlite_schema
                 WHERE type = 'table'
                   AND name NOT LIKE 'sqlite_%'",
            )
            .expect("table list statement should prepare");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table list should execute");

        rows.map(|row| row.expect("table name should load"))
            .collect()
    }

    fn insert_storage_provider_and_repository_mapping(connection: &rusqlite::Connection) {
        connection
            .execute(
                "INSERT INTO storage_providers(
                    id,
                    provider_type,
                    backend_root_id
                ) VALUES (?1, ?2, ?3)",
                ("drive-user-a", "google_drive", "drive-root"),
            )
            .expect("storage provider should insert");
        connection
            .execute(
                "INSERT INTO repository_mappings(
                    id,
                    repo_provider_id,
                    host,
                    owner,
                    name,
                    storage_provider_id,
                    route_path
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                (
                    "github-main:owner/repo",
                    "github-main",
                    "github.com",
                    "owner",
                    "repo",
                    "drive-user-a",
                    "/github.com/owner/repo.git/info/lfs",
                ),
            )
            .expect("repository mapping should insert");
    }

    fn object_row_count(database: &MetadataDatabase) -> u32 {
        database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .expect("object row count should load")
    }

    fn lfs_object(oid_character: char, size_bytes: u64) -> LfsObject {
        LfsObject::new(
            LfsOid::new(oid_character.to_string().repeat(64)).expect("fixture OID should be valid"),
            LfsObjectSize::new(size_bytes),
        )
    }

    fn server_config_with_repository(
        repository_id: &str,
        owner: &str,
        name: &str,
        provider_repository_id: &str,
    ) -> ServerConfig {
        ServerConfig::load_from_str(&format!(
            r#"
server:
  public_url: http://127.0.0.1:8080
repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: test-client
    oauth_client_secret: test-secret
storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: google-drive-user-a
    root_folder_id: drive-root
repositories:
  - id: {repository_id}
    repo_provider: github-main
    host: github.com
    owner: {owner}
    name: {name}
    provider_repository_id: "{provider_repository_id}"
    storage_provider: drive-user-a
"#
        ))
        .expect("test config should load")
    }

    fn insert_object_without_verification_timestamp(
        connection: &rusqlite::Connection,
        oid_character: char,
        verification_status: &str,
    ) {
        let oid = oid_character.to_string().repeat(64);
        connection
            .execute(
                "INSERT INTO objects(
                    repo_id,
                    storage_provider_id,
                    oid,
                    size_bytes,
                    backend_id,
                    created_by_provider_id,
                    created_by_login,
                    verification_status
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                (
                    "github-main:owner/repo",
                    "drive-user-a",
                    oid,
                    42_i64,
                    "drive-file-id",
                    "github-main",
                    "octocat",
                    verification_status,
                ),
            )
            .expect("object should insert");
    }

    fn object_last_verified_column_is_nullable(connection: &rusqlite::Connection) -> bool {
        let mut statement = connection
            .prepare("PRAGMA table_info(objects)")
            .expect("table info statement should prepare");
        let columns = statement
            .query_map([], |row| {
                let name = row.get::<_, String>(1)?;
                let not_null = row.get::<_, bool>(3)?;
                Ok((name, not_null))
            })
            .expect("table info should execute");

        columns
            .map(|column| column.expect("column info should load"))
            .find_map(|(name, not_null)| {
                (name == "last_verified_at_unix_seconds").then_some(!not_null)
            })
            .expect("last verification column should exist")
    }
}
