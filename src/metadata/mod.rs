//! SQLite metadata database setup and schema migrations.
//!
//! The metadata database is server-owned state. It records repository routing,
//! storage-provider records, object mappings, local LFS sessions, and transfer
//! attempts without becoming part of any Git repository's committed config.

mod configuration;
mod instance;
mod migrations;
mod objects;
mod process_lock;
mod sessions;
mod transfers;
mod upload_locks;

use std::{
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use rusqlite::Connection;

use crate::{LfsObject, ServerError, ServerResult};

pub use migrations::METADATA_SCHEMA_VERSION;
pub use objects::{MetadataObjectRecord, MetadataObjectVerificationStatus};
pub(crate) use process_lock::ServerProcessLock;
pub(crate) use sessions::MetadataSessionRecord;
pub(crate) use transfers::{MetadataTransferOperation, MetadataTransferResult};
#[allow(unused_imports)]
pub(crate) use upload_locks::MetadataObjectUploadLock;

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
    async fn run_blocking<T>(
        self: &Arc<Self>,
        operation: impl FnOnce(&Self) -> ServerResult<T> + Send + 'static,
    ) -> ServerResult<T>
    where
        T: Send + 'static,
    {
        let database = Arc::clone(self);
        let path = self.path.clone();
        tokio::task::spawn_blocking(move || operation(database.as_ref()))
            .await
            .map_err(|source| ServerError::MetadataTaskJoin { path, source })?
    }
    fn operation_error(&self, source: rusqlite::Error) -> ServerError {
        ServerError::MetadataOperation {
            path: self.path.clone(),
            source,
        }
    }

    fn lock_connection(&self) -> ServerResult<MutexGuard<'_, Connection>> {
        self.connection
            .lock()
            .map_err(|_| ServerError::MetadataConnectionPoisoned {
                path: self.path.clone(),
            })
    }
}

fn sqlite_size_bytes(object: &LfsObject) -> ServerResult<i64> {
    i64::try_from(object.size.bytes()).map_err(|_| ServerError::InvalidRequest {
        message: format!(
            "LFS object {} size {} exceeds SQLite metadata integer range",
            object.oid, object.size
        ),
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
    use super::MetadataDatabase;

    #[test]
    fn metadata_database_is_send_and_sync() {
        fn assert_send_sync<T: Send + Sync>() {}

        assert_send_sync::<MetadataDatabase>();
    }
}
