//! Cross-process exclusion for one running server or session-key rotation.

use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use crate::{ServerError, ServerResult};

/// Exclusive process guard associated with one metadata database.
#[derive(Debug)]
pub(crate) struct ServerProcessLock {
    _file: fs::File,
}

impl ServerProcessLock {
    /// Acquires the server lifecycle lock without waiting.
    ///
    /// Holding this guard prevents another server process or a session-key
    /// rotation command from observing stale in-memory session state.
    pub(crate) fn acquire(metadata_path: &Path) -> ServerResult<Self> {
        let lock_path = lock_path(metadata_path);
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent).map_err(|source| ServerError::ServerLock {
                path: lock_path.clone(),
                source,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
            .map_err(|source| ServerError::ServerLock {
                path: lock_path.clone(),
                source,
            })?;
        match fs4::FileExt::try_lock(&file) {
            Ok(()) => Ok(Self { _file: file }),
            Err(fs4::TryLockError::WouldBlock) => {
                Err(ServerError::ServerAlreadyRunning { path: lock_path })
            }
            Err(fs4::TryLockError::Error(source)) => Err(ServerError::ServerLock {
                path: lock_path,
                source,
            }),
        }
    }
}

fn lock_path(metadata_path: &Path) -> PathBuf {
    let mut path = metadata_path.as_os_str().to_os_string();
    path.push(".lock");
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::ServerProcessLock;

    #[test]
    fn lifecycle_lock_rejects_a_second_process_guard() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let metadata = directory.path().join("metadata.sqlite3");
        let first = ServerProcessLock::acquire(&metadata).expect("first lock should succeed");
        let error = ServerProcessLock::acquire(&metadata)
            .expect_err("second lock should report the running server");

        assert!(error.to_string().contains("already running"));
        drop(first);
        ServerProcessLock::acquire(&metadata).expect("released lock should be reusable");
    }

    #[test]
    fn lifecycle_lock_is_scoped_to_the_exact_metadata_database() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let primary = directory.path().join("primary.sqlite3");
        let secondary = directory.path().join("secondary.sqlite3");

        let _primary = ServerProcessLock::acquire(&primary).expect("primary lock should succeed");
        ServerProcessLock::acquire(&secondary)
            .expect("a distinct metadata database should have an independent lock");
    }
}
