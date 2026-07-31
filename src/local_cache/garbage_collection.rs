//! Local cache reachability analysis and garbage collection.

use super::*;

impl LocalCacheLayout {
    /// Removes cached objects not referenced by any registered worktree.
    ///
    /// Reachability is intentionally conservative and local: the collector asks
    /// Git for NUL-delimited tracked paths in each registered worktree, filters
    /// those paths by the effective `filter=lfs` index attribute, and keeps
    /// every cached OID that a matching worktree pointer references. When any
    /// registered root is unavailable, objects not referenced by the remaining
    /// roots are
    /// protected unless `prune_unavailable_worktrees` is true. Explicit pruning
    /// treats unavailable roots as permanently abandoned. Cache paths that do
    /// not match the expected sharded SHA-256 layout are reported but never
    /// deleted.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the worktree registry cannot be read or
    /// written, a registered worktree cannot be scanned, or a cache object
    /// cannot be removed.
    pub fn garbage_collect(
        &self,
        dry_run: bool,
        prune_unavailable_worktrees: bool,
    ) -> LocalCacheResult<LocalCacheGarbageCollection> {
        // Mutations and materializations take the shared side of this lock.
        // Taking it exclusively gives GC a stable cache/worktree snapshot and
        // keeps it out of multi-step publication windows.
        let _operation_lock = self.lock_cache_operation_exclusive()?;
        // Keep registry roots stable while reachability is computed and cache
        // objects are deleted; otherwise a concurrent worktree registration
        // could lose cache bytes before its pointers are considered.
        let _lock = self.lock_worktree_registry()?;
        let mut registry = self.load_worktree_registry()?;
        let (active_worktrees, unavailable_worktrees) =
            partition_existing_worktrees(registry.worktrees())?;
        let (referenced_oids, mut skipped_worktree_pointer_paths) =
            referenced_worktree_oids(&active_worktrees)?;
        let (mut cache_objects, mut skipped_cache_paths) = self.cache_object_files()?;
        let mut retained_objects = Vec::new();
        let mut protected_objects = Vec::new();
        let mut unreferenced_objects = Vec::new();
        let mut deleted_objects = Vec::new();
        let pruned_worktrees = if prune_unavailable_worktrees {
            unavailable_worktrees.clone()
        } else {
            Vec::new()
        };

        cache_objects.sort_by(|left, right| left.path.cmp(&right.path));
        skipped_cache_paths.sort();
        skipped_worktree_pointer_paths.sort();
        skipped_worktree_pointer_paths.dedup();

        if !dry_run && prune_unavailable_worktrees && !pruned_worktrees.is_empty() {
            for registration in &pruned_worktrees {
                registry.remove(&registration.worktree_root);
            }
            self.save_worktree_registry(&registry)?;
        }

        for object in cache_objects {
            if referenced_oids.contains(&object.oid) {
                retained_objects.push(object);
            } else if !unavailable_worktrees.is_empty() && !prune_unavailable_worktrees {
                // An unavailable worktree may contain the only pointer keeping
                // this object reachable, so absence from the scanned roots is
                // not enough evidence for destructive collection.
                protected_objects.push(object);
            } else {
                if !dry_run {
                    self.delete_cache_object(&object)?;
                    deleted_objects.push(object.clone());
                }
                unreferenced_objects.push(object);
            }
        }

        Ok(LocalCacheGarbageCollection {
            dry_run,
            active_worktree_count: active_worktrees.len(),
            unavailable_worktrees,
            pruned_worktrees,
            retained_objects,
            protected_objects,
            unreferenced_objects,
            deleted_objects,
            skipped_cache_paths,
            skipped_worktree_pointer_paths,
        })
    }

    fn cache_object_files(
        &self,
    ) -> LocalCacheResult<(Vec<LocalCacheGarbageCollectionObject>, Vec<PathBuf>)> {
        collect_cache_object_files(&self.objects_dir())
    }

