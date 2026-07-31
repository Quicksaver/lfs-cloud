// This file is included by `mod.rs` so the migration API remains in one module.

/// Local availability check for discovered migration objects.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LocalMigrationObjectAvailability {
    /// Git worktree root whose local object stores were inspected.
    pub worktree_root: PathBuf,
    /// Repository-local Git LFS media object directory that was inspected.
    pub git_lfs_objects_dir: PathBuf,
    /// Shared LFS Cloud cache root available as a fallback, when supplied.
    pub shared_cache_root: Option<PathBuf>,
    /// Deduplicated object availability records in stable object order.
    pub objects: Vec<LocalMigrationObject>,
}

impl LocalMigrationObjectAvailability {
    /// Returns objects with at least one verified local copy.
    #[must_use]
    pub fn available_objects(&self) -> Vec<&LocalMigrationObject> {
        self.objects
            .iter()
            .filter(|object| object.is_available())
            .collect()
    }

    /// Returns objects without any verified local copy.
    #[must_use]
    pub fn unavailable_objects(&self) -> Vec<&LocalMigrationObject> {
        self.objects
            .iter()
            .filter(|object| !object.is_available())
            .collect()
    }
}
/// Local availability details for one discovered Git LFS object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LocalMigrationObject {
    /// Git LFS object identity being checked.
    pub object: LfsObject,
    /// Locations checked for a verified copy of this object.
    pub locations: Vec<LocalMigrationObjectLocation>,
}

impl LocalMigrationObject {
    /// Returns true when any checked location contains verified object bytes.
    #[must_use]
    pub fn is_available(&self) -> bool {
        self.locations.iter().any(|location| {
            matches!(
                location.status,
                LocalMigrationObjectLocationStatus::Available
            )
        })
    }
}

/// One local storage location checked for a migration object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct LocalMigrationObjectLocation {
    /// Kind of local object store inspected.
    pub kind: LocalMigrationObjectLocationKind,
    /// Filesystem path expected to contain the object bytes.
    pub path: PathBuf,
    /// Availability status for this location.
    pub status: LocalMigrationObjectLocationStatus,
}

/// Kind of local object store inspected during migration planning.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LocalMigrationObjectLocationKind {
    /// Repository-local Git LFS media storage, normally `.git/lfs/objects`.
    GitLfsMedia,
    /// Shared LFS Cloud content-addressed cache.
    SharedCache,
}

/// Availability state for a checked local migration object location.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum LocalMigrationObjectLocationStatus {
    /// The location contains bytes whose SHA-256 and size match the pointer.
    Available,
    /// The expected local object path does not exist.
    Missing,
    /// The path exists but cannot be used as the requested object.
    Invalid {
        /// Safe diagnostic explaining why the local bytes are unusable.
        message: SanitizedMessage,
    },
}

/// Checks whether discovered migration objects already have verified local bytes.
///
/// The repository's stock Git LFS media directory is always checked first.
/// When it does not contain a verified copy and a shared LFS Cloud cache layout
/// is supplied, that cache is checked as a fallback. The helper is
/// intentionally read-only: missing or corrupt objects are reported in the
/// returned availability records instead of fetching or rewriting local state.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree or
/// Git cannot resolve the repository's local object storage configuration.
pub fn check_local_migration_objects<I, O>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
) -> MigrationResult<LocalMigrationObjectAvailability>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
{
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let git_lfs_objects_dir = migration_git_lfs_objects_dir(&worktree_root)?;
    let shared_cache_root = shared_cache.map(|layout| layout.root().to_path_buf());
    let mut seen_objects = BTreeSet::new();
    let objects = objects
        .into_iter()
        .filter_map(|object| {
            let object = object.borrow().clone();
            seen_objects.insert(object.clone()).then_some(object)
        })
        .collect::<Vec<_>>();

    let objects = objects
        .into_iter()
        .map(|object| {
            let git_lfs_location = check_local_migration_object_location(
                LocalMigrationObjectLocationKind::GitLfsMedia,
                git_lfs_object_path(&git_lfs_objects_dir, &object.oid)?,
                &object,
            )?;
            let git_lfs_media_is_available = matches!(
                &git_lfs_location.status,
                LocalMigrationObjectLocationStatus::Available
            );
            let mut locations = vec![git_lfs_location];

            if !git_lfs_media_is_available && let Some(layout) = shared_cache {
                locations.push(check_local_migration_object_location(
                    LocalMigrationObjectLocationKind::SharedCache,
                    layout.object_path(&object),
                    &object,
                )?);
            }

            Ok(LocalMigrationObject { object, locations })
        })
        .collect::<MigrationResult<Vec<_>>>()?;

    Ok(LocalMigrationObjectAvailability {
        worktree_root,
        git_lfs_objects_dir,
        shared_cache_root,
        objects,
    })
}
fn migration_git_lfs_objects_dir(worktree_root: &Path) -> MigrationResult<PathBuf> {
    let git_common_dir = git_absolute_path(
        worktree_root,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        "git rev-parse --path-format=absolute --git-common-dir",
    )?;
    let storage_dir = match configured_git_lfs_storage_dir(worktree_root)? {
        Some(storage_dir) if storage_dir.is_absolute() => storage_dir,
        Some(storage_dir) => git_common_dir.join(storage_dir),
        None => git_common_dir.join("lfs"),
    };

    Ok(storage_dir.join("objects"))
}

