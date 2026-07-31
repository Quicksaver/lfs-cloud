//! Repository-scoped object records, verification state, and object persistence.

use std::{fmt, path::Path, sync::Arc};

use rusqlite::{Connection, OptionalExtension, Row, params, types::Type};

use crate::{LfsObject, LfsObjectSize, LfsOid, RepositoryUser, ServerError, ServerResult};

use super::{MetadataDatabase, sqlite_size_bytes};

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

impl MetadataDatabase {
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

    /// Looks up object metadata without blocking an async runtime worker.
    pub(crate) async fn lookup_object_async(
        self: &Arc<Self>,
        repo_id: String,
        storage_provider_id: String,
        object: LfsObject,
    ) -> ServerResult<Option<MetadataObjectRecord>> {
        self.run_blocking(move |database| {
            database.lookup_object(&repo_id, &storage_provider_id, &object)
        })
        .await
    }

    /// Marks a backend mapping stale if it still points at the expected ID.
    ///
    /// Comparing the backend ID makes repair safe against a concurrent upload
    /// that has already installed a newer verified mapping.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when SQLite cannot update the metadata row.
    pub(crate) fn mark_object_stale(
        &self,
        repo_id: &str,
        storage_provider_id: &str,
        object: &LfsObject,
        expected_backend_id: &str,
    ) -> ServerResult<bool> {
        let size_bytes = sqlite_size_bytes(object)?;
        let connection = self.lock_connection()?;
        let updated = connection
            .execute(
                "UPDATE objects
                 SET verification_status = ?1,
                     last_verified_at_unix_seconds = NULL
                 WHERE repo_id = ?2
                   AND storage_provider_id = ?3
                   AND oid = ?4
                   AND size_bytes = ?5
                   AND backend_id = ?6",
                params![
                    MetadataObjectVerificationStatus::Stale.as_database_str(),
                    repo_id,
                    storage_provider_id,
                    object.oid.as_hex(),
                    size_bytes,
                    expected_backend_id,
                ],
            )
            .map_err(|source| self.operation_error(source))?;
        Ok(updated == 1)
    }

    /// Marks a missing backend mapping stale off the async runtime worker.
    pub(crate) async fn mark_object_stale_async(
        self: &Arc<Self>,
        repo_id: String,
        storage_provider_id: String,
        object: LfsObject,
        expected_backend_id: String,
    ) -> ServerResult<bool> {
        self.run_blocking(move |database| {
            database.mark_object_stale(
                &repo_id,
                &storage_provider_id,
                &object,
                &expected_backend_id,
            )
        })
        .await
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
            .map_err(|source| self.operation_error(source))
    }

    /// Records verified object metadata without blocking an async runtime worker.
    ///
    /// Request handlers use this boundary because SQLite and the connection
    /// mutex are synchronous. Owned inputs let the complete operation run on
    /// Tokio's blocking pool while retaining the database's serialized access.
    pub(crate) async fn record_verified_object_async(
        self: &Arc<Self>,
        repo_id: String,
        storage_provider_id: String,
        object: LfsObject,
        backend_id: String,
        creator_if_missing: RepositoryUser,
    ) -> ServerResult<MetadataObjectRecord> {
        self.run_blocking(move |database| {
            database.record_verified_object(
                &repo_id,
                &storage_provider_id,
                &object,
                &backend_id,
                &creator_if_missing,
            )
        })
        .await
    }
}

#[derive(Debug, thiserror::Error)]
enum MetadataDecodeError {
    #[error("invalid metadata object verification status: {0}")]
    InvalidVerificationStatus(String),
    #[error("invalid metadata object size: {0}")]
    InvalidObjectSize(i64),
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

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use crate::{RepositoryUser, ServerError};

    use super::test_support::*;
    use super::*;
    use crate::metadata::{
        configuration::server_config_with_repository, migrations::SQLITE_BUSY_TIMEOUT,
    };

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

    #[tokio::test(flavor = "current_thread")]
    async fn async_verified_object_write_does_not_block_runtime_worker() {
        let directory = tempfile::tempdir().expect("tempdir should be created");
        let database_path = directory.path().join("metadata.sqlite3");
        let database =
            Arc::new(MetadataDatabase::open(&database_path).expect("metadata DB should open"));
        database
            .sync_config(&server_config_with_repository(
                "github-main:owner/repo",
                "owner",
                "repo",
                "8675309",
            ))
            .expect("metadata config sync should succeed");
        let lock_connection =
            rusqlite::Connection::open(&database_path).expect("lock connection should open");
        lock_connection
            .execute_batch("BEGIN EXCLUSIVE")
            .expect("exclusive transaction should start");

        let write = database.record_verified_object_async(
            "github-main:owner/repo".to_owned(),
            "drive-user-a".to_owned(),
            lfs_object('f', 42),
            "drive-file-verified".to_owned(),
            RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned())),
        );
        tokio::pin!(write);
        tokio::select! {
            () = tokio::time::sleep(Duration::from_millis(50)) => {}
            result = &mut write => {
                panic!("metadata write completed before the lock was released: {result:?}");
            }
        }

        lock_connection
            .execute_batch("COMMIT")
            .expect("exclusive transaction should commit");
        // A connection already inside SQLite's busy handler may take up to the
        // configured busy timeout to observe the released lock on loaded CI
        // runners. The earlier timer is the assertion that the runtime worker
        // itself remains responsive while this blocking work is pending.
        let record = tokio::time::timeout(SQLITE_BUSY_TIMEOUT + Duration::from_secs(1), write)
            .await
            .expect("metadata write should finish after lock release")
            .expect("metadata write should succeed");
        assert_eq!(record.backend_id, "drive-file-verified");
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
    fn missing_backend_id_is_marked_stale_only_while_mapping_is_unchanged() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        insert_storage_provider_and_repository_mapping(
            &database
                .connection
                .lock()
                .expect("metadata connection should lock"),
        );
        let object = lfs_object('a', 42);
        let user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));
        database
            .record_verified_object(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                "drive-file-current",
                &user,
            )
            .expect("verified object should record");

        assert!(
            !database
                .mark_object_stale(
                    "github-main:owner/repo",
                    "drive-user-a",
                    &object,
                    "drive-file-replaced",
                )
                .expect("obsolete repair should be ignored")
        );
        assert!(
            database
                .mark_object_stale(
                    "github-main:owner/repo",
                    "drive-user-a",
                    &object,
                    "drive-file-current",
                )
                .expect("current missing backend should become stale")
        );

        let record = database
            .lookup_object("github-main:owner/repo", "drive-user-a", &object)
            .expect("stale object lookup should succeed")
            .expect("stale object should remain recorded");
        assert_eq!(
            record.verification_status,
            MetadataObjectVerificationStatus::Stale
        );
        assert_eq!(record.last_verified_at_unix_seconds, None);
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
}

#[cfg(test)]
pub(super) mod test_support {
    use crate::{LfsObject, LfsObjectSize, LfsOid};

    use super::MetadataDatabase;

    pub(crate) fn insert_storage_provider_and_repository_mapping(
        connection: &rusqlite::Connection,
    ) {
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

    pub(super) fn object_row_count(database: &MetadataDatabase) -> u32 {
        database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
            .expect("object row count should load")
    }
    pub(crate) fn lfs_object(oid_character: char, size_bytes: u64) -> LfsObject {
        LfsObject::new(
            LfsOid::new(oid_character.to_string().repeat(64)).expect("fixture OID should be valid"),
            LfsObjectSize::new(size_bytes),
        )
    }
    pub(crate) fn insert_object_without_verification_timestamp(
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
}
