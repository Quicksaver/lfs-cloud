//! SQLite connection setup and ordered schema migrations.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Duration,
};

use rusqlite::Connection;

use crate::{ServerError, ServerResult};

use super::MetadataDatabase;

/// Current metadata schema version installed by the migration runner.
pub const METADATA_SCHEMA_VERSION: u32 = 6;

pub(super) const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);
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

const PAT_AUTHENTICATION_SESSION_MIGRATION: &str = r#"
DELETE FROM sessions;

INSERT OR IGNORE INTO schema_migrations(version, name)
VALUES (5, 'invalidate_oauth_sessions_for_pat_authentication');

PRAGMA user_version = 5;
"#;

const SESSION_ENCRYPTION_SECRET_MIGRATION: &str = r#"
DELETE FROM sessions;

INSERT OR IGNORE INTO schema_migrations(version, name)
VALUES (6, 'invalidate_provider_pat_sessions_for_server_secret');

PRAGMA user_version = 6;
"#;

const METADATA_MIGRATIONS: &[(u32, &str)] = &[
    (2, NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION),
    (3, PROTECTED_SESSION_TOKEN_MIGRATION),
    (4, ACTIVE_REPOSITORY_MAPPING_MIGRATION),
    // OAuth sessions used the removed client-secret key and must be invalidated
    // before the PAT-derived session key can load current durable rows.
    (5, PAT_AUTHENTICATION_SESSION_MIGRATION),
    // Provider-PAT sessions use different encryption material from the new
    // dedicated server secret and must be removed before durable loading.
    (6, SESSION_ENCRYPTION_SECRET_MIGRATION),
];

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
        let schema_version = connection
            .query_row("PRAGMA user_version", [], |row| row.get::<_, u32>(0))
            .map_err(|source| self.migration_error(source))?;
        if schema_version > METADATA_SCHEMA_VERSION {
            return Err(ServerError::MetadataSchemaTooNew {
                path: self.path.clone(),
                found: schema_version,
                supported: METADATA_SCHEMA_VERSION,
            });
        }
        let transaction = connection
            .transaction()
            .map_err(|source| self.migration_error(source))?;
        transaction
            .execute_batch(INITIAL_SCHEMA)
            .map_err(|source| self.migration_error(source))?;
        for &(version, migration) in METADATA_MIGRATIONS {
            if schema_version >= version {
                continue;
            }
            transaction
                .execute_batch(migration)
                .map_err(|source| self.migration_error(source))?;
        }
        transaction
            .commit()
            .map_err(|source| self.migration_error(source))
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
    fn migration_error(&self, source: rusqlite::Error) -> ServerError {
        ServerError::MetadataMigration {
            path: self.path.clone(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use crate::ServerError;

    use super::*;
    use crate::metadata::objects::test_support::{
        insert_object_without_verification_timestamp,
        insert_storage_provider_and_repository_mapping,
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
    fn migration_table_is_strictly_ordered_and_current() {
        assert!(
            METADATA_MIGRATIONS
                .windows(2)
                .all(|versions| versions[0].0 < versions[1].0),
            "metadata migrations must be ordered by unique ascending version"
        );
        assert_eq!(
            METADATA_MIGRATIONS.last().map(|(version, _)| *version),
            Some(METADATA_SCHEMA_VERSION),
            "the final migration must install the current schema version"
        );
    }

    #[test]
    fn open_creates_parent_directory_and_runs_initial_schema() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let db_path = directory
            .path()
            .join("state")
            .join(".lfscloud")
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
        assert_eq!(migration_count, 6);
        assert_eq!(
            database
                .schema_version()
                .expect("schema version should load"),
            METADATA_SCHEMA_VERSION
        );
    }

    #[test]
    fn migration_rejects_future_schema_without_modifying_it() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let db_path = directory.path().join("future-metadata.sqlite3");
        let future_schema_version = METADATA_SCHEMA_VERSION + 1;
        let future_connection =
            rusqlite::Connection::open(&db_path).expect("future metadata DB should open");
        future_connection
            .execute_batch(
                "CREATE TABLE future_metadata(value TEXT NOT NULL) STRICT;
                 INSERT INTO future_metadata(value) VALUES ('preserve-me');",
            )
            .expect("future metadata schema should be created");
        future_connection
            .pragma_update(None, "user_version", future_schema_version)
            .expect("future schema version should be set");
        drop(future_connection);

        let error = MetadataDatabase::open(&db_path)
            .expect_err("future metadata schema should be rejected");

        assert!(matches!(
            error,
            ServerError::MetadataSchemaTooNew {
                path,
                found,
                supported: METADATA_SCHEMA_VERSION,
            } if path == db_path && found == future_schema_version
        ));
        let unchanged_connection =
            rusqlite::Connection::open(&db_path).expect("future metadata DB should reopen");
        let schema_version: u32 = unchanged_connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("future schema version should load");
        assert_eq!(schema_version, future_schema_version);
        let table_names = unchanged_connection
            .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
            .expect("table query should prepare")
            .query_map([], |row| row.get::<_, String>(0))
            .expect("table query should run")
            .collect::<Result<BTreeSet<_>, _>>()
            .expect("table names should load");
        assert_eq!(table_names, BTreeSet::from(["future_metadata".to_owned()]));
        let preserved_value: String = unchanged_connection
            .query_row("SELECT value FROM future_metadata", [], |row| row.get(0))
            .expect("future metadata value should load");
        assert_eq!(preserved_value, "preserve-me");
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
    fn migration_invalidates_sessions_from_removed_oauth_authentication() {
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
        legacy_connection
            .execute_batch(ACTIVE_REPOSITORY_MAPPING_MIGRATION)
            .expect("version 4 migration should apply");
        legacy_connection
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
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    "a".repeat(64),
                    "github-main",
                    "octocat",
                    "42",
                    "[\"repo\"]",
                    1_700_000_000_i64,
                    4_000_000_000_i64,
                    vec![1_u8, 2, 3],
                    vec![4_u8, 5, 6],
                ],
            )
            .expect("legacy OAuth session should be inserted");
        drop(legacy_connection);

        let database = MetadataDatabase::open(&db_path).expect("metadata DB should migrate");
        let session_count: u32 = database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("session count should load");

        assert_eq!(session_count, 0);
        assert_eq!(
            database
                .schema_version()
                .expect("schema version should load"),
            METADATA_SCHEMA_VERSION
        );
    }

    #[test]
    fn migration_invalidates_sessions_encrypted_with_legacy_provider_pat() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let db_path = directory.path().join("metadata.sqlite3");
        let legacy_connection =
            rusqlite::Connection::open(&db_path).expect("legacy metadata DB should open");
        legacy_connection
            .execute_batch(INITIAL_SCHEMA)
            .expect("initial schema should be created");
        for migration in [
            NULLABLE_OBJECT_VERIFICATION_TIMESTAMP_MIGRATION,
            PROTECTED_SESSION_TOKEN_MIGRATION,
            ACTIVE_REPOSITORY_MAPPING_MIGRATION,
            PAT_AUTHENTICATION_SESSION_MIGRATION,
        ] {
            legacy_connection
                .execute_batch(migration)
                .expect("legacy migration should apply");
        }
        legacy_connection
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
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                rusqlite::params![
                    "b".repeat(64),
                    "github-main",
                    "octocat",
                    "42",
                    "[\"repo\"]",
                    1_700_000_000_i64,
                    4_000_000_000_i64,
                    vec![1_u8, 2, 3],
                    vec![4_u8, 5, 6],
                ],
            )
            .expect("provider-PAT-encrypted session should be inserted");
        drop(legacy_connection);

        let database = MetadataDatabase::open(&db_path).expect("metadata DB should migrate");
        let session_count: u32 = database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))
            .expect("session count should load");

        assert_eq!(session_count, 0);
        assert_eq!(
            database
                .schema_version()
                .expect("schema version should load"),
            METADATA_SCHEMA_VERSION
        );
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