fn git_absolute_path<const N: usize>(
    worktree_root: &Path,
    args: [&str; N],
    command_name: &str,
) -> MigrationResult<PathBuf> {
    let output = run_git(worktree_root, args)?;
    let stdout = required_success_stdout(output, command_name)?;

    Ok(PathBuf::from(stdout.trim_end_matches(['\n', '\r'])))
}

fn configured_git_lfs_storage_dir(worktree_root: &Path) -> MigrationResult<Option<PathBuf>> {
    git_config_get(worktree_root, ["config", "--get", "lfs.storage"]).map(|storage| {
        storage.and_then(|storage| {
            let storage = storage.trim();
            (!storage.is_empty()).then(|| PathBuf::from(storage))
        })
    })
}

fn check_local_migration_object_location(
    kind: LocalMigrationObjectLocationKind,
    path: PathBuf,
    object: &LfsObject,
) -> MigrationResult<LocalMigrationObjectLocation> {
    let status = match fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            LocalMigrationObjectLocationStatus::Invalid {
                message: SanitizedMessage::new("local object path is a symbolic link"),
            }
        }
        Ok(metadata) if metadata.is_file() => {
            let metadata_size = LfsObjectSize::new(metadata.len());
            if metadata_size != object.size {
                LocalMigrationObjectLocationStatus::Invalid {
                    message: SanitizedMessage::new(format!(
                        "expected sha256:{} ({} bytes), got local object with {} bytes",
                        object.oid, object.size, metadata_size
                    )),
                }
            } else {
                match hash_migration_object_file(&path) {
                    Ok((actual_oid, actual_size)) => {
                        if actual_oid == object.oid && actual_size == object.size {
                            LocalMigrationObjectLocationStatus::Available
                        } else {
                            LocalMigrationObjectLocationStatus::Invalid {
                                message: SanitizedMessage::new(format!(
                                    "expected sha256:{} ({} bytes), got sha256:{} ({} bytes)",
                                    object.oid, object.size, actual_oid, actual_size
                                )),
                            }
                        }
                    }
                    Err(source) => LocalMigrationObjectLocationStatus::Invalid {
                        message: local_object_verification_failure_message(&source),
                    },
                }
            }
        }
        Ok(_) => LocalMigrationObjectLocationStatus::Invalid {
            message: SanitizedMessage::new("local object path is not a regular file"),
        },
        Err(source) if source.kind() == io::ErrorKind::NotFound => {
            LocalMigrationObjectLocationStatus::Missing
        }
        Err(source) => LocalMigrationObjectLocationStatus::Invalid {
            message: SanitizedMessage::new(format!(
                "failed to inspect local object path: {}",
                source.kind()
            )),
        },
    };

    Ok(LocalMigrationObjectLocation { kind, path, status })
}

fn local_object_verification_failure_message(error: &MigrationError) -> SanitizedMessage {
    match error {
        MigrationError::Io { source, .. } => SanitizedMessage::new(format!(
            "failed to verify local object bytes: {}",
            source.kind()
        )),
        _ => SanitizedMessage::new("failed to verify local object bytes"),
    }
}

fn hash_migration_object_file(path: &Path) -> MigrationResult<(LfsOid, LfsObjectSize)> {
    let mut file = File::open(path).map_err(|source| MigrationError::Io {
        context: format!("failed to open local migration object {}", path.display()),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut total_size = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read local migration object {}", path.display()),
                source,
            })?;
        if read == 0 {
            break;
        }

        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or_else(|| MigrationError::Io {
                context: format!(
                    "failed to measure local migration object {}",
                    path.display()
                ),
                source: io::Error::other("local migration object is too large to measure"),
            })?;
    }

    Ok((
        LfsOid::new(format!("{:x}", hasher.finalize())).expect("SHA-256 hex should be valid"),
        LfsObjectSize::new(total_size),
    ))
}

