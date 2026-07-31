//! Object paths, hashing, verification, and cache publication.

use super::*;

pub(super) fn object_shards(hex: &str) -> [&str; OBJECT_SHARD_LEVELS] {
    debug_assert!(
        hex.len() >= OBJECT_SHARD_PREFIX_LENGTH,
        "validated SHA-256 OID should be long enough for cache sharding"
    );

    [
        hex.get(..OBJECT_SHARD_WIDTH)
            .expect("validated SHA-256 OID should contain the first cache shard"),
        hex.get(OBJECT_SHARD_WIDTH..OBJECT_SHARD_PREFIX_LENGTH)
            .expect("validated SHA-256 OID should contain the second cache shard"),
    ]
}

pub(super) fn git_lfs_object_path(git_lfs_objects_dir: &Path, oid: &LfsOid) -> PathBuf {
    let hex = oid.as_hex();
    let [first_shard, second_shard] = object_shards(hex);

    git_lfs_objects_dir
        .join(first_shard)
        .join(second_shard)
        .join(hex)
}

pub(super) fn cache_object_path_exists(path: &Path) -> LocalCacheResult<bool> {
    regular_file_exists(
        path,
        "failed to inspect cache object",
        "cache object path is not a file",
    )
}

pub(super) fn regular_file_exists(
    path: &Path,
    inspect_context: &'static str,
    wrong_type_context: &'static str,
) -> LocalCacheResult<bool> {
    match fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => Ok(true),
        Ok(_) => Err(LocalCacheError::Io {
            context: wrong_type_context,
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        }),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(LocalCacheError::Io {
            context: inspect_context,
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn ensure_cache_object_file(path: &Path, object: &LfsObject) -> LocalCacheResult<()> {
    if regular_file_exists(
        path,
        "failed to inspect cache object",
        "cache object path is not a file",
    )? {
        Ok(())
    } else {
        Err(LocalCacheError::MissingCacheObject {
            oid: object.oid.clone(),
            size: object.size,
            path: path.to_path_buf(),
        })
    }
}

pub(super) fn ensure_source_object_file(path: &Path, object: &LfsObject) -> LocalCacheResult<()> {
    if regular_file_exists(
        path,
        "failed to inspect Git LFS source object",
        "Git LFS source object path is not a file",
    )? {
        Ok(())
    } else {
        Err(LocalCacheError::MissingSourceObject {
            oid: object.oid.clone(),
            size: object.size,
            path: path.to_path_buf(),
        })
    }
}

pub(super) fn verify_file_object(
    path: &Path,
    expected: &LfsObject,
) -> LocalCacheResult<VerifiedLocalCacheObject> {
    let actual = hash_file(path)?;
    verified_object(path, expected, actual)
}

pub(super) fn verify_worktree_file_object(
    path: &Path,
    expected: &LfsObject,
) -> LocalCacheResult<VerifiedLocalCacheObject> {
    let file = open_worktree_file_without_following_symlinks(
        path,
        "failed to open worktree object for hashing",
    )?;
    let actual = hash_open_file(file, path)?;
    verified_object(path, expected, actual)
}

pub(super) fn verified_object(
    path: &Path,
    expected: &LfsObject,
    (actual_oid, actual_size): (LfsOid, LfsObjectSize),
) -> LocalCacheResult<VerifiedLocalCacheObject> {
    ensure_object_identity(path, expected, (actual_oid, actual_size))?;
    Ok(VerifiedLocalCacheObject {
        object: expected.clone(),
        path: path.to_path_buf(),
    })
}

pub(super) fn ensure_object_identity(
    path: &Path,
    expected: &LfsObject,
    (actual_oid, actual_size): (LfsOid, LfsObjectSize),
) -> LocalCacheResult<()> {
    if actual_oid != expected.oid || actual_size != expected.size {
        return Err(LocalCacheError::IntegrityMismatch {
            path: path.to_path_buf(),
            expected_oid: expected.oid.clone(),
            expected_size: expected.size,
            actual_oid,
            actual_size,
        });
    }

    Ok(())
}

pub(super) fn copy_verified_object_to_cache(
    source_path: &Path,
    cache_path: &Path,
    object: &LfsObject,
) -> LocalCacheResult<LocalCacheIngestStatus> {
    let source = File::open(source_path).map_err(|source| LocalCacheError::Io {
        context: "failed to open Git LFS source object",
        path: source_path.to_path_buf(),
        source,
    })?;
    copy_verified_file_to_cache(
        source,
        source_path,
        cache_path,
        object,
        "failed to read Git LFS source object",
        CachePublishDurability::Recoverable,
    )
}

pub(super) fn copy_verified_worktree_object_to_cache(
    source_path: &Path,
    cache_path: &Path,
    object: &LfsObject,
) -> LocalCacheResult<()> {
    let source = open_worktree_file_without_following_symlinks(
        source_path,
        "failed to open hydrated worktree object",
    )?;
    copy_verified_file_to_cache(
        source,
        source_path,
        cache_path,
        object,
        "failed to read hydrated worktree object",
        CachePublishDurability::Durable,
    )
    .map(|_| ())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CachePublishDurability {
    Recoverable,
    Durable,
}

pub(super) fn copy_verified_file_to_cache(
    mut source: File,
    source_path: &Path,
    cache_path: &Path,
    object: &LfsObject,
    read_context: &'static str,
    durability: CachePublishDurability,
) -> LocalCacheResult<LocalCacheIngestStatus> {
    let cache_parent = cache_path.parent().ok_or_else(|| LocalCacheError::Io {
        context: "failed to resolve cache object parent",
        path: cache_path.to_path_buf(),
        source: io::Error::new(
            io::ErrorKind::InvalidInput,
            "cache object path has no parent directory",
        ),
    })?;
    fs::create_dir_all(cache_parent).map_err(|source| LocalCacheError::Io {
        context: "failed to create cache object directory",
        path: cache_parent.to_path_buf(),
        source,
    })?;

    let mut temp =
        tempfile::NamedTempFile::new_in(cache_parent).map_err(|source| LocalCacheError::Io {
            context: "failed to create temporary cache object",
            path: cache_parent.to_path_buf(),
            source,
        })?;
    copy_and_verify_object(
        &mut source,
        source_path,
        &mut temp,
        cache_path,
        object,
        read_context,
    )?;
    match durability {
        CachePublishDurability::Recoverable => {
            // This deliberately stops at `flush()`: ordinary cache ingest is
            // recoverable derived state, and every cache reuse revalidates
            // object identity. Avoiding `sync_all()` keeps large-object ingest
            // from paying a durable write latency cost on the hot path.
            temp.as_file_mut()
                .flush()
                .map_err(|source| LocalCacheError::Io {
                    context: "failed to flush temporary cache object",
                    path: cache_path.to_path_buf(),
                    source,
                })?;
        }
        CachePublishDurability::Durable => {
            temp.as_file_mut()
                .sync_all()
                .map_err(|source| LocalCacheError::Io {
                    context: "failed to sync temporary cache object",
                    path: cache_path.to_path_buf(),
                    source,
                })?;
        }
    }

    match temp.persist_noclobber(cache_path) {
        Ok(published) => {
            if durability == CachePublishDurability::Durable {
                published.sync_all().map_err(|source| LocalCacheError::Io {
                    context: "failed to sync published cache object",
                    path: cache_path.to_path_buf(),
                    source,
                })?;
                sync_directory(cache_parent).map_err(|source| LocalCacheError::Io {
                    context: "failed to sync cache object directory",
                    path: cache_parent.to_path_buf(),
                    source,
                })?;
            }

            Ok(LocalCacheIngestStatus::Copied)
        }
        Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
            verify_file_object(cache_path, object)?;
            if durability == CachePublishDurability::Durable {
                sync_verified_cache_object(cache_path)?;
            }
            Ok(LocalCacheIngestStatus::AlreadyCached)
        }
        Err(error) => Err(LocalCacheError::Io {
            context: "failed to publish cache object",
            path: cache_path.to_path_buf(),
            source: error.error,
        }),
    }
}

pub(super) fn copy_and_verify_object(
    source: &mut File,
    source_path: &Path,
    destination: &mut tempfile::NamedTempFile,
    destination_path: &Path,
    expected: &LfsObject,
    read_context: &'static str,
) -> LocalCacheResult<()> {
    let actual = hash_and_optionally_copy(
        source,
        source_path,
        read_context,
        Some(ObjectCopyDestination {
            writer: destination,
            path: destination_path,
            context: "failed to write temporary cache object",
        }),
    )?;
    ensure_object_identity(source_path, expected, actual)
}

pub(super) fn hash_file(path: &Path) -> LocalCacheResult<(LfsOid, LfsObjectSize)> {
    let file = File::open(path).map_err(|source| LocalCacheError::Io {
        context: "failed to open object for hashing",
        path: path.to_path_buf(),
        source,
    })?;

    hash_open_file(file, path)
}

pub(super) fn hash_open_file(
    mut file: File,
    path: &Path,
) -> LocalCacheResult<(LfsOid, LfsObjectSize)> {
    hash_and_optionally_copy(&mut file, path, "failed to read object for hashing", None)
}

pub(super) struct ObjectCopyDestination<'a> {
    pub(super) writer: &'a mut dyn Write,
    pub(super) path: &'a Path,
    pub(super) context: &'static str,
}

pub(super) fn hash_and_optionally_copy(
    source: &mut impl Read,
    source_path: &Path,
    read_context: &'static str,
    mut destination: Option<ObjectCopyDestination<'_>>,
) -> LocalCacheResult<(LfsOid, LfsObjectSize)> {
    let mut hasher = Sha256::new();
    let mut total_size = 0u64;
    let mut buffer = [0u8; 64 * 1024];

    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|source| LocalCacheError::Io {
                context: read_context,
                path: source_path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        if let Some(destination) = destination.as_mut() {
            destination
                .writer
                .write_all(&buffer[..read])
                .map_err(|source| LocalCacheError::Io {
                    context: destination.context,
                    path: destination.path.to_path_buf(),
                    source,
                })?;
        }
        hasher.update(&buffer[..read]);
        total_size = total_size
            .checked_add(read as u64)
            .ok_or_else(|| LocalCacheError::Io {
                context: "object is too large to measure",
                path: source_path.to_path_buf(),
                source: io::Error::new(io::ErrorKind::InvalidData, "object size overflow"),
            })?;
    }

    Ok((
        LfsOid::new(format!("{:x}", hasher.finalize())).expect("SHA-256 hex should be valid"),
        LfsObjectSize::new(total_size),
    ))
}

pub(super) fn worktree_file_metadata_without_following_symlinks(
    path: &Path,
) -> LocalCacheResult<fs::Metadata> {
    let metadata = fs::symlink_metadata(path).map_err(|source| LocalCacheError::Io {
        context: "failed to inspect worktree path",
        path: path.to_path_buf(),
        source,
    })?;
    if metadata.file_type().is_symlink() {
        return Err(LocalCacheError::WorktreePathSymlink {
            path: path.to_path_buf(),
        });
    }
    if !metadata.is_file() {
        return Err(LocalCacheError::Io {
            context: "worktree path is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        });
    }

    Ok(metadata)
}

pub(super) fn open_worktree_file_without_following_symlinks(
    path: &Path,
    context: &'static str,
) -> LocalCacheResult<File> {
    worktree_file_metadata_without_following_symlinks(path)?;

    #[cfg(unix)]
    let file = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from);

    #[cfg(not(unix))]
    let file = File::open(path);

    let file = file.map_err(|source| {
        if fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            LocalCacheError::WorktreePathSymlink {
                path: path.to_path_buf(),
            }
        } else {
            LocalCacheError::Io {
                context,
                path: path.to_path_buf(),
                source,
            }
        }
    })?;
    if !file
        .metadata()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to inspect opened worktree file",
            path: path.to_path_buf(),
            source,
        })?
        .is_file()
    {
        return Err(LocalCacheError::Io {
            context: "opened worktree path is not a file",
            path: path.to_path_buf(),
            source: io::Error::new(io::ErrorKind::InvalidData, "expected a regular file"),
        });
    }

    Ok(file)
}
