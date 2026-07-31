//! Verified cache-object materialization and pointer publication.

use super::*;

impl LocalCacheLayout {
    /// Materializes a verified cache object at a worktree destination.
    ///
    /// Existing destination files are accepted only when they already contain
    /// the exact requested object bytes. This keeps the lower-level helper from
    /// overwriting dirty worktree contents; pointer-file replacement is handled
    /// by [`Self::hydrate_pointer_file`], which proves the pointer identity
    /// before replacing it. On Unix, a destination created from scratch starts
    /// owner-readable and owner-writable only. Hydrating an existing pointer
    /// instead preserves that worktree file's mode, including its executable
    /// bit.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the cache object is missing or corrupt,
    /// the destination contains different bytes, or the destination cannot be
    /// written and verified.
    pub fn materialize_object(
        &self,
        object: &LfsObject,
        destination_path: impl AsRef<Path>,
    ) -> LocalCacheResult<LocalCacheMaterialization> {
        let _operation_lock = self.lock_cache_operation_shared()?;
        let destination_path = destination_path.as_ref();
        let _path_lock = self.lock_worktree_path(destination_path)?;
        let verified = self.verify_object(object)?;

        materialize_verified_object(
            &verified,
            destination_path,
            MaterializationMode::NoReplace,
            || {},
        )
    }

    /// Replaces a Git LFS pointer file with verified cache object bytes.
    ///
    /// The existing worktree file must parse as a Git LFS pointer, and the
    /// pointed object must exist in the shared cache with matching SHA-256 and
    /// byte size. Non-pointer content is treated as a dirty or unsupported
    /// worktree state and is left untouched.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the pointer cannot be parsed, the cache
    /// object is missing or corrupt, or materialization cannot be completed and
    /// verified.
    pub fn hydrate_pointer_file(
        &self,
        pointer_path: impl AsRef<Path>,
    ) -> LocalCacheResult<LocalCacheMaterialization> {
        self.hydrate_pointer_file_with_before_publish(pointer_path, || {})
    }

    fn hydrate_pointer_file_with_before_publish<F>(
        &self,
        pointer_path: impl AsRef<Path>,
        before_publish: F,
    ) -> LocalCacheResult<LocalCacheMaterialization>
    where
        F: FnOnce(),
    {
        let _operation_lock = self.lock_cache_operation_shared()?;
        let pointer_path = pointer_path.as_ref();
        let _path_lock = self.lock_worktree_path(pointer_path)?;
        let pointer = read_lfs_pointer_file(pointer_path)?;
        if pointer.is_empty() {
            return Ok(LocalCacheMaterialization {
                cache_path: self.object_path(&pointer.object),
                destination_path: pointer_path.to_path_buf(),
                object: pointer.object,
                status: LocalCacheMaterializationStatus::AlreadyMaterialized,
            });
        }
        let verified = self.verify_object(&pointer.object)?;

        materialize_verified_object(
            &verified,
            pointer_path,
            MaterializationMode::ReplaceMatchingPointer,
            before_publish,
        )
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MaterializationMode {
    NoReplace,
    ReplaceMatchingPointer,
}

pub(super) fn read_lfs_pointer_file(path: &Path) -> LocalCacheResult<LfsPointer> {
    let file =
        open_worktree_file_without_following_symlinks(path, "failed to open Git LFS pointer file")?;
    let contents = match read_bounded_pointer_bytes(
        file,
        path,
        "failed to inspect Git LFS pointer file",
        "failed to read Git LFS pointer file",
    )? {
        BoundedPointerBytes::Contents(contents) => contents,
        BoundedPointerBytes::TooLarge { size } => {
            return Err(LocalCacheError::PointerFileTooLarge {
                path: path.to_path_buf(),
                size,
                size_cutoff: LFS_POINTER_SIZE_CUTOFF,
            });
        }
        BoundedPointerBytes::NotRegularFile => {
            return Err(LocalCacheError::PointerPathNotRegularFile {
                path: path.to_path_buf(),
            });
        }
    };

    let contents = std::str::from_utf8(&contents).map_err(|source| {
        LocalCacheError::PointerFileInvalidUtf8 {
            path: path.to_path_buf(),
            source,
        }
    })?;

    LfsPointer::parse(contents).map_err(|source| LocalCacheError::PointerParse {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn read_existing_lfs_pointer_file(path: &Path) -> LocalCacheResult<Option<LfsPointer>> {
    let file =
        open_worktree_file_without_following_symlinks(path, "failed to open dehydration target")?;
    let BoundedPointerBytes::Contents(contents) = read_bounded_pointer_bytes(
        file,
        path,
        "failed to inspect dehydration target",
        "failed to read dehydration target",
    )?
    else {
        return Ok(None);
    };

    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents).ok())
}

pub(super) fn publish_pointer_file<F, R>(
    path: &Path,
    object: &LfsObject,
    before_publish: F,
    before_displaced_verification: R,
) -> LocalCacheResult<()>
where
    F: FnOnce(),
    R: FnOnce(),
{
    let parent = path.parent().ok_or_else(|| LocalCacheError::Io {
        context: "failed to resolve dehydration target parent",
        path: path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "dehydration target path has no parent directory",
        ),
    })?;
    let mode = existing_file_mode(path)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create temporary Git LFS pointer",
            path: parent.to_path_buf(),
            source,
        })?;
    let pointer = LfsPointer::new(object.clone()).to_pointer_file();