    fn delete_cache_object(
        &self,
        object: &LocalCacheGarbageCollectionObject,
    ) -> LocalCacheResult<()> {
        fs::remove_file(&object.path).map_err(|source| LocalCacheError::Io {
            context: "failed to remove unreferenced local cache object",
            path: object.path.clone(),
            source,
        })?;
        remove_empty_cache_shard_dirs(&object.path, &self.objects_dir())
    }
}
pub(super) fn partition_existing_worktrees(
    registrations: &[LocalCacheWorktreeRegistration],
) -> LocalCacheResult<(
    Vec<LocalCacheWorktreeRegistration>,
    Vec<LocalCacheWorktreeRegistration>,
)> {
    let mut active = Vec::new();
    let mut missing = Vec::new();

    for registration in registrations {
        match fs::metadata(&registration.worktree_root) {
            Ok(metadata) if metadata.is_dir() => active.push(registration.clone()),
            Ok(_) => missing.push(registration.clone()),
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                missing.push(registration.clone());
            }
            Err(source) => {
                return Err(LocalCacheError::Io {
                    context: "failed to inspect registered worktree root",
                    path: registration.worktree_root.clone(),
                    source,
                });
            }
        }
    }

    Ok((active, missing))
}

pub(super) fn referenced_worktree_oids(
    registrations: &[LocalCacheWorktreeRegistration],
) -> LocalCacheResult<(BTreeSet<LfsOid>, Vec<PathBuf>)> {
    let mut referenced = BTreeSet::new();
    let mut skipped_worktree_pointer_paths = Vec::new();

    for registration in registrations {
        collect_tracked_lfs_pointer_oids(
            registration,
            &mut referenced,
            &mut skipped_worktree_pointer_paths,
        )?;
    }

    Ok((referenced, skipped_worktree_pointer_paths))
}

pub(super) fn collect_tracked_lfs_pointer_oids(
    registration: &LocalCacheWorktreeRegistration,
    referenced: &mut BTreeSet<LfsOid>,
    skipped_worktree_pointer_paths: &mut Vec<PathBuf>,
) -> LocalCacheResult<()> {
    const LS_FILES: &str = "git ls-files -z";
    let tracked_paths = registered_git_command(registration)
        .args(["ls-files", "-z"])
        .output()
        .map_err(|source| LocalCacheError::Io {
            context: "failed to start git ls-files -z for local cache garbage collection",
            path: registration.worktree_root.clone(),
            source,
        })?;
    if !tracked_paths.status.success() {
        return Err(git_command_failed(
            LS_FILES,
            registration,
            tracked_paths.status,
        ));
    }
    if tracked_paths.stdout.is_empty() {
        return Ok(());
    }

    let attributes = check_tracked_path_filter_attributes(registration, tracked_paths.stdout)?;
    let lfs_paths = parse_lfs_filter_attribute_paths(&attributes)
        .map_err(|error| local_cache_git_path_output_error(error, registration))?;
    for relative_path in lfs_paths {
        let path = registration.worktree_root.join(relative_path);
        if collect_pointer_oid_from_file(&path, referenced)? {
            skipped_worktree_pointer_paths.push(path);
        }
    }

    Ok(())
}

pub(super) fn registered_git_command(registration: &LocalCacheWorktreeRegistration) -> Command {
    let mut command = Command::new("git");
    command
        .arg("--git-dir")
        .arg(&registration.git_dir)
        .arg("--work-tree")
        .arg(&registration.worktree_root)
        .current_dir(&registration.worktree_root);
    command
}

