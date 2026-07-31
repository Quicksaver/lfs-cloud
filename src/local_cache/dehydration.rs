//! Worktree dehydration operations.

use super::*;

impl LocalCacheLayout {
    /// Replaces a clean hydrated worktree file with a Git LFS pointer.
    ///
    /// The worktree file must contain the exact expected object bytes, unless
    /// it is already a matching pointer. If the shared cache does not already
    /// contain a verified copy of those bytes, the bytes are copied into cache
    /// before the worktree path is replaced. Dirty or unrelated worktree
    /// content is rejected and left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the worktree path is missing, the
    /// hydrated bytes do not match `object`, the cache object is corrupt, or
    /// the pointer cannot be written and verified.
    pub fn dehydrate_file(
        &self,
        object: &LfsObject,
        worktree_path: impl AsRef<Path>,
    ) -> LocalCacheResult<LocalCacheDehydration> {
        self.dehydrate_file_with_before_pointer_publish(object, worktree_path, || {})
    }

    fn dehydrate_file_with_before_pointer_publish<F>(
        &self,
        object: &LfsObject,
        worktree_path: impl AsRef<Path>,
        before_pointer_publish: F,
    ) -> LocalCacheResult<LocalCacheDehydration>
    where
        F: FnOnce(),
    {
        self.dehydrate_file_with_read_observer(object, worktree_path, before_pointer_publish, || {})
    }

    fn dehydrate_file_with_read_observer<F, R>(
        &self,
        object: &LfsObject,
        worktree_path: impl AsRef<Path>,
        before_pointer_publish: F,
        mut before_full_worktree_read: R,
    ) -> LocalCacheResult<LocalCacheDehydration>
    where
        F: FnOnce(),
        R: FnMut(),
    {
        // Hold a shared operation lock through pointer publication. GC takes
        // the exclusive side, so it can never observe the cache object after
        // publication but before the worktree reference becomes visible.
        let _operation_lock = self.lock_cache_operation_shared()?;
        let worktree_path = worktree_path.as_ref();
        let _path_lock = self.lock_worktree_path(worktree_path)?;
        let cache_path = self.object_path(object);

        let existing_pointer = read_existing_lfs_pointer_file(worktree_path)?;
        if let Some(pointer) = existing_pointer.as_ref()
            && pointer.object == *object
        {
            return Ok(LocalCacheDehydration {
                object: object.clone(),
                cache_path,
                pointer_path: worktree_path.to_path_buf(),
                status: LocalCacheDehydrationStatus::AlreadyDehydrated,
            });
        }
        if let Some(pointer) = existing_pointer {
            // A valid pointer can also be the literal contents of another LFS
            // object. This bounded read distinguishes that case while keeping
            // ordinary large hydrated files on the staged-copy fast path.
            before_full_worktree_read();
            if verify_worktree_file_object(worktree_path, object).is_err() {
                return Err(LocalCacheError::PointerObjectMismatch {
                    path: worktree_path.to_path_buf(),
                    expected_oid: object.oid.clone(),
                    expected_size: object.size,
                    actual_oid: pointer.object.oid,
                    actual_size: pointer.object.size,
                });
            }
        }
        let status = if cache_object_path_exists(&cache_path)? {
            self.verify_object(object)?;
            sync_verified_cache_object(&cache_path)?;
            LocalCacheDehydrationStatus::ReplacedWithPointer
        } else {
            // Hash the source while staging its cache copy. Atomic exchange
            // later retains and verifies the exact displaced bytes, so a
            // separate pre-copy and pre-exchange hash would add I/O without
            // strengthening the concurrent-edit guarantee.
            before_full_worktree_read();
            copy_verified_worktree_object_to_cache(worktree_path, &cache_path, object)?;
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        };

        publish_pointer_file(
            worktree_path,
            object,
            before_pointer_publish,
            &mut before_full_worktree_read,
        )?;

        Ok(LocalCacheDehydration {
            object: object.clone(),
            cache_path,
            pointer_path: worktree_path.to_path_buf(),
            status,
        })
    }
}

