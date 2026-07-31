//! Cross-process cache, worktree, and registry locking.

use super::*;

impl LocalCacheLayout {
    pub(super) fn worktree_registry_lock_path(&self) -> PathBuf {
        self.root.join(WORKTREE_REGISTRY_LOCK_FILE)
    }

    pub(super) fn cache_operation_lock_path(&self) -> PathBuf {
        self.root.join(CACHE_OPERATION_LOCK_FILE)
    }

    pub(super) fn worktree_path_lock_path(&self, worktree_path: &Path) -> PathBuf {
        let normalized = normalized_path_key(worktree_path);
        let digest = format!(
            "{:x}",
            Sha256::digest(normalized.as_os_str().to_string_lossy().as_bytes())
        );

        // Fixed stripes bound persistent coordination state. A collision only
        // serializes unrelated paths; every operation for one path still uses
        // the same cross-process lock.
        self.root
            .join(WORKTREE_PATH_LOCKS_DIR)
            .join(format!("{}.lock", &digest[..2]))
    }

    pub(super) fn lock_worktree_path(&self, worktree_path: &Path) -> LocalCacheResult<File> {
        let path = self.worktree_path_lock_path(worktree_path);
        let parent = path
            .parent()
            .expect("worktree lock path should have a parent");
        fs::create_dir_all(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache worktree lock directory",
            path: parent.to_path_buf(),
            source,
        })?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to open local cache worktree path lock",
                path: path.clone(),
                source,
            })?;
        FileExt::lock(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache worktree path",
            path,
            source,
        })?;

        Ok(lock)
    }

    pub(super) fn open_cache_operation_lock(&self) -> LocalCacheResult<(File, PathBuf)> {
        fs::create_dir_all(&self.root).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache root",
            path: self.root.clone(),
            source,
        })?;

        let path = self.cache_operation_lock_path();
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to open local cache operation lock",
                path: path.clone(),
                source,
            })?;

        Ok((lock, path))
    }

    pub(super) fn lock_cache_operation_shared(&self) -> LocalCacheResult<File> {
        let (lock, path) = self.open_cache_operation_lock()?;
        FileExt::lock_shared(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache operation for shared access",
            path,
            source,
        })?;

        Ok(lock)
    }

    pub(super) fn lock_cache_operation_exclusive(&self) -> LocalCacheResult<File> {
        let (lock, path) = self.open_cache_operation_lock()?;
        FileExt::lock(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache operation for exclusive access",
            path,
            source,
        })?;

        Ok(lock)
    }

    pub(super) fn lock_worktree_registry(&self) -> LocalCacheResult<File> {
        fs::create_dir_all(&self.root).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache root",
            path: self.root.clone(),
            source,
        })?;

        let path = self.worktree_registry_lock_path();
        let lock = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|source| LocalCacheError::Io {
                context: "failed to open local cache worktree registry lock",
                path: path.clone(),
                source,
            })?;

        // This blocking lock is deliberately non-reentrant. Registry callers
        // must not acquire it again while holding the returned file handle.
        FileExt::lock(&lock).map_err(|source| LocalCacheError::Io {
            context: "failed to lock local cache worktree registry",
            path,
            source,
        })?;

        Ok(lock)
    }

    pub(super) fn save_worktree_registry(
        &self,
        registry: &LocalCacheWorktreeRegistry,
    ) -> LocalCacheResult<()> {
        let path = self.worktree_registry_path();
        let parent = path.parent().ok_or_else(|| LocalCacheError::Io {
            context: "failed to resolve local cache worktree registry parent",
            path: path.clone(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "worktree registry path has no parent directory",
            ),
        })?;

        fs::create_dir_all(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create local cache worktree registry directory",
            path: parent.to_path_buf(),
            source,
        })?;

        let mut temp =
            tempfile::NamedTempFile::new_in(parent).map_err(|source| LocalCacheError::Io {
                context: "failed to create temporary local cache worktree registry",
                path: parent.to_path_buf(),
                source,
            })?;

        serde_json::to_writer_pretty(&mut temp, registry).map_err(|source| {
            LocalCacheError::WorktreeRegistryJson {
                context: "failed to encode local cache worktree registry",
                path: path.clone(),
                source,
            }
        })?;
        temp.write_all(b"\n")
            .map_err(|source| LocalCacheError::Io {
                context: "failed to write local cache worktree registry",
                path: path.clone(),
                source,
            })?;
        temp.flush().map_err(|source| LocalCacheError::Io {
            context: "failed to flush local cache worktree registry",
            path: path.clone(),
            source,
        })?;
        temp.as_file_mut()
            .sync_all()
            .map_err(|source| LocalCacheError::Io {
                context: "failed to sync local cache worktree registry",
                path: path.clone(),
                source,
            })?;

        temp.persist(&path).map_err(|error| LocalCacheError::Io {
            context: "failed to publish local cache worktree registry",
            path: path.clone(),
            source: error.error,
        })?;
        sync_directory(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to sync local cache worktree registry directory",
            path: parent.to_path_buf(),
            source,
        })?;

        Ok(())
    }
}