pub(super) fn check_tracked_path_filter_attributes(
    registration: &LocalCacheWorktreeRegistration,
    tracked_paths: Vec<u8>,
) -> LocalCacheResult<Vec<u8>> {
    const CHECK_ATTR: &str = "git check-attr --cached -z --stdin filter";
    let mut command = registered_git_command(registration);
    command
        .args(["check-attr", "--cached", "-z", "--stdin", "filter"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    configure_process_tree(&mut command);
    let mut child = command.spawn().map_err(|source| LocalCacheError::Io {
        context: "failed to start git check-attr for local cache garbage collection",
        path: registration.worktree_root.clone(),
        source,
    })?;
    let mut stdin = child
        .stdin
        .take()
        .expect("Git attribute stdin should be piped");
    let writer = std::thread::spawn(move || {
        let result = stdin.write_all(&tracked_paths);
        drop(stdin);
        result
    });
    let output = wait_for_child(
        &mut child,
        CHECK_ATTR,
        ChildProcessOptions {
            timeout: None,
            stdout: PipeCapture::Unlimited,
            stderr: PipeCapture::Unlimited,
            inherited_pipe_is_error: false,
        },
    )
    .map_err(|error| local_cache_child_process_error(error, registration))?;
    let write_result = writer.join().map_err(|_| LocalCacheError::Io {
        context: "git check-attr input writer panicked during local cache garbage collection",
        path: registration.worktree_root.clone(),
        source: io::Error::other("git check-attr input writer panicked"),
    })?;

    if !output.status.success() {
        return Err(git_command_failed(CHECK_ATTR, registration, output.status));
    }
    write_result.map_err(|source| LocalCacheError::Io {
        context: "failed to write tracked paths to git check-attr",
        path: registration.worktree_root.clone(),
        source,
    })?;

    Ok(output.stdout)
}

pub(super) fn local_cache_child_process_error(
    error: ChildProcessError,
    registration: &LocalCacheWorktreeRegistration,
) -> LocalCacheError {
    let (context, source) = match error {
        ChildProcessError::Io { context, source } => {
            let context = if context.starts_with("failed to read stdout") {
                "failed to read git check-attr stdout during local cache garbage collection"
            } else if context.starts_with("failed to read stderr") {
                "failed to read git check-attr stderr during local cache garbage collection"
            } else if context.contains("reader thread panicked") {
                "git check-attr output reader panicked during local cache garbage collection"
            } else {
                "failed to wait for git check-attr during local cache garbage collection"
            };
            (context, source)
        }
        ChildProcessError::TimedOut { .. } => (
            "git check-attr unexpectedly timed out",
            io::Error::new(io::ErrorKind::TimedOut, "no timeout was configured"),
        ),
        ChildProcessError::OutputLimit { .. } => (
            "git check-attr unexpectedly exceeded an output limit",
            io::Error::other("no output limit was configured"),
        ),
        ChildProcessError::InheritedPipe => (
            "timed out draining git check-attr output",
            io::Error::new(
                io::ErrorKind::TimedOut,
                "Git output pipes remained open after process-tree cleanup",
            ),
        ),
    };

    LocalCacheError::Io {
        context,
        path: registration.worktree_root.clone(),
        source,
    }
}

pub(super) fn git_command_failed(
    command: &'static str,
    registration: &LocalCacheWorktreeRegistration,
    status: std::process::ExitStatus,
) -> LocalCacheError {
    LocalCacheError::GitCommandFailed {
        command,
        worktree_root: registration.worktree_root.clone(),
        status: status.to_string(),
    }
}

pub(super) fn git_command_output(
    command: &'static str,
    registration: &LocalCacheWorktreeRegistration,
    message: &'static str,
) -> LocalCacheError {
    LocalCacheError::GitCommandOutput {
        command,
        worktree_root: registration.worktree_root.clone(),
        message,
    }
}

pub(super) fn local_cache_git_path_output_error(
    error: GitPathOutputError,
    registration: &LocalCacheWorktreeRegistration,
) -> LocalCacheError {
    let message = match error {
        GitPathOutputError::MalformedAttributeOutput => {
            "expected path, attribute, and value triples"
        }
        #[cfg(not(unix))]
        GitPathOutputError::NonUtf8Path => "returned a non-UTF-8 path",
        GitPathOutputError::PathOutsideWorktree => {
            "returned a path outside the registered worktree"
        }
    };
    git_command_output(
        "git check-attr --cached -z --stdin filter",
        registration,
        message,
    )
}

pub(super) fn collect_pointer_oid_from_file(
    path: &Path,
    referenced: &mut BTreeSet<LfsOid>,
) -> LocalCacheResult<bool> {
    collect_pointer_oid_from_file_with_before_open(path, referenced, || {})
}

pub(super) fn collect_pointer_oid_from_file_with_before_open(
    path: &Path,
    referenced: &mut BTreeSet<LfsOid>,
    before_open: impl FnOnce(),
) -> LocalCacheResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if !metadata.is_file() => return Ok(true),
        Ok(_) => {}
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(LocalCacheError::Io {
                context: "failed to inspect worktree pointer candidate",
                path: path.to_path_buf(),
                source,
            });
        }
    }
    before_open();

    #[cfg(unix)]
    let file = rustix::fs::openat(
        rustix::fs::CWD,
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK
            | rustix::fs::OFlags::NOFOLLOW,
        rustix::fs::Mode::empty(),
    )
    .map(File::from)
    .map_err(io::Error::from);

    #[cfg(not(unix))]
    let file = File::open(path);

    let file = match file {
        Ok(file) => file,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(_source) if fs::symlink_metadata(path).is_ok_and(|metadata| !metadata.is_file()) => {
            return Ok(true);
        }
        Err(source) => {
            return Err(LocalCacheError::Io {
                context: "failed to open worktree pointer candidate",
                path: path.to_path_buf(),
                source,
            });
        }
    };
    let bounded_contents = read_bounded_pointer_bytes(
        file,
        path,
        "failed to inspect worktree pointer candidate",
        "failed to read worktree pointer candidate",
    )?;
    let contents = match bounded_contents {
        BoundedPointerBytes::Contents(contents) => contents,
        BoundedPointerBytes::NotRegularFile => return Ok(true),
        BoundedPointerBytes::TooLarge { .. } => return Ok(false),
    };
    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(false);
    };
    if let Ok(pointer) = LfsPointer::parse(contents)
        && !pointer.is_empty()
    {
        referenced.insert(pointer.object.oid);
    }

    Ok(false)
}