fn git_lfs_object_path(git_lfs_objects_dir: &Path, oid: &LfsOid) -> MigrationResult<PathBuf> {
    let hex = oid.as_hex();
    let first_shard = hex.get(..2).ok_or_else(|| MigrationError::InvalidInput {
        message: SanitizedMessage::new("validated SHA-256 object ID is too short"),
    })?;
    let second_shard = hex.get(2..4).ok_or_else(|| MigrationError::InvalidInput {
        message: SanitizedMessage::new("validated SHA-256 object ID is too short"),
    })?;

    Ok(git_lfs_objects_dir
        .join(first_shard)
        .join(second_shard)
        .join(hex))
}


#[cfg(test)]
mod local_objects_tests {
    use super::test_support::*;

    #[test]
    fn local_object_check_verifies_git_lfs_media_and_deduplicates_objects() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"local object bytes");
        write_git_lfs_source_object(&repo, &object, b"local object bytes");

        let availability = check_local_migration_objects(repo.path(), [&object, &object], None)
            .expect("local object check should succeed");

        assert_eq!(availability.objects.len(), 1);
        assert_eq!(availability.available_objects().len(), 1);
        assert_eq!(availability.unavailable_objects().len(), 0);
        assert_eq!(availability.objects[0].object, object);
        assert!(availability.objects[0].is_available());
        assert_eq!(availability.objects[0].locations.len(), 1);
        assert_eq!(
            availability.objects[0].locations[0].kind,
            LocalMigrationObjectLocationKind::GitLfsMedia
        );
        assert_eq!(
            availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Available
        );
    }

    #[test]
    fn local_object_check_preserves_first_seen_object_order() {
        let repo = TempRepo::new();
        let later_sorting_object = test_lfs_object('f', 222);
        let earlier_sorting_object = test_lfs_object('a', 111);

        let availability = check_local_migration_objects(
            repo.path(),
            [
                &later_sorting_object,
                &earlier_sorting_object,
                &later_sorting_object,
            ],
            None,
        )
        .expect("local object check should succeed");

        let objects = availability
            .objects
            .iter()
            .map(|record| &record.object)
            .collect::<Vec<_>>();
        assert_eq!(
            objects,
            vec![&later_sorting_object, &earlier_sorting_object]
        );
    }

    #[test]
    fn local_object_check_reports_missing_and_corrupt_git_lfs_media_objects() {
        let repo = TempRepo::new();
        let missing = test_lfs_object_from_bytes(b"missing object bytes");
        let corrupt = test_lfs_object_from_bytes(b"expected object bytes");
        write_git_lfs_source_object(&repo, &corrupt, b"different object bytes");

        let availability = check_local_migration_objects(repo.path(), [&missing, &corrupt], None)
            .expect("local object check should succeed");

        let missing_record = availability
            .objects
            .iter()
            .find(|record| record.object == missing)
            .expect("missing object should be reported");
        assert!(!missing_record.is_available());
        assert_eq!(
            missing_record.locations[0].status,
            LocalMigrationObjectLocationStatus::Missing
        );

        let corrupt_record = availability
            .objects
            .iter()
            .find(|record| record.object == corrupt)
            .expect("corrupt object should be reported");
        assert!(!corrupt_record.is_available());
        assert!(matches!(
            &corrupt_record.locations[0].status,
            LocalMigrationObjectLocationStatus::Invalid { message }
                if message.as_str().contains("expected sha256:")
        ));
    }

    #[cfg(unix)]
    #[test]
    fn local_object_check_rejects_symbolic_link_media_objects() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"linked object bytes");
        let target_path = repo.path().join("linked-object-target");
        let media_object_path =
            git_lfs_object_path(&repo.path().join(".git/lfs/objects"), &object.oid)
                .expect("test object path should be valid");
        write_file(&target_path, b"linked object bytes");
        fs::create_dir_all(
            media_object_path
                .parent()
                .expect("media object path should have a parent"),
        )
        .expect("media object parent should be created");
        std::os::unix::fs::symlink(&target_path, &media_object_path)
            .expect("media object symlink should be created");

        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local object check should succeed");

        assert!(matches!(
            &availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Invalid { message }
                if message.as_str().contains("symbolic link")
        ));
    }

    #[test]
    fn local_object_check_uses_configured_git_lfs_storage_dir() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"configured storage bytes");
        repo.git(["config", "lfs.storage", "custom-lfs-storage"]);
        let storage_objects_dir = repo
            .path()
            .join(".git")
            .join("custom-lfs-storage")
            .join("objects");
        write_git_lfs_source_object_in(&storage_objects_dir, &object, b"configured storage bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local object check should use configured lfs.storage");

        assert_eq!(
            availability
                .git_lfs_objects_dir
                .canonicalize()
                .expect("reported storage path should canonicalize"),
            storage_objects_dir
                .canonicalize()
                .expect("expected storage path should canonicalize")
        );
        assert!(availability.objects[0].is_available());
    }

    #[test]
    fn local_object_check_treats_empty_configured_git_lfs_storage_dir_as_default() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"default storage bytes");
        repo.git(["config", "lfs.storage", ""]);
        let storage_objects_dir = repo.path().join(".git").join("lfs").join("objects");
        write_git_lfs_source_object_in(&storage_objects_dir, &object, b"default storage bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local object check should use default lfs.storage");

        assert_eq!(
            availability
                .git_lfs_objects_dir
                .canonicalize()
                .expect("reported storage path should canonicalize"),
            storage_objects_dir
                .canonicalize()
                .expect("expected storage path should canonicalize")
        );
        assert!(availability.objects[0].is_available());
    }

    #[test]
    fn local_object_check_uses_shared_cache_when_supplied() {
        let repo = TempRepo::new();
        let cache_root = tempfile::tempdir().expect("temporary cache root should be created");
        let layout = LocalCacheLayout::new(cache_root.path());
        let object = test_lfs_object_from_bytes(b"shared cache bytes");
        write_file(&layout.object_path(&object), b"shared cache bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], Some(&layout))
            .expect("local object check should inspect shared cache");

        assert_eq!(
            availability.shared_cache_root.as_deref(),
            Some(cache_root.path())
        );
        assert!(availability.objects[0].is_available());
        assert_eq!(availability.objects[0].locations.len(), 2);
        assert_eq!(
            availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Missing
        );
        assert_eq!(
            availability.objects[0].locations[1].kind,
            LocalMigrationObjectLocationKind::SharedCache
        );
        assert_eq!(
            availability.objects[0].locations[1].status,
            LocalMigrationObjectLocationStatus::Available
        );
    }

    #[test]
    fn local_object_check_skips_shared_cache_after_verified_git_lfs_media() {
        let repo = TempRepo::new();
        let cache_root = tempfile::tempdir().expect("temporary cache root should be created");
        let layout = LocalCacheLayout::new(cache_root.path());
        let object = test_lfs_object_from_bytes(b"preferred media bytes");
        write_git_lfs_source_object(&repo, &object, b"preferred media bytes");
        write_file(&layout.object_path(&object), b"preferred media bytes");

        let availability = check_local_migration_objects(repo.path(), [&object], Some(&layout))
            .expect("verified media should satisfy local availability");

        assert_eq!(availability.objects[0].locations.len(), 1);
        assert_eq!(
            availability.objects[0].locations[0].kind,
            LocalMigrationObjectLocationKind::GitLfsMedia
        );
        assert_eq!(
            availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Available
        );
    }

    #[cfg(unix)]
    #[test]
    fn local_object_check_keeps_checking_after_unreadable_media_object() {
        let repo = TempRepo::new();
        let cache_root = tempfile::tempdir().expect("temporary cache root should be created");
        let layout = LocalCacheLayout::new(cache_root.path());
        let object = test_lfs_object_from_bytes(b"shared cache fallback bytes");
        let media_object_path =
            git_lfs_object_path(&repo.path().join(".git/lfs/objects"), &object.oid)
                .expect("test object path should be valid");
        write_git_lfs_source_object(&repo, &object, b"shared cache fallback bytes");
        write_file(&layout.object_path(&object), b"shared cache fallback bytes");

        let original_permissions = fs::metadata(&media_object_path)
            .expect("media object metadata should be readable")
            .permissions();
        let mut unreadable_permissions = original_permissions.clone();
        unreadable_permissions.set_mode(0o000);
        fs::set_permissions(&media_object_path, unreadable_permissions)
            .expect("media object should be made unreadable");
        if fs::File::open(&media_object_path).is_ok() {
            fs::set_permissions(&media_object_path, original_permissions)
                .expect("media object permissions should be restored");
            return;
        }

        let availability_result =
            check_local_migration_objects(repo.path(), [&object], Some(&layout));
        fs::set_permissions(&media_object_path, original_permissions)
            .expect("media object permissions should be restored");
        let availability =
            availability_result.expect("unreadable media should not abort cache inspection");

        assert!(availability.objects[0].is_available());
        assert!(matches!(
            &availability.objects[0].locations[0].status,
            LocalMigrationObjectLocationStatus::Invalid { message }
                if message.as_str().contains("failed to verify local object bytes")
        ));
        assert_eq!(
            availability.objects[0].locations[1].status,
            LocalMigrationObjectLocationStatus::Available
        );
    }

}