    temp.write_all(pointer.as_bytes())
        .map_err(|source| LocalCacheError::Io {
            context: "failed to write temporary Git LFS pointer",
            path: path.to_path_buf(),
            source,
        })?;
    temp.flush().map_err(|source| LocalCacheError::Io {
        context: "failed to flush temporary Git LFS pointer",
        path: path.to_path_buf(),
        source,
    })?;
    set_temporary_file_mode(temp.path(), path, mode)?;

    before_publish();
    replace_retaining_displaced(temp, path, |displaced_path| {
        before_displaced_verification();
        remap_integrity_path(verify_worktree_file_object(displaced_path, object), path).map(|_| ())
    })?;

    let pointer = read_lfs_pointer_file(path)?;
    if pointer.object != *object {
        return Err(LocalCacheError::PointerParse {
            path: path.to_path_buf(),
            source: LfsObjectError::PointerUnexpectedLine {
                line: "dehydrated pointer did not round-trip to the expected object".to_owned(),
            },
        });
    }

    Ok(())
}

pub(super) fn materialize_verified_object(
    verified: &VerifiedLocalCacheObject,
    destination_path: &Path,
    mode: MaterializationMode,
    before_publish: impl FnOnce(),
) -> LocalCacheResult<LocalCacheMaterialization> {
    match existing_destination_status(destination_path, &verified.object)? {
        ExistingDestinationStatus::Missing => {}
        ExistingDestinationStatus::AlreadyMaterialized => {
            return Ok(LocalCacheMaterialization {
                object: verified.object.clone(),
                cache_path: verified.path.clone(),
                destination_path: destination_path.to_path_buf(),
                status: LocalCacheMaterializationStatus::AlreadyMaterialized,
            });
        }
        ExistingDestinationStatus::Different => match mode {
            MaterializationMode::NoReplace => {
                return Err(LocalCacheError::MaterializationTargetExists {
                    oid: verified.object.oid.clone(),
                    size: verified.object.size,
                    path: destination_path.to_path_buf(),
                });
            }
            MaterializationMode::ReplaceMatchingPointer => {
                let pointer = read_lfs_pointer_file(destination_path)?;
                if pointer.object != verified.object {
                    return Err(LocalCacheError::MaterializationTargetExists {
                        oid: verified.object.oid.clone(),
                        size: verified.object.size,
                        path: destination_path.to_path_buf(),
                    });
                }
            }
        },
    }

    let destination_parent = destination_path
        .parent()
        .ok_or_else(|| LocalCacheError::Io {
            context: "failed to resolve materialization destination parent",
            path: destination_path.to_path_buf(),
            source: io::Error::new(
                io::ErrorKind::InvalidInput,
                "destination path has no parent directory",
            ),
        })?;
    fs::create_dir_all(destination_parent).map_err(|source| LocalCacheError::Io {
        context: "failed to create materialization destination directory",
        path: destination_parent.to_path_buf(),
        source,
    })?;

    let (temp, status) =
        materialize_to_temporary_file(&verified.path, destination_parent, &verified.object)?;

    publish_materialized_file(
        temp,
        destination_path,
        mode,
        &verified.object,
        before_publish,
    )?;
    // The final verification proves the path currently contains the expected
    // object. If an uncoordinated writer races this local worktree path, the
    // caller may still receive an integrity error after publication.
    let materialized = verify_worktree_file_object(destination_path, &verified.object)?;

    Ok(LocalCacheMaterialization {
        object: materialized.object,
        cache_path: verified.path.clone(),
        destination_path: materialized.path,
        status,
    })
}