pub(super) enum BoundedPointerBytes {
    Contents(Vec<u8>),
    NotRegularFile,
    TooLarge { size: u64 },
}

pub(super) fn read_bounded_pointer_bytes(
    file: File,
    path: &Path,
    inspect_context: &'static str,
    read_context: &'static str,
) -> LocalCacheResult<BoundedPointerBytes> {
    let metadata = file.metadata().map_err(|source| LocalCacheError::Io {
        context: inspect_context,
        path: path.to_path_buf(),
        source,
    })?;
    if !metadata.is_file() {
        return Ok(BoundedPointerBytes::NotRegularFile);
    }
    if metadata.len() >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(BoundedPointerBytes::TooLarge {
            size: metadata.len(),
        });
    }

    let mut contents = Vec::new();
    file.take(LFS_POINTER_SIZE_CUTOFF)
        .read_to_end(&mut contents)
        .map_err(|source| LocalCacheError::Io {
            context: read_context,
            path: path.to_path_buf(),
            source,
        })?;
    let size = u64::try_from(contents.len()).unwrap_or(u64::MAX);
    if size >= LFS_POINTER_SIZE_CUTOFF {
        Ok(BoundedPointerBytes::TooLarge { size })
    } else {
        Ok(BoundedPointerBytes::Contents(contents))
    }
}

pub(super) fn collect_cache_object_files(
    objects_dir: &Path,
) -> LocalCacheResult<(Vec<LocalCacheGarbageCollectionObject>, Vec<PathBuf>)> {
    let mut objects = Vec::new();
    let mut skipped = Vec::new();
    let first_shards = match fs::read_dir(objects_dir) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok((objects, skipped)),
        Err(source) => {
            return Err(LocalCacheError::Io {
                context: "failed to read local cache objects directory",
                path: objects_dir.to_path_buf(),
                source,
            });
        }
    };

    for first_shard in first_shards {
        let first_shard = read_cache_entry(first_shard, objects_dir)?;
        let first_path = first_shard.path();
        if !entry_is_kind(
            &first_shard,
            "first-level cache shard",
            fs::FileType::is_dir,
        )? {
            skipped.push(first_path);
            continue;
        }

        let second_shards = fs::read_dir(&first_path).map_err(|source| LocalCacheError::Io {
            context: "failed to read local cache shard directory",
            path: first_path.clone(),
            source,
        })?;
        for second_shard in second_shards {
            let second_shard = read_cache_entry(second_shard, &first_path)?;
            let second_path = second_shard.path();
            if !entry_is_kind(
                &second_shard,
                "second-level cache shard",
                fs::FileType::is_dir,
            )? {
                skipped.push(second_path);
                continue;
            }

            let object_entries =
                fs::read_dir(&second_path).map_err(|source| LocalCacheError::Io {
                    context: "failed to read local cache shard directory",
                    path: second_path.clone(),
                    source,
                })?;
            for object_entry in object_entries {
                let object_entry = read_cache_entry(object_entry, &second_path)?;
                let path = object_entry.path();
                if !entry_is_kind(&object_entry, "cache object path", fs::FileType::is_file)? {
                    skipped.push(path);
                    continue;
                }

                match cache_object_from_entry(&object_entry) {
                    Some(object) => objects.push(object),
                    None => skipped.push(path),
                }
            }
        }
    }

    Ok((objects, skipped))
}

