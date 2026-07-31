//! Cross-process striped locks for repository-scoped object uploads.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

use sha2::{Digest, Sha256};

use crate::{LfsObject, ServerError, ServerResult};

use super::MetadataDatabase;

const OBJECT_UPLOAD_LOCK_STRIPES: usize = 64;

/// An operating-system-backed exclusive lock for one object upload.
///
/// The lock file lives beside the metadata database and remains locked for the
/// lifetime of this guard. Closing the file releases the lock even when the
/// process exits unexpectedly.
pub(crate) struct MetadataObjectUploadLock {
    _file: fs::File,
}

impl MetadataDatabase {
    /// Acquires the cross-process upload lock for one repository-scoped object.
    ///
    /// Production server processes that share a metadata path also share these
    /// lock files. The blocking filesystem-lock wait runs outside Tokio worker
    /// threads. In-memory databases retain only the server state's in-process
    /// lock because they have no durable path to coordinate through.
    pub(crate) async fn acquire_object_upload_lock(
        self: &Arc<Self>,
        repo_id: String,
        storage_provider_id: String,
        object: LfsObject,
    ) -> ServerResult<Option<MetadataObjectUploadLock>> {
        if self.path == Path::new(":memory:") {
            return Ok(None);
        }

        let lock_path = self.object_upload_lock_path(&repo_id, &storage_provider_id, &object);
        let lock_directory = lock_path
            .parent()
            .expect("object upload lock path should have a parent")
            .to_path_buf();
        let provider = storage_provider_id;
        let task_path = self.path.clone();
        tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&lock_directory).map_err(|source| ServerError::Storage {
                source: crate::StorageError::Retryable {
                    provider: provider.clone(),
                    message: format!(
                        "durable object upload lock directory could not be created: {source}"
                    ),
                },
            })?;
            let file = OpenOptions::new()
                .create(true)
                .truncate(false)
                .read(true)
                .write(true)
                .open(&lock_path)
                .map_err(|source| ServerError::Storage {
                    source: crate::StorageError::Retryable {
                        provider: provider.clone(),
                        message: format!(
                            "durable object upload lock could not be opened: {source}"
                        ),
                    },
                })?;
            fs4::FileExt::lock(&file).map_err(|source| ServerError::Storage {
                source: crate::StorageError::Retryable {
                    provider,
                    message: format!("durable object upload lock could not be acquired: {source}"),
                },
            })?;

            Ok(Some(MetadataObjectUploadLock { _file: file }))
        })
        .await
        .map_err(|source| ServerError::MetadataTaskJoin {
            path: task_path,
            source,
        })?
    }

    fn object_upload_lock_path(
        &self,
        repo_id: &str,
        storage_provider_id: &str,
        object: &LfsObject,
    ) -> PathBuf {
        let mut digest = Sha256::new();
        for component in [repo_id, storage_provider_id, object.oid.as_hex()] {
            digest.update(
                u64::try_from(component.len())
                    .expect("validated upload lock identity should fit u64")
                    .to_be_bytes(),
            );
            digest.update(component.as_bytes());
        }
        digest.update(object.size.bytes().to_be_bytes());
        let digest = digest.finalize();
        // Lock files cannot be deleted safely after unlocking because a waiter
        // may still hold the old inode while another process creates a new one.
        // A fixed stripe set bounds persistent files without globally
        // serializing unrelated uploads.
        let stripe = usize::from(digest[0]) % OBJECT_UPLOAD_LOCK_STRIPES;
        let file_name = format!("{stripe:02x}.lock");
        self.path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .join("upload-locks")
            .join(file_name)
    }
}
