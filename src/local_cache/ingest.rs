//! Ingest from repository-local Git LFS storage into the shared cache.

use super::*;

impl LocalCacheLayout {
    /// Ingests an object from an existing repository `.git/lfs/objects` cache.
    ///
    /// The source object is verified before publication, then copied into the
    /// shared cache using a temporary file in the target shard directory. If a
    /// valid shared-cache object is already present, it is reused without
    /// requiring the repository-local source file to exist.
    pub fn ingest_git_lfs_object(
        &self,
        git_lfs_objects_dir: impl AsRef<Path>,
        object: &LfsObject,
    ) -> LocalCacheResult<LocalCacheIngest> {
        let _operation_lock = self.lock_cache_operation_shared()?;
        let git_lfs_objects_dir = git_lfs_objects_dir.as_ref();
        let source_path = git_lfs_object_path(git_lfs_objects_dir, &object.oid);
        let cache_path = self.object_path(object);

        if cache_object_path_exists(&cache_path)? {
            self.verify_object(object)?;

            return Ok(LocalCacheIngest {
                object: object.clone(),
                source_path,
                cache_path,
                status: LocalCacheIngestStatus::AlreadyCached,
            });
        }

        ensure_source_object_file(&source_path, object)?;
        let status = copy_verified_object_to_cache(&source_path, &cache_path, object)?;

        Ok(LocalCacheIngest {
            object: object.clone(),
            source_path,
            cache_path,
            status,
        })
    }
}

#[cfg(test)]
mod ingest_tests {
    use super::*;
    use crate::local_cache::test_support::*;
    #[test]
    fn ingest_copies_valid_git_lfs_object_into_shared_cache() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let bytes = b"object already downloaded by git lfs";
        let object = object_for_bytes(bytes);
        let source_path = git_lfs_object_path(&git_lfs_objects_dir, &object.oid);
        write_file(&source_path, bytes);

        let ingest = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect("valid Git LFS object should ingest");

        assert_eq!(ingest.status, LocalCacheIngestStatus::Copied);
        assert_eq!(ingest.source_path, source_path);
        assert_eq!(ingest.cache_path, layout.object_path(&object));
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should exist"),
            bytes
        );
    }

    #[test]
    fn ingest_verifies_existing_cache_before_using_source() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let bytes = b"already present in shared cache";
        let object = object_for_bytes(bytes);
        write_file(&layout.object_path(&object), bytes);

        let ingest = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect("verified cache object should be reused without source");

        assert_eq!(ingest.status, LocalCacheIngestStatus::AlreadyCached);
    }

    #[test]
    fn ingest_rejects_corrupt_existing_cache_without_using_source() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let bytes = b"already present but corrupt in shared cache";
        let object = object_for_bytes(bytes);
        write_file(&layout.object_path(&object), b"corrupt cache bytes");
        write_file(
            &git_lfs_object_path(&git_lfs_objects_dir, &object.oid),
            bytes,
        );

        let error = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect_err("corrupt existing cache object should fail ingest");

        assert!(matches!(
            error,
            LocalCacheError::IntegrityMismatch { path, .. } if path == layout.object_path(&object)
        ));
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should remain inspectable"),
            b"corrupt cache bytes"
        );
    }

    #[test]
    fn ingest_rejects_missing_git_lfs_source_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let object = object_for_bytes(b"missing from source cache");

        let error = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect_err("missing source object should fail ingest");

        assert!(matches!(
            error,
            LocalCacheError::MissingSourceObject { oid, size, .. }
                if oid == object.oid && size == object.size
        ));
    }

    #[test]
    fn ingest_rejects_corrupt_git_lfs_source_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let git_lfs_objects_dir = temp.path().join(".git/lfs/objects");
        let object = object_for_bytes(b"expected contents");
        write_file(
            &git_lfs_object_path(&git_lfs_objects_dir, &object.oid),
            b"corrupt contents",
        );

        let error = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &object)
            .expect_err("corrupt source object should fail ingest");

        assert!(matches!(error, LocalCacheError::IntegrityMismatch { .. }));
        assert!(!layout.object_path(&object).exists());
    }
}