pub(super) fn read_cache_entry(
    entry: io::Result<fs::DirEntry>,
    directory: &Path,
) -> LocalCacheResult<fs::DirEntry> {
    entry.map_err(|source| LocalCacheError::Io {
        context: "failed to read local cache directory entry",
        path: directory.to_path_buf(),
        source,
    })
}

pub(super) fn entry_is_kind(
    entry: &fs::DirEntry,
    label: &'static str,
    matches: fn(&fs::FileType) -> bool,
) -> LocalCacheResult<bool> {
    entry
        .file_type()
        .map(|file_type| matches(&file_type))
        .map_err(|source| LocalCacheError::Io {
            context: label,
            path: entry.path(),
            source,
        })
}

pub(super) fn cache_object_from_entry(
    entry: &fs::DirEntry,
) -> Option<LocalCacheGarbageCollectionObject> {
    let path = entry.path();
    let oid = LfsOid::new(entry.file_name().to_str()?).ok()?;
    let [first_shard, second_shard] = object_shards(oid.as_hex());
    let second_directory = path.parent()?;
    let first_directory = second_directory.parent()?;

    if first_directory.file_name()?.to_str()? != first_shard
        || second_directory.file_name()?.to_str()? != second_shard
    {
        return None;
    }

    let size_bytes = entry.metadata().ok()?.len();

    Some(LocalCacheGarbageCollectionObject {
        oid,
        path,
        size_bytes,
    })
}

pub(super) fn remove_empty_cache_shard_dirs(
    cache_object_path: &Path,
    objects_dir: &Path,
) -> LocalCacheResult<()> {
    let Some(second_shard) = cache_object_path.parent() else {
        return Ok(());
    };
    let Some(first_shard) = second_shard.parent() else {
        return Ok(());
    };

    remove_empty_directory(second_shard)?;
    if first_shard != objects_dir {
        remove_empty_directory(first_shard)?;
    }

    Ok(())
}

