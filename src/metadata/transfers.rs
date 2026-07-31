//! Durable lifecycle rows for authenticated object transfers.

use std::sync::Arc;

use rusqlite::params;

#[allow(unused_imports)]
use crate::{
    ErrorCategory, LfsObject, RepositoryUser, SanitizedMessage, ServerError, ServerResult,
};

use super::{MetadataDatabase, sqlite_size_bytes};

/// Transfer operation represented by a durable lifecycle row.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MetadataTransferOperation {
    /// A Git LFS object upload.
    Upload,
    /// A Git LFS object download.
    Download,
}

impl MetadataTransferOperation {
    fn as_database_str(self) -> &'static str {
        match self {
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

/// Terminal result written to a started transfer-attempt row.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MetadataTransferResult {
    /// The storage operation completed and may have returned a backend ID.
    Succeeded {
        /// Backend identifier associated with the completed transfer.
        backend_id: Option<String>,
    },
    /// The transfer failed with a safe, caller-sanitized diagnostic.
    Failed {
        /// Domain responsible for the failure.
        category: ErrorCategory,
        /// Secret-free diagnostic suitable for durable operator inspection.
        message: SanitizedMessage,
    },
}

impl MetadataTransferResult {
    /// Creates a successful terminal result.
    pub(crate) fn succeeded(backend_id: Option<String>) -> Self {
        Self::Succeeded { backend_id }
    }

    /// Creates a failed terminal result from an already sanitized message.
    pub(crate) fn failed(category: ErrorCategory, message: SanitizedMessage) -> Self {
        Self::Failed { category, message }
    }
}

impl MetadataDatabase {
    /// Inserts the started row for one authenticated object transfer.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the object size cannot be represented by
    /// SQLite or the configured parent rows reject the attempt.
    pub(crate) fn start_transfer_attempt(
        &self,
        repo_id: &str,
        storage_provider_id: &str,
        object: &LfsObject,
        operation: MetadataTransferOperation,
        user: &RepositoryUser,
    ) -> ServerResult<i64> {
        let size_bytes = sqlite_size_bytes(object)?;
        let connection = self.lock_connection()?;
        connection
            .query_row(
                "INSERT INTO transfer_attempts(
                    repo_id,
                    storage_provider_id,
                    oid,
                    size_bytes,
                    operation,
                    status,
                    user_provider_id,
                    user_login
                ) VALUES (?1, ?2, ?3, ?4, ?5, 'started', ?6, ?7)
                RETURNING id",
                params![
                    repo_id,
                    storage_provider_id,
                    object.oid.as_hex(),
                    size_bytes,
                    operation.as_database_str(),
                    user.provider_id.as_str(),
                    user.login.as_str(),
                ],
                |row| row.get(0),
            )
            .map_err(|source| self.operation_error(source))
    }

    /// Inserts a transfer start row without blocking an async runtime worker.
    pub(crate) async fn start_transfer_attempt_async(
        self: &Arc<Self>,
        repo_id: String,
        storage_provider_id: String,
        object: LfsObject,
        operation: MetadataTransferOperation,
        user: RepositoryUser,
    ) -> ServerResult<i64> {
        self.run_blocking(move |database| {
            database.start_transfer_attempt(
                &repo_id,
                &storage_provider_id,
                &object,
                operation,
                &user,
            )
        })
        .await
    }

    /// Completes one started transfer-attempt row exactly once.
    ///
    /// Successful rows retain only a backend ID. Failed rows retain only the
    /// caller-provided category and sanitized message, preventing raw provider
    /// errors or credentials from crossing the durable metadata boundary.
    ///
    /// # Errors
    ///
    /// Returns [`ServerError`] when the attempt does not exist, was already
    /// completed, or SQLite cannot update it.
    pub(crate) fn finish_transfer_attempt(
        &self,
        attempt_id: i64,
        result: &MetadataTransferResult,
    ) -> ServerResult<()> {
        let (status, backend_id, error_category, error_message) = match result {
            MetadataTransferResult::Succeeded { backend_id } => {
                ("succeeded", backend_id.as_deref(), None, None)
            }
            MetadataTransferResult::Failed { category, message } => (
                "failed",
                None,
                Some(category.to_string()),
                Some(message.as_str()),
            ),
        };
        let connection = self.lock_connection()?;
        connection
            .query_row(
                "UPDATE transfer_attempts
                 SET status = ?2,
                     backend_id = ?3,
                     error_category = ?4,
                     error_message = ?5,
                     finished_at_unix_seconds = unixepoch()
                 WHERE id = ?1
                   AND status = 'started'
                 RETURNING id",
                params![
                    attempt_id,
                    status,
                    backend_id,
                    error_category,
                    error_message,
                ],
                |_| Ok(()),
            )
            .map_err(|source| self.operation_error(source))
    }