#[cfg(test)]
mod dehydration_tests {
    use super::*;
    use crate::local_cache::test_support::*;
    #[test]
    fn dehydrate_file_replaces_clean_hydrated_bytes_with_pointer_and_caches_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"worktree bytes to dehydrate";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, bytes);

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("clean worktree object should dehydrate");

        assert_eq!(dehydration.object, object);
        assert_eq!(dehydration.cache_path, layout.object_path(&object));
        assert_eq!(dehydration.pointer_path, worktree_path);
        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        );
        assert_eq!(
            fs::read_to_string(&dehydration.pointer_path).expect("pointer file should be readable"),
            LfsPointer::new(object.clone()).to_pointer_file()
        );
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
    }

    #[test]
    fn dehydrate_empty_file_preserves_the_canonical_empty_pointer() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let object = object_for_bytes(b"");
        let worktree_path = temp.path().join("repo/assets/empty.bin");
        write_file(&worktree_path, b"");

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("an empty file should already be its canonical pointer");

        assert_eq!(dehydration.object, object);
        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::AlreadyDehydrated
        );
        assert_eq!(
            fs::read(&dehydration.pointer_path).expect("empty pointer should remain readable"),
            b""
        );
        assert!(!dehydration.cache_path.exists());
    }

    #[test]
    fn dehydrate_file_reads_uncached_large_worktree_bytes_only_twice() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = vec![b'x'; (LFS_POINTER_SIZE_CUTOFF + 1) as usize];
        let object = object_for_bytes(&bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, &bytes);
        let mut full_read_count = 0;

        layout
            .dehydrate_file_with_read_observer(
                &object,
                &worktree_path,
                || {},
                || full_read_count += 1,
            )
            .expect("clean worktree object should dehydrate");

        assert_eq!(full_read_count, 2);
    }

    #[test]
    fn dehydrate_file_reads_cached_large_worktree_bytes_only_once() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = vec![b'x'; (LFS_POINTER_SIZE_CUTOFF + 1) as usize];
        let object = object_for_bytes(&bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), &bytes);
        write_file(&worktree_path, &bytes);
        let mut full_read_count = 0;

        layout
            .dehydrate_file_with_read_observer(
                &object,
                &worktree_path,
                || {},
                || full_read_count += 1,
            )
            .expect("clean cached worktree object should dehydrate");

        assert_eq!(full_read_count, 1);
    }

    #[cfg(unix)]
    #[test]
    fn dehydrate_file_rejects_symlink_without_replacing_it() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"outside bytes must not be dehydrated";
        let object = object_for_bytes(bytes);
        let outside_file = temp.path().join("outside-object");
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&outside_file, bytes);
        fs::create_dir_all(
            worktree_path
                .parent()
                .expect("worktree path should have a parent"),
        )
        .expect("worktree parent should be created");
        std::os::unix::fs::symlink(&outside_file, &worktree_path)
            .expect("worktree symlink should be created");

        let error = layout
            .dehydrate_file(&object, &worktree_path)
            .expect_err("a symlink must not be dehydrated");

        assert!(matches!(
            error,
            LocalCacheError::WorktreePathSymlink { path } if path == worktree_path
        ));
        assert!(
            fs::symlink_metadata(&worktree_path)
                .expect("symlink metadata should remain readable")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read(&outside_file).expect("outside file should remain readable"),
            bytes
        );
        assert!(!layout.object_path(&object).exists());
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn dehydrate_file_preserves_edit_before_pointer_exchange() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"worktree bytes to dehydrate";
        let edited_bytes = b"concurrent worktree edit";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, bytes);

        let pointer_staged = Arc::new(Barrier::new(2));
        let allow_publish = Arc::new(Barrier::new(2));
        let dehydration_layout = layout.clone();
        let dehydration_object = object.clone();
        let dehydration_path = worktree_path.clone();
        let dehydration_pointer_staged = Arc::clone(&pointer_staged);
        let dehydration_allow_publish = Arc::clone(&allow_publish);
        let dehydration = thread::spawn(move || {
            dehydration_layout.dehydrate_file_with_before_pointer_publish(
                &dehydration_object,
                &dehydration_path,
                || {
                    dehydration_pointer_staged.wait();
                    dehydration_allow_publish.wait();
                },
            )
        });

        pointer_staged.wait();
        write_file(&worktree_path, edited_bytes);
        allow_publish.wait();

        let error = dehydration
            .join()
            .expect("dehydration thread should not panic")
            .expect_err("a concurrent edit should abort dehydration");
        assert!(matches!(error, LocalCacheError::IntegrityMismatch { .. }));
        assert_eq!(
            fs::read(&worktree_path).expect("concurrent edit should remain readable"),
            edited_bytes
        );
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("verified cache object should remain"),
            bytes
        );
    }

    #[test]
    fn garbage_collect_waits_until_dehydration_publishes_its_pointer() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let worktree_path = repo.join("assets/model.bin");
        let bytes = b"only copy must survive concurrent garbage collection";
        let object = object_for_bytes(bytes);
        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &worktree_path,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        git_add(
            &repo,
            &[Path::new(".gitattributes"), Path::new("assets/model.bin")],
        );
        write_file(&worktree_path, bytes);
        layout
            .register_worktree(
                LocalCacheWorktreeRegistration::new(
                    "github-main:owner/repo",
                    &repo,
                    repo.join(".git"),
                )
                .expect("registration should validate"),
            )
            .expect("worktree should register");

        let cache_published = Arc::new(Barrier::new(2));
        let allow_pointer_publish = Arc::new(Barrier::new(2));
        let dehydration_layout = layout.clone();
        let dehydration_object = object.clone();
        let dehydration_path = worktree_path.clone();
        let dehydration_cache_published = Arc::clone(&cache_published);
        let dehydration_allow_pointer_publish = Arc::clone(&allow_pointer_publish);
        let dehydration = thread::spawn(move || {
            dehydration_layout.dehydrate_file_with_before_pointer_publish(
                &dehydration_object,
                &dehydration_path,
                || {
                    dehydration_cache_published.wait();
                    dehydration_allow_pointer_publish.wait();
                },
            )
        });

        cache_published.wait();
        let cache_bytes_during_pause = fs::read(layout.object_path(&object));
        let worktree_bytes_during_pause = fs::read(&worktree_path);

        let collection_layout = layout.clone();
        let (collection_started_tx, collection_started_rx) = mpsc::channel();
        let (collection_done_tx, collection_done_rx) = mpsc::channel();
        let collection = thread::spawn(move || {
            collection_started_tx
                .send(())
                .expect("test should receive collector start");
            collection_done_tx
                .send(collection_layout.garbage_collect(false, false))
                .expect("test should receive collection result");
        });
        let collection_started = collection_started_rx.recv();
        let collection_during_pause = collection_done_rx.recv_timeout(Duration::from_millis(250));

        allow_pointer_publish.wait();
        let dehydration = dehydration
            .join()
            .expect("dehydration thread should not panic")
            .expect("dehydration should finish");
        let collection_report = collection_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("garbage collection should finish after dehydration")
            .expect("garbage collection should succeed");
        collection
            .join()
            .expect("collection thread should not panic");

        assert_eq!(
            cache_bytes_during_pause.expect("cache object should be published"),
            bytes
        );
        assert_eq!(
            worktree_bytes_during_pause
                .expect("worktree bytes should remain until pointer publish"),
            bytes
        );
        collection_started.expect("collector should report before locking");
        assert!(
            collection_during_pause.is_err(),
            "garbage collection should wait for pointer publication"
        );
        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        );
        assert_eq!(collection_report.retained_objects.len(), 1);
        assert!(collection_report.deleted_objects.is_empty());
        assert!(layout.object_path(&object).exists());
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("pointer should be readable"),
            LfsPointer::new(object).to_pointer_file()
        );
    }

    #[cfg(unix)]
    #[test]
    fn dehydrate_file_preserves_existing_worktree_file_mode_on_pointer() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"executable worktree bytes";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/tool.bin");
        write_file(&worktree_path, bytes);
        fs::set_permissions(&worktree_path, fs::Permissions::from_mode(0o755))
            .expect("test file mode should be set");

        layout
            .dehydrate_file(&object, &worktree_path)
            .expect("clean executable worktree object should dehydrate");

        let mode = fs::metadata(&worktree_path)
            .expect("pointer file metadata should be readable")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn dehydrate_file_reuses_existing_verified_cache_before_replacing() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"already cached worktree bytes";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&worktree_path, bytes);

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("clean cached worktree object should dehydrate");

        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::ReplacedWithPointer
        );
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("pointer file should be readable"),
            LfsPointer::new(object).to_pointer_file()
        );
    }

    #[test]
    fn dehydrate_file_accepts_matching_pointer_without_cache_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let object = object_for_bytes(b"already dehydrated bytes");
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(
            &worktree_path,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("matching pointer should already be dehydrated");

        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::AlreadyDehydrated
        );
        assert!(!layout.object_path(&object).exists());
    }

    #[test]
    fn dehydrate_file_accepts_pointer_shaped_object_contents() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let referenced = object_for_bytes(b"referenced object bytes");
        let pointer_shaped_bytes = LfsPointer::new(referenced).to_pointer_file();
        let object = object_for_bytes(pointer_shaped_bytes.as_bytes());
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&worktree_path, pointer_shaped_bytes.as_bytes());

        let dehydration = layout
            .dehydrate_file(&object, &worktree_path)
            .expect("pointer-shaped object bytes should dehydrate");

        assert_eq!(
            dehydration.status,
            LocalCacheDehydrationStatus::CachedAndReplacedWithPointer
        );
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            pointer_shaped_bytes.as_bytes()
        );
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("pointer file should be readable"),
            LfsPointer::new(object).to_pointer_file()
        );
    }

    #[test]
    fn dehydrate_file_reports_existing_pointer_for_different_object() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let expected = object_for_bytes(b"expected object bytes");
        let actual = object_for_bytes(b"other object bytes");
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(
            &worktree_path,
            LfsPointer::new(actual.clone()).to_pointer_file().as_bytes(),
        );

        let error = layout
            .dehydrate_file(&expected, &worktree_path)
            .expect_err("different pointer should not dehydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerObjectMismatch {
                path,
                expected_oid,
                expected_size,
                actual_oid,
                actual_size,
            } if path == worktree_path
                && expected_oid == expected.oid
                && expected_size == expected.size
                && actual_oid == actual.oid
                && actual_size == actual.size
        ));
        assert_eq!(
            fs::read_to_string(&worktree_path).expect("existing pointer should remain"),
            LfsPointer::new(actual).to_pointer_file()
        );
    }

    #[test]
    fn dehydrate_file_refuses_to_replace_dirty_worktree_content() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"original hydrated bytes";
        let object = object_for_bytes(bytes);
        let worktree_path = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&worktree_path, b"dirty worktree bytes");

        let error = layout
            .dehydrate_file(&object, &worktree_path)
            .expect_err("dirty worktree content should not dehydrate");

        assert!(matches!(
            error,
            LocalCacheError::IntegrityMismatch { path, .. } if path == worktree_path
        ));
        assert_eq!(
            fs::read(&worktree_path).expect("dirty worktree content should remain"),
            b"dirty worktree bytes"
        );
    }
}
