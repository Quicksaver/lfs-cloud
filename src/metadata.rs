//! SQLite metadata database setup and schema migrations.
//!
//! The metadata database is server-owned state. It records repository routing,
//! storage-provider records, object mappings, local LFS sessions, and transfer
//! attempts without becoming part of any Git repository's committed config.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Duration,
};

use rusqlite::Connection;

use crate::{ServerError, ServerResult};

/// Current metadata schema version installed by the migration runner.
pub const METADATA_SCHEMA_VERSION: u32 = 2;

const SQLITE_BUSY_TIMEOUT: Duration = Duration::from_secs(5);

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

    use super::{METADATA_SCHEMA_VERSION, MetadataDatabase};

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
        assert_eq!(migration_count, 2);
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