pub(super) enum ExistingDestinationStatus {
    Missing,
    AlreadyMaterialized,
    Different,
}

pub(super) fn existing_destination_status(
    path: &Path,
    object: &LfsObject,
) -> LocalCacheResult<ExistingDestinationStatus> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(LocalCacheError::WorktreePathSymlink {
                path: path.to_path_buf(),
            })
        }
        Ok(metadata) if metadata.is_file() => {
            if metadata.len() != object.size.bytes() {
                return Ok(ExistingDestinationStatus::Different);
            }

            match verify_worktree_file_object(path, object) {
                Ok(_) => Ok(ExistingDestinationStatus::AlreadyMaterialized),
                Err(LocalCacheError::IntegrityMismatch { .. }) => {
                    Ok(ExistingDestinationStatus::Different)
                }
                Err(error) => Err(error),
            }
        }
        Ok(_) => Err(LocalCacheError::Io {
            context: "materialization destination is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            Ok(ExistingDestinationStatus::Missing)
        }
        Err(source) => Err(LocalCacheError::Io {
            context: "failed to inspect materialization destination",
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn materialize_to_temporary_file(
    cache_path: &Path,
    destination_parent: &Path,
    object: &LfsObject,
) -> LocalCacheResult<(tempfile::NamedTempFile, LocalCacheMaterializationStatus)> {
    materialize_to_temporary_file_with_clone(
        cache_path,
        destination_parent,
        object,
        copy_on_write_clone,
    )
}

pub(super) fn materialize_to_temporary_file_with_clone(
    cache_path: &Path,
    destination_parent: &Path,
    object: &LfsObject,
    try_clone: impl FnOnce(&Path, &Path) -> LocalCacheResult<Option<tempfile::NamedTempFile>>,
) -> LocalCacheResult<(tempfile::NamedTempFile, LocalCacheMaterializationStatus)> {
    if let Some(temp) = try_clone(cache_path, destination_parent)? {
        verify_file_object(temp.path(), object)?;
        return Ok((temp, LocalCacheMaterializationStatus::CopyOnWriteCloned));
    }

    let mut temp = tempfile::NamedTempFile::new_in(destination_parent).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to create temporary materialized object",
            path: destination_parent.to_path_buf(),
            source,
        }
    })?;
    copy_cache_object_to_temporary_file(cache_path, &mut temp, object)?;

    Ok((temp, LocalCacheMaterializationStatus::Copied))
}

pub(super) fn copy_cache_object_to_temporary_file(
    cache_path: &Path,
    temp: &mut tempfile::NamedTempFile,
    expected: &LfsObject,
) -> LocalCacheResult<()> {
    let mut source = File::open(cache_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open verified cache object",
        path: cache_path.to_path_buf(),
        source,
    })?;
    let temp_path = temp.path().to_path_buf();
    let destination = temp.as_file_mut();
    destination
        .set_len(0)
        .and_then(|()| destination.seek(SeekFrom::Start(0)).map(|_| ()))
        .map_err(|source| LocalCacheError::Io {
            context: "failed to open temporary materialized object",
            path: temp_path.clone(),
            source,
        })?;
    let actual = hash_and_optionally_copy(
        &mut source,
        cache_path,
        "failed to read verified cache object",
        Some(ObjectCopyDestination {
            writer: &mut *destination,
            path: &temp_path,
            context: "failed to write temporary materialized object",
        }),
    )?;
    destination.flush().map_err(|source| LocalCacheError::Io {
        context: "failed to flush temporary materialized object",
        path: temp_path,
        source,
    })?;
    ensure_object_identity(cache_path, expected, actual)
}