    /// Completes a transfer-attempt row without blocking an async worker.
    pub(crate) async fn finish_transfer_attempt_async(
        self: &Arc<Self>,
        attempt_id: i64,
        result: MetadataTransferResult,
    ) -> ServerResult<()> {
        self.run_blocking(move |database| database.finish_transfer_attempt(attempt_id, &result))
            .await
    }
}

#[cfg(test)]
mod tests {
    use crate::{ErrorCategory, RepositoryUser, SanitizedMessage};

    use super::*;
    use crate::metadata::objects::test_support::{
        insert_storage_provider_and_repository_mapping, lfs_object,
    };

    #[test]
    fn transfer_attempt_records_started_and_successful_lifecycle() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        insert_storage_provider_and_repository_mapping(
            &database
                .connection
                .lock()
                .expect("metadata connection should lock"),
        );
        let object = lfs_object('a', 42);
        let user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));

        let attempt_id = database
            .start_transfer_attempt(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                MetadataTransferOperation::Upload,
                &user,
            )
            .expect("transfer attempt should start");

        let started = transfer_attempt_row(&database, attempt_id);
        assert_eq!(started.0, "upload");
        assert_eq!(started.1, "started");
        assert_eq!(started.2, "github-main");
        assert_eq!(started.3, "octocat");
        assert_eq!(started.4, None);
        assert_eq!(started.5, None);
        assert_eq!(started.6, None);
        assert!(started.7.is_none());

        database
            .finish_transfer_attempt(
                attempt_id,
                &MetadataTransferResult::Succeeded {
                    backend_id: Some("drive-file-verified".to_owned()),
                },
            )
            .expect("transfer attempt should finish");

        let succeeded = transfer_attempt_row(&database, attempt_id);
        assert_eq!(succeeded.1, "succeeded");
        assert_eq!(succeeded.4.as_deref(), Some("drive-file-verified"));
        assert_eq!(succeeded.5, None);
        assert_eq!(succeeded.6, None);
        assert!(succeeded.7.is_some());
    }

    #[test]
    fn transfer_attempt_records_only_caller_sanitized_failure_details() {
        let database = MetadataDatabase::open_in_memory().expect("metadata DB should open");
        insert_storage_provider_and_repository_mapping(
            &database
                .connection
                .lock()
                .expect("metadata connection should lock"),
        );
        let object = lfs_object('b', 84);
        let user = RepositoryUser::new("github-main", "octocat", Some("user-1".to_owned()));
        let attempt_id = database
            .start_transfer_attempt(
                "github-main:owner/repo",
                "drive-user-a",
                &object,
                MetadataTransferOperation::Download,
                &user,
            )
            .expect("transfer attempt should start");

        database
            .finish_transfer_attempt(
                attempt_id,
                &MetadataTransferResult::Failed {
                    category: ErrorCategory::Storage,
                    message: SanitizedMessage::new("object storage read failed"),
                },
            )
            .expect("transfer attempt should finish");

        let failed = transfer_attempt_row(&database, attempt_id);
        assert_eq!(failed.0, "download");
        assert_eq!(failed.1, "failed");
        assert_eq!(failed.4, None);
        assert_eq!(failed.5.as_deref(), Some("storage"));
        assert_eq!(failed.6.as_deref(), Some("object storage read failed"));
        assert!(failed.7.is_some());
    }

    #[allow(clippy::type_complexity)]
    fn transfer_attempt_row(
        database: &MetadataDatabase,
        attempt_id: i64,
    ) -> (
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    ) {
        database
            .connection
            .lock()
            .expect("metadata connection should lock")
            .query_row(
                "SELECT
                    operation,
                    status,
                    user_provider_id,
                    user_login,
                    backend_id,
                    error_category,
                    error_message,
                    finished_at_unix_seconds
                 FROM transfer_attempts
                 WHERE id = ?1",
                [attempt_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                    ))
                },
            )
            .expect("transfer attempt row should load")
    }
}