pub(super) fn remove_empty_directory(path: &Path) -> LocalCacheResult<()> {
    match fs::remove_dir(path) {
        Ok(()) => Ok(()),
        Err(source)
            if matches!(
                source.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::DirectoryNotEmpty
            ) =>
        {
            Ok(())
        }
        Err(source) => Err(LocalCacheError::Io {
            context: "failed to remove empty local cache shard directory",
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn validate_absolute_path(field: &'static str, path: &Path) -> LocalCacheResult<()> {
    if !path.is_absolute() {
        return Err(LocalCacheError::InvalidWorktreeRegistration {
            field,
            message: format!("path must be absolute: {}", path.display()),
        });
    }

    Ok(())
}

pub(super) fn normalized_path_key(path: &Path) -> PathBuf {
    // Existing paths compare by canonical identity, while missing paths remain
    // lexical because there is no stable filesystem identity to resolve yet.
    dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod garbage_collection_tests {
    use super::*;
    use crate::local_cache::test_support::*;
    #[test]
    fn garbage_collect_preserves_objects_when_a_registered_worktree_is_unavailable() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let missing_repo = temp.path().join("missing-repo");
        let referenced_bytes = b"referenced cache object";
        let unreferenced_bytes = b"unreferenced cache object";
        let referenced = object_for_bytes(referenced_bytes);
        let unreferenced = object_for_bytes(unreferenced_bytes);
        let active_registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("active registration should validate");
        let missing_registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/missing",
            &missing_repo,
            missing_repo.join(".git"),
        )
        .expect("missing registration should validate");

        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(&layout.object_path(&referenced), referenced_bytes);
        write_file(&layout.object_path(&unreferenced), unreferenced_bytes);
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(referenced.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        git_add(
            &repo,
            &[Path::new(".gitattributes"), Path::new("asset/model.bin")],
        );
        layout
            .register_worktree(active_registration.clone())
            .expect("active worktree should register");
        layout
            .register_worktree(missing_registration.clone())
            .expect("missing worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.active_worktree_count, 1);
        assert_eq!(
            report.unavailable_worktrees,
            vec![missing_registration.clone()]
        );
        assert!(report.pruned_worktrees.is_empty());
        assert_eq!(report.retained_objects.len(), 1);
        assert_eq!(report.retained_objects[0].oid, referenced.oid);
        assert_eq!(report.protected_objects.len(), 1);
        assert_eq!(report.protected_objects[0].oid, unreferenced.oid);
        assert!(report.unreferenced_objects.is_empty());
        assert!(report.deleted_objects.is_empty());
        assert!(layout.object_path(&referenced).exists());
        assert!(layout.object_path(&unreferenced).exists());
        let registry = layout
            .load_worktree_registry()
            .expect("registry should reload");
        assert_eq!(registry.worktrees().len(), 2);
        assert!(registry.worktrees().contains(&active_registration));
        assert!(registry.worktrees().contains(&missing_registration));
    }

    #[test]
    fn garbage_collect_uses_only_tracked_lfs_paths_for_reachability() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let tracked_lfs = object_for_bytes(b"tracked LFS object");
        let tracked_non_lfs = object_for_bytes(b"tracked non-LFS object");
        let ignored = object_for_bytes(b"ignored generated object");
        let empty = object_for_bytes(b"");
        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(&repo.join(".gitignore"), b"generated/\n");
        write_file(
            &repo.join("asset/keep.bin"),
            LfsPointer::new(tracked_lfs.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("docs/pointer.txt"),
            LfsPointer::new(tracked_non_lfs.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("generated/pointer.bin"),
            LfsPointer::new(ignored.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(&repo.join("asset/empty.bin"), b"");
        git_add(
            &repo,
            &[
                Path::new(".gitattributes"),
                Path::new(".gitignore"),
                Path::new("asset/empty.bin"),
                Path::new("asset/keep.bin"),
                Path::new("docs/pointer.txt"),
            ],
        );
        write_file(&layout.object_path(&tracked_lfs), b"tracked LFS object");
        write_file(
            &layout.object_path(&tracked_non_lfs),
            b"tracked non-LFS object",
        );
        write_file(&layout.object_path(&ignored), b"ignored generated object");
        write_file(&layout.object_path(&empty), b"");
        let registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("registration should validate");
        layout
            .register_worktree(registration)
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(
            report
                .retained_objects
                .iter()
                .map(|object| object.oid.clone())
                .collect::<Vec<_>>(),
            vec![tracked_lfs.oid.clone()]
        );
        assert_eq!(report.deleted_objects.len(), 3);
        assert!(layout.object_path(&tracked_lfs).exists());
        assert!(!layout.object_path(&tracked_non_lfs).exists());
        assert!(!layout.object_path(&ignored).exists());
        assert!(!layout.object_path(&empty).exists());
    }

    #[test]
    fn garbage_collect_handles_nul_delimited_tracked_lfs_paths() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let object = object_for_bytes(b"newline path object");
        #[cfg(unix)]
        let relative_path = Path::new("asset/line\nbreak.bin");
        #[cfg(windows)]
        let relative_path = Path::new("asset/space path.bin");
        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join(relative_path),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        git_add(&repo, &[Path::new(".gitattributes"), relative_path]);
        write_file(&layout.object_path(&object), b"newline path object");
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

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.retained_objects.len(), 1);
        assert_eq!(report.retained_objects[0].oid, object.oid);
        assert!(layout.object_path(&object).exists());
    }

    #[cfg(unix)]
    #[test]
    fn pointer_collection_does_not_follow_a_final_symlink() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let object = object_for_bytes(b"outside pointer object");
        let outside_pointer = temp.path().join("outside-pointer");
        let tracked_path = temp.path().join("tracked.bin");
        write_file(
            &outside_pointer,
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        std::os::unix::fs::symlink(&outside_pointer, &tracked_path)
            .expect("tracked symlink should be created");
        let mut referenced = BTreeSet::new();

        let skipped = collect_pointer_oid_from_file(&tracked_path, &mut referenced)
            .expect("a final symlink should be skipped");

        assert!(skipped);
        assert!(referenced.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn pointer_collection_does_not_block_when_file_becomes_fifo_before_open() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let candidate = temp.path().join("candidate.bin");
        write_file(&candidate, b"regular before pointer open");
        let worker_candidate = candidate.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            let mut referenced = BTreeSet::new();
            let result = collect_pointer_oid_from_file_with_before_open(
                &worker_candidate,
                &mut referenced,
                || {
                    fs::remove_file(&worker_candidate)
                        .expect("regular candidate should be removable");
                    assert!(
                        Command::new("mkfifo")
                            .arg(&worker_candidate)
                            .status()
                            .expect("mkfifo should start")
                            .success(),
                        "FIFO should replace the inspected regular file"
                    );
                },
            );
            done_tx
                .send(result.map(|skipped| (skipped, referenced)))
                .expect("test should receive collection result");
        });

        let result = match done_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _unblocker = rustix::fs::openat(
                    rustix::fs::CWD,
                    &candidate,
                    rustix::fs::OFlags::RDWR
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .map(File::from)
                .expect("FIFO should open for test cleanup");
                let _ = done_rx.recv_timeout(Duration::from_secs(2));
                panic!("pointer collection blocked while opening a FIFO");
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("pointer collection worker disconnected")
            }
        };
        worker
            .join()
            .expect("pointer collection worker should not panic");

        let (skipped, referenced) = result.expect("FIFO candidate should be skipped");
        assert!(skipped);
        assert!(referenced.is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn garbage_collect_does_not_wait_for_tracked_fifos() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let direct_fifo = repo.join("asset/direct.bin");
        let symlink_fifo = repo.join("asset/symlink.bin");
        let symlink_target = temp.path().join("symlink-target");
        let object = object_for_bytes(b"unreferenced FIFO cache object");

        initialize_git_worktree(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(&direct_fifo, b"tracked before becoming a FIFO");
        write_file(&symlink_fifo, b"tracked before becoming a FIFO symlink");
        git_add(
            &repo,
            &[
                Path::new(".gitattributes"),
                Path::new("asset/direct.bin"),
                Path::new("asset/symlink.bin"),
            ],
        );
        fs::remove_file(&direct_fifo).expect("tracked file should be removable");
        fs::remove_file(&symlink_fifo).expect("tracked file should be removable");
        assert!(
            Command::new("mkfifo")
                .arg(&direct_fifo)
                .status()
                .expect("mkfifo should start")
                .success(),
            "direct FIFO should be created"
        );
        assert!(
            Command::new("mkfifo")
                .arg(&symlink_target)
                .status()
                .expect("mkfifo should start")
                .success(),
            "symlink target FIFO should be created"
        );
        std::os::unix::fs::symlink(&symlink_target, &symlink_fifo)
            .expect("tracked FIFO symlink should be created");
        write_file(
            &layout.object_path(&object),
            b"unreferenced FIFO cache object",
        );
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

        let collection_layout = layout.clone();
        let (collection_done_tx, collection_done_rx) = mpsc::channel();
        let collection = thread::spawn(move || {
            collection_done_tx
                .send(collection_layout.garbage_collect(false, false))
                .expect("test should receive collection result");
        });
        let (timed_out, report) = match collection_done_rx.recv_timeout(Duration::from_secs(30)) {
            Ok(report) => (false, report),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                let _direct_unblocker = rustix::fs::openat(
                    rustix::fs::CWD,
                    &direct_fifo,
                    rustix::fs::OFlags::RDWR
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .map(File::from)
                .expect("direct FIFO should open for test cleanup");
                let _symlink_unblocker = rustix::fs::openat(
                    rustix::fs::CWD,
                    &symlink_target,
                    rustix::fs::OFlags::RDWR
                        | rustix::fs::OFlags::CLOEXEC
                        | rustix::fs::OFlags::NONBLOCK,
                    rustix::fs::Mode::empty(),
                )
                .map(File::from)
                .expect("symlink target FIFO should open for test cleanup");
                let report = collection_done_rx
                    .recv_timeout(Duration::from_secs(2))
                    .expect("garbage collection should finish after test cleanup");
                (true, report)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("garbage collection worker disconnected")
            }
        };
        collection
            .join()
            .expect("garbage collection worker should not panic");

        assert!(
            !timed_out,
            "garbage collection must not wait for FIFO writers"
        );
        let report = report.expect("garbage collection should succeed");
        assert_eq!(report.unreferenced_objects.len(), 1);
        assert_eq!(report.unreferenced_objects[0].oid, object.oid);
        assert_eq!(
            report.skipped_worktree_pointer_paths,
            vec![direct_fifo, symlink_fifo]
        );
        assert!(!layout.object_path(&object).exists());
    }

    #[test]
    fn garbage_collect_prunes_unavailable_worktree_only_when_explicitly_requested() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let replaced_repo = temp.path().join("replaced-repo");
        let bytes = b"file-replaced worktree cache object";
        let object = object_for_bytes(bytes);
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/replaced",
            &replaced_repo,
            temp.path().join("replaced-repo/.git"),
        )
        .expect("registration should validate");

        write_file(&layout.object_path(&object), bytes);
        write_file(&replaced_repo, b"not a directory");
        layout
            .register_worktree(registration.clone())
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, true)
            .expect("file-replaced worktree should prune");

        assert_eq!(report.active_worktree_count, 0);
        assert_eq!(report.unavailable_worktrees, vec![registration.clone()]);
        assert_eq!(report.pruned_worktrees, vec![registration]);
        assert!(report.protected_objects.is_empty());
        assert_eq!(report.unreferenced_objects.len(), 1);
        assert!(!layout.object_path(&object).exists());
        assert!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .is_empty()
        );
    }

    #[test]
    fn garbage_collect_ignores_untracked_pointer_in_git_metadata() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let bytes = b"git file referenced cache object";
        let object = object_for_bytes(bytes);
        let registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("active registration should validate");

        initialize_git_worktree(&repo);
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &repo.join(".git/lfscloud-pointer"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        layout
            .register_worktree(registration)
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.active_worktree_count, 1);
        assert_eq!(report.unreferenced_objects.len(), 1);
        assert_eq!(report.unreferenced_objects[0].oid, object.oid);
        assert!(!layout.object_path(&object).exists());
    }

    #[cfg(unix)]
    #[test]
    fn garbage_collect_ignores_symlinked_directory_when_collecting_pointers() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside");
        let bytes = b"symlinked pointer cache object";
        let object = object_for_bytes(bytes);
        let registration =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", &repo, repo.join(".git"))
                .expect("active registration should validate");

        initialize_git_worktree(&repo);
        fs::create_dir_all(&outside).expect("outside directory should be created");
        std::os::unix::fs::symlink(&outside, repo.join("linked"))
            .expect("directory symlink should be created");
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &outside.join("model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        layout
            .register_worktree(registration)
            .expect("worktree should register");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should finish");

        assert_eq!(report.unreferenced_objects.len(), 1);
        assert_eq!(report.unreferenced_objects[0].oid, object.oid);
        assert!(!layout.object_path(&object).exists());
    }

    #[test]
    fn garbage_collect_dry_run_leaves_cache_objects_and_registry_untouched() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let missing_repo = temp.path().join("missing-repo");
        let bytes = b"dry run unreferenced cache object";
        let object = object_for_bytes(bytes);
        let missing_registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/missing",
            &missing_repo,
            missing_repo.join(".git"),
        )
        .expect("missing registration should validate");

        write_file(&layout.object_path(&object), bytes);
        layout
            .register_worktree(missing_registration.clone())
            .expect("missing worktree should register");

        let report = layout
            .garbage_collect(true, false)
            .expect("dry-run garbage collection should finish");

        assert!(report.dry_run);
        assert_eq!(report.active_worktree_count, 0);
        assert_eq!(
            report.unavailable_worktrees,
            vec![missing_registration.clone()]
        );
        assert!(report.pruned_worktrees.is_empty());
        assert_eq!(report.protected_objects.len(), 1);
        assert!(report.unreferenced_objects.is_empty());
        assert!(report.deleted_objects.is_empty());
        assert!(layout.object_path(&object).exists());
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            &[missing_registration]
        );
    }

    #[test]
    fn garbage_collect_reports_invalid_cache_paths_without_deleting_them() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let invalid_cache_path = layout.objects_dir().join("zz/zz/not-a-sha256");
        write_file(&invalid_cache_path, b"invalid cache payload");

        let report = layout
            .garbage_collect(false, false)
            .expect("garbage collection should skip invalid paths");

        assert!(report.retained_objects.is_empty());
        assert!(report.unreferenced_objects.is_empty());
        assert_eq!(report.skipped_cache_paths, vec![invalid_cache_path.clone()]);
        assert!(invalid_cache_path.exists());
    }
}