pub(super) fn publish_materialized_file(
    temp: tempfile::NamedTempFile,
    destination_path: &Path,
    mode: MaterializationMode,
    object: &LfsObject,
    before_publish: impl FnOnce(),
) -> LocalCacheResult<()> {
    match mode {
        MaterializationMode::NoReplace => {
            before_publish();
            set_temporary_file_mode(
                temp.path(),
                destination_path,
                DEFAULT_MATERIALIZED_FILE_MODE,
            )?;
            match temp.persist_noclobber(destination_path) {
                Ok(_) => Ok(()),
                Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                    match verify_worktree_file_object(destination_path, object) {
                        Ok(_) => Ok(()),
                        Err(LocalCacheError::IntegrityMismatch { .. }) => {
                            Err(LocalCacheError::MaterializationTargetExists {
                                oid: object.oid.clone(),
                                size: object.size,
                                path: destination_path.to_path_buf(),
                            })
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => Err(LocalCacheError::Io {
                    context: "failed to publish materialized object",
                    path: destination_path.to_path_buf(),
                    source: error.error,
                }),
            }
        }
        MaterializationMode::ReplaceMatchingPointer => {
            let pointer = read_lfs_pointer_file(destination_path)?;
            if pointer.object != *object {
                return Err(LocalCacheError::MaterializationTargetExists {
                    oid: object.oid.clone(),
                    size: object.size,
                    path: destination_path.to_path_buf(),
                });
            }
            let replacement_mode = existing_file_mode(destination_path)?;
            set_temporary_file_mode(temp.path(), destination_path, replacement_mode)?;
            before_publish();
            replace_retaining_displaced(temp, destination_path, |displaced_path| {
                let pointer = read_lfs_pointer_file(displaced_path).map_err(|_| {
                    LocalCacheError::MaterializationTargetExists {
                        oid: object.oid.clone(),
                        size: object.size,
                        path: destination_path.to_path_buf(),
                    }
                })?;
                if pointer.object != *object {
                    return Err(LocalCacheError::MaterializationTargetExists {
                        oid: object.oid.clone(),
                        size: object.size,
                        path: destination_path.to_path_buf(),
                    });
                }

                Ok(())
            })
        }
    }
}

pub(super) fn remap_integrity_path<T>(
    result: LocalCacheResult<T>,
    public_path: &Path,
) -> LocalCacheResult<T> {
    result.map_err(|error| match error {
        LocalCacheError::IntegrityMismatch {
            expected_oid,
            expected_size,
            actual_oid,
            actual_size,
            ..
        } => LocalCacheError::IntegrityMismatch {
            path: public_path.to_path_buf(),
            expected_oid,
            expected_size,
            actual_oid,
            actual_size,
        },
        error => error,
    })
}

pub(super) fn replace_retaining_displaced<F>(
    temp: tempfile::NamedTempFile,
    destination_path: &Path,
    verify_displaced: F,
) -> LocalCacheResult<()>
where
    F: FnOnce(&Path) -> LocalCacheResult<()>,
{
    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    {
        let mut temp = temp;
        exchange_paths(temp.path(), destination_path).map_err(|source| LocalCacheError::Io {
            context: "failed to atomically exchange worktree content",
            path: destination_path.to_path_buf(),
            source,
        })?;

        match verify_displaced(temp.path()) {
            Ok(()) => Ok(()),
            Err(error) => {
                if let Err(source) = exchange_paths(temp.path(), destination_path) {
                    let recovery_path = temp.path().to_path_buf();
                    temp.disable_cleanup(true);
                    return Err(LocalCacheError::WorktreeReplacementRollback {
                        path: destination_path.to_path_buf(),
                        recovery_path,
                        source,
                    });
                }

                Err(error)
            }
        }
    }

    #[cfg(not(any(target_os = "android", target_os = "linux", target_vendor = "apple")))]
    {
        // Platforms without exchange rename retain the final identity check and
        // path lock. The replacement remains atomic, but cannot preserve a
        // displaced file for post-rename verification.
        verify_displaced(destination_path)?;
        temp.persist(destination_path)
            .map(|_| ())
            .map_err(|error| LocalCacheError::Io {
                context: "failed to publish worktree content",
                path: destination_path.to_path_buf(),
                source: error.error,
            })
    }
}

#[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
pub(super) fn exchange_paths(left: &Path, right: &Path) -> io::Result<()> {
    rustix::fs::renameat_with(
        rustix::fs::CWD,
        left,
        rustix::fs::CWD,
        right,
        rustix::fs::RenameFlags::EXCHANGE,
    )
    .map_err(io::Error::from)
}

#[cfg(unix)]
pub(super) fn existing_file_mode(path: &Path) -> LocalCacheResult<u32> {
    use std::os::unix::fs::PermissionsExt;

    worktree_file_metadata_without_following_symlinks(path)
        .map(|metadata| metadata.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
pub(super) fn existing_file_mode(_path: &Path) -> LocalCacheResult<u32> {
    Ok(0)
}

#[cfg(unix)]
pub(super) fn set_temporary_file_mode(
    temp_path: &Path,
    destination_path: &Path,
    mode: u32,
) -> LocalCacheResult<()> {
    use std::os::unix::fs::PermissionsExt;

    fs::set_permissions(temp_path, fs::Permissions::from_mode(mode)).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to set temporary file permissions",
            path: destination_path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
pub(super) fn set_temporary_file_mode(
    _temp_path: &Path,
    _destination_path: &Path,
    _mode: u32,
) -> LocalCacheResult<()> {
    Ok(())
}

#[cfg(target_os = "macos")]
pub(super) fn copy_on_write_clone(
    source_path: &Path,
    destination_parent: &Path,
) -> LocalCacheResult<Option<tempfile::NamedTempFile>> {
    let source = File::open(source_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open cache object for copy-on-write cloning",
        path: source_path.to_path_buf(),
        source,
    })?;
    let destination_directory =
        tempfile::tempdir_in(destination_parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create temporary clone directory",
            path: destination_parent.to_path_buf(),
            source,
        })?;
    let candidate = tempfile::NamedTempFile::new_in(destination_parent).map_err(|source| {
        LocalCacheError::Io {
            context: "failed to reserve temporary clone path",
            path: destination_parent.to_path_buf(),
            source,
        }
    })?;
    let destination_path = candidate.path().to_path_buf();
    let clone_name = "materialized-object";
    let clone_path = destination_directory.path().join(clone_name);
    let clone_directory =
        File::open(destination_directory.path()).map_err(|source| LocalCacheError::Io {
            context: "failed to open temporary clone directory",
            path: destination_directory.path().to_path_buf(),
            source,
        })?;

    if let Err(source) = rustix::fs::fclonefileat(
        &source,
        &clone_directory,
        clone_name,
        rustix::fs::CloneFlags::NOFOLLOW,
    ) {
        tracing::debug!(
            source = %source_path.display(),
            destination = %destination_path.display(),
            error = %source,
            "macOS copy-on-write clone failed; falling back to verified copy"
        );
        return Ok(None);
    }

    let (candidate_file, temporary_path) = candidate.into_parts();
    drop(candidate_file);
    fs::rename(&clone_path, &destination_path).map_err(|source| LocalCacheError::Io {
        context: "failed to move temporary cloned object into place",
        path: destination_path.clone(),
        source,
    })?;
    let cloned_file = rustix::fs::openat(
        rustix::fs::CWD,
        &destination_path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from)
    .map_err(|source| LocalCacheError::Io {
        context: "failed to open temporary cloned object",
        path: destination_path,
        source,
    })?;

    Ok(Some(tempfile::NamedTempFile::from_parts(
        cloned_file,
        temporary_path,
    )))
}

#[cfg(not(target_os = "macos"))]
pub(super) fn copy_on_write_clone(
    _source_path: &Path,
    _destination_parent: &Path,
) -> LocalCacheResult<Option<tempfile::NamedTempFile>> {
    Ok(None)
}

#[cfg(unix)]
pub(super) fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
pub(super) fn sync_directory(_path: &Path) -> io::Result<()> {
    Ok(())
}

pub(super) fn sync_verified_cache_object(cache_path: &Path) -> LocalCacheResult<()> {
    let cache_parent = cache_path.parent().ok_or_else(|| LocalCacheError::Io {
        context: "failed to resolve cache object parent",
        path: cache_path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache object path has no parent directory",
        ),
    })?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    options.write(true);
    let file = options
        .open(cache_path)
        .map_err(|source| LocalCacheError::Io {
            context: "failed to open verified cache object for sync",
            path: cache_path.to_path_buf(),
            source,
        })?;
    file.sync_all().map_err(|source| LocalCacheError::Io {
        context: "failed to sync verified cache object",
        path: cache_path.to_path_buf(),
        source,
    })?;
    sync_directory(cache_parent).map_err(|source| LocalCacheError::Io {
        context: "failed to sync cache object directory",
        path: cache_parent.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod materialization_tests {
    use super::*;
    use crate::local_cache::test_support::*;
    #[test]
    fn materialize_object_creates_verified_destination_from_cache() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cache bytes to materialize";
        let object = object_for_bytes(bytes);
        write_file(&layout.object_path(&object), bytes);
        let destination = temp.path().join("repo/assets/model.bin");

        let materialization = layout
            .materialize_object(&object, &destination)
            .expect("verified cache object should materialize");

        assert_eq!(materialization.object, object);
        assert_eq!(materialization.cache_path, layout.object_path(&object));
        assert_eq!(materialization.destination_path, destination);
        #[cfg(target_os = "macos")]
        let expected_status = if is_apfs(temp.path()) {
            LocalCacheMaterializationStatus::CopyOnWriteCloned
        } else {
            LocalCacheMaterializationStatus::Copied
        };
        #[cfg(not(target_os = "macos"))]
        let expected_status = LocalCacheMaterializationStatus::Copied;
        assert_eq!(materialization.status, expected_status);
        assert_eq!(
            fs::read(&materialization.destination_path)
                .expect("materialized destination should be readable"),
            bytes
        );
    }

    #[test]
    fn materialize_to_temporary_file_copies_when_clone_is_unavailable() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let bytes = b"cache bytes for fallback materialization";
        let object = object_for_bytes(bytes);
        let cache_path = temp.path().join("cache-object");
        write_file(&cache_path, bytes);

        let (materialized, status) = materialize_to_temporary_file_with_clone(
            &cache_path,
            temp.path(),
            &object,
            |_source_path, _destination_parent| Ok(None),
        )
        .expect("unavailable cloning should fall back to a verified copy");

        assert_eq!(status, LocalCacheMaterializationStatus::Copied);
        assert_eq!(
            fs::read(materialized.path()).expect("fallback copy should be readable"),
            bytes
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn materialize_object_uses_copy_on_write_on_apfs() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        assert!(is_apfs(temp.path()), "this test requires an APFS fixture");

        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cache bytes that must be cloned on APFS";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);

        let materialization = layout
            .materialize_object(&object, &destination)
            .expect("APFS materialization should succeed");

        assert_eq!(
            materialization.status,
            LocalCacheMaterializationStatus::CopyOnWriteCloned
        );
    }

    #[cfg(unix)]
    #[test]
    fn materialize_object_respects_restrictive_process_umask_for_new_files() {
        use std::os::unix::fs::PermissionsExt;

        const UMASK_CHILD_ENV: &str = "LFS_CLOUD_MATERIALIZATION_UMASK_CHILD";
        const TEST_NAME: &str = "local_cache::materialization::materialization_tests::materialize_object_respects_restrictive_process_umask_for_new_files";

        if std::env::var_os(UMASK_CHILD_ENV).is_none() {
            let current_exe = std::env::current_exe().expect("test executable should resolve");
            let status = Command::new("sh")
                .args([
                    "-c",
                    "umask 077; exec \"$1\" --exact \"$2\" --nocapture",
                    "sh",
                ])
                .arg(current_exe)
                .arg(TEST_NAME)
                .env(UMASK_CHILD_ENV, "1")
                .status()
                .expect("restrictive-umask test subprocess should start");
            assert!(status.success(), "restrictive-umask subprocess should pass");
            return;
        }

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cache bytes with private worktree permissions";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);

        layout
            .materialize_object(&object, &destination)
            .expect("verified cache object should materialize");

        let mode = fs::metadata(&destination)
            .expect("materialized destination should have metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn materialize_object_reuses_existing_verified_destination() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"already hydrated contents";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&destination, bytes);

        let materialization = layout
            .materialize_object(&object, &destination)
            .expect("existing verified destination should be reused");

        assert_eq!(
            materialization.status,
            LocalCacheMaterializationStatus::AlreadyMaterialized
        );
    }

    #[test]
    fn materialize_object_refuses_to_overwrite_different_destination() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"safe cache contents";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(&destination, b"dirty worktree contents");

        let error = layout
            .materialize_object(&object, &destination)
            .expect_err("different destination content should not be overwritten");

        assert!(matches!(
            error,
            LocalCacheError::MaterializationTargetExists { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("destination should remain untouched"),
            b"dirty worktree contents"
        );
    }

    #[test]
    fn hydrate_pointer_file_replaces_matching_pointer_with_cache_bytes() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated bytes from shared cache";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );

        let materialization = layout
            .hydrate_pointer_file(&destination)
            .expect("matching pointer should hydrate from cache");

        assert_eq!(materialization.object, object);
        #[cfg(target_os = "macos")]
        let expected_status = if is_apfs(temp.path()) {
            LocalCacheMaterializationStatus::CopyOnWriteCloned
        } else {
            LocalCacheMaterializationStatus::Copied
        };
        #[cfg(not(target_os = "macos"))]
        let expected_status = LocalCacheMaterializationStatus::Copied;
        assert_eq!(materialization.status, expected_status);
        assert_eq!(
            fs::read(&destination).expect("hydrated destination should be readable"),
            bytes
        );
    }

    #[test]
    fn hydrate_empty_pointer_is_already_materialized_without_cache_bytes() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let object = object_for_bytes(b"");
        let destination = temp.path().join("repo/assets/empty.bin");
        write_file(&destination, b"");

        let materialization = layout
            .hydrate_pointer_file(&destination)
            .expect("an empty pointer should already be materialized");

        assert_eq!(materialization.object, object);
        assert_eq!(materialization.destination_path, destination);
        assert_eq!(
            materialization.status,
            LocalCacheMaterializationStatus::AlreadyMaterialized
        );
        assert!(!materialization.cache_path.exists());
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn hydrate_pointer_file_preserves_edit_after_final_pointer_check() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated bytes from shared cache";
        let edited_bytes = b"concurrent worktree edit";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );

        let pointer_checked = Arc::new(Barrier::new(2));
        let allow_publish = Arc::new(Barrier::new(2));
        let hydration_layout = layout.clone();
        let hydration_destination = destination.clone();
        let hydration_pointer_checked = Arc::clone(&pointer_checked);
        let hydration_allow_publish = Arc::clone(&allow_publish);
        let hydration = thread::spawn(move || {
            hydration_layout.hydrate_pointer_file_with_before_publish(
                &hydration_destination,
                || {
                    hydration_pointer_checked.wait();
                    hydration_allow_publish.wait();
                },
            )
        });

        pointer_checked.wait();
        write_file(&destination, edited_bytes);
        allow_publish.wait();

        let error = hydration
            .join()
            .expect("hydration thread should not panic")
            .expect_err("a concurrent edit should abort hydration");
        assert!(matches!(
            error,
            LocalCacheError::MaterializationTargetExists { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("concurrent edit should remain readable"),
            edited_bytes
        );
    }

    #[cfg(any(target_os = "android", target_os = "linux", target_vendor = "apple"))]
    #[test]
    fn hydrate_pointer_file_serializes_same_path_operations() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated bytes from shared cache";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );

        let pointer_checked = Arc::new(Barrier::new(2));
        let allow_publish = Arc::new(Barrier::new(2));
        let first_layout = layout.clone();
        let first_destination = destination.clone();
        let first_pointer_checked = Arc::clone(&pointer_checked);
        let first_allow_publish = Arc::clone(&allow_publish);
        let first = thread::spawn(move || {
            first_layout.hydrate_pointer_file_with_before_publish(&first_destination, || {
                first_pointer_checked.wait();
                first_allow_publish.wait();
            })
        });

        pointer_checked.wait();
        let second_layout = layout.clone();
        let second_destination = destination.clone();
        let (second_done_tx, second_done_rx) = mpsc::channel();
        let second = thread::spawn(move || {
            second_done_tx
                .send(second_layout.hydrate_pointer_file(&second_destination))
                .expect("test should receive second hydration result");
        });

        assert!(
            second_done_rx
                .recv_timeout(Duration::from_millis(250))
                .is_err(),
            "a same-path hydration should wait for the active publication"
        );
        allow_publish.wait();

        first
            .join()
            .expect("first hydration thread should not panic")
            .expect("first hydration should succeed");
        let second_error = second_done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("second hydration should finish after publication")
            .expect_err("the completed hydration should no longer be a pointer");
        second.join().expect("second hydration should not panic");

        assert!(matches!(
            second_error,
            LocalCacheError::PointerParse { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("hydrated destination should be readable"),
            bytes
        );
    }

    #[cfg(unix)]
    #[test]
    fn hydrate_pointer_file_preserves_existing_worktree_file_mode() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"hydrated executable bytes";
        let object = object_for_bytes(bytes);
        let destination = temp.path().join("repo/assets/tool.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &destination,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        let mut permissions = fs::metadata(&destination)
            .expect("pointer should have metadata")
            .permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&destination, permissions)
            .expect("pointer mode should be made executable");

        layout
            .hydrate_pointer_file(&destination)
            .expect("matching pointer should hydrate from cache");

        let mode = fs::metadata(&destination)
            .expect("hydrated destination should have metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[test]
    fn hydrate_pointer_file_rejects_non_pointer_worktree_content() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&destination, b"dirty non-pointer contents");

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("non-pointer worktree content should not hydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerParse { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("destination should remain untouched"),
            b"dirty non-pointer contents"
        );
    }

    #[test]
    fn hydrate_pointer_file_rejects_non_utf8_worktree_content() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let destination = temp.path().join("repo/assets/model.bin");
        let bytes = b"version https://git-lfs.github.com/spec/v1\noid sha256:\xff\nsize 1\n";
        write_file(&destination, bytes);

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("non-UTF-8 worktree content should not hydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerFileInvalidUtf8 { path, .. } if path == destination
        ));
        assert_eq!(
            fs::read(&destination).expect("destination should remain untouched"),
            bytes
        );
    }

    #[test]
    fn hydrate_pointer_file_rejects_oversized_non_pointer_without_unbounded_read() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let destination = temp.path().join("repo/assets/model.bin");
        let bytes = vec![b'x'; LFS_POINTER_SIZE_CUTOFF as usize];
        write_file(&destination, &bytes);

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("oversized non-pointer worktree content should not hydrate");

        assert!(matches!(
            error,
            LocalCacheError::PointerFileTooLarge { path, size, size_cutoff }
                if path == destination
                    && size == LFS_POINTER_SIZE_CUTOFF
                    && size_cutoff == LFS_POINTER_SIZE_CUTOFF
        ));
    }

    #[cfg(unix)]
    #[test]
    fn hydrate_pointer_file_rejects_symlink_without_replacing_it() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let bytes = b"cached bytes for symlink hydration";
        let object = object_for_bytes(bytes);
        let outside_pointer = temp.path().join("outside-pointer");
        let destination = temp.path().join("repo/assets/model.bin");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &outside_pointer,
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        fs::create_dir_all(
            destination
                .parent()
                .expect("destination should have a parent"),
        )
        .expect("destination parent should be created");
        std::os::unix::fs::symlink(&outside_pointer, &destination)
            .expect("worktree symlink should be created");

        let error = layout
            .hydrate_pointer_file(&destination)
            .expect_err("a symlink must not be hydrated");

        assert!(matches!(
            error,
            LocalCacheError::WorktreePathSymlink { path } if path == destination
        ));
        assert!(
            fs::symlink_metadata(&destination)
                .expect("symlink metadata should remain readable")
                .file_type()
                .is_symlink()
        );
        assert_eq!(
            fs::read_to_string(&outside_pointer).expect("outside pointer should remain readable"),
            LfsPointer::new(object_for_bytes(bytes)).to_pointer_file()
        );
    }
}
