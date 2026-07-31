// This file is included by `mod.rs` so the migration API remains in one module.

/// Git LFS pointers discovered from the current checkout's index.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CurrentCheckoutLfsPointers {
    /// Git worktree root that was inspected.
    pub worktree_root: PathBuf,
    /// Number of tracked index paths whose Git attributes use `filter=lfs`.
    pub tracked_path_count: usize,
    /// Pointer blobs found among the current index's LFS-tracked paths.
    pub pointers: Vec<CurrentCheckoutLfsPointer>,
}

/// A Git LFS pointer blob found in the current checkout's index.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct CurrentCheckoutLfsPointer {
    /// Repository-relative path to the pointer file.
    pub relative_path: PathBuf,
    /// Corresponding absolute worktree path, which may be absent in a sparse checkout.
    pub path: PathBuf,
    /// Object identity referenced by the pointer file.
    pub object: LfsObject,
}

/// Enumerates Git LFS pointer blobs in the current checkout's index.
///
/// This function is intentionally read-only. It asks Git which index paths have
/// `filter=lfs`, then parses small pointer-shaped blobs directly from the index.
/// The index remains authoritative when a worktree file is hydrated or omitted
/// by sparse checkout, so both states retain complete current-checkout coverage.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// Git cannot list tracked files or attributes, or Git returns unsafe path data.
pub fn enumerate_current_checkout_lfs_pointers(
    start_dir: impl AsRef<Path>,
) -> MigrationResult<CurrentCheckoutLfsPointers> {
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    let lfs_tracked_blobs = current_checkout_lfs_tracked_blobs(&worktree_root)?;
    let mut pointers = Vec::new();

    for blob in &lfs_tracked_blobs {
        let Some(pointer) = read_index_pointer_blob_candidate(&worktree_root, &blob.object_id)?
        else {
            continue;
        };

        pointers.push(CurrentCheckoutLfsPointer {
            relative_path: blob.relative_path.clone(),
            path: worktree_root.join(&blob.relative_path),
            object: pointer.object,
        });
    }

    Ok(CurrentCheckoutLfsPointers {
        worktree_root,
        tracked_path_count: lfs_tracked_blobs.len(),
        pointers,
    })
}
fn current_checkout_lfs_tracked_blobs(worktree_root: &Path) -> MigrationResult<Vec<GitIndexBlob>> {
    const COMMAND: &str = "git ls-files -z --cached --stage";
    let output = run_git_os_with_limit(
        worktree_root,
        [
            OsStr::new("ls-files"),
            OsStr::new("-z"),
            OsStr::new("--cached"),
            OsStr::new("--stage"),
        ],
        COMMAND,
        MAX_CURRENT_CHECKOUT_ATTR_OUTPUT_BYTES,
    )?;
    if !output.status.success() {
        return Err(command_error(COMMAND, output.status, &output.stderr));
    }
    if output.stdout.is_empty() {
        return Ok(Vec::new());
    }

    let index_blobs = parse_ls_files_stage_blob_output(&output.stdout, COMMAND)?;
    if index_blobs.is_empty() {
        return Ok(Vec::new());
    }
    let tracked_paths = index_blobs
        .iter()
        .flat_map(|blob| blob.relative_path_bytes.iter().copied().chain([b'\0']))
        .collect();
    let attributes = git_check_attr_filter(worktree_root, tracked_paths)?;
    let lfs_tracked_paths = parse_git_check_attr_filter_stdout(
        &attributes.stdout,
        &git_check_attr_filter_command_name(None),
    )?
    .into_iter()
    .collect::<BTreeSet<_>>();

    Ok(index_blobs
        .into_iter()
        .filter(|blob| lfs_tracked_paths.contains(&blob.relative_path))
        .collect())
}

fn parse_ls_files_stage_blob_output(
    stdout: &[u8],
    command_name: &str,
) -> MigrationResult<Vec<GitIndexBlob>> {
    let mut blobs = Vec::new();

    for record in stdout
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
    {
        let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
            return Err(index_entry_parse_error(command_name));
        };
        let metadata = &record[..separator];
        let fields = metadata
            .split(|byte| *byte == b' ')
            .filter(|field| !field.is_empty())
            .collect::<Vec<_>>();
        let [mode, object_id, stage] = fields.as_slice() else {
            return Err(index_entry_parse_error(command_name));
        };
        if *stage != b"0" {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("Git index contains an unmerged entry"),
            });
        }
        if !matches!(*mode, b"100644" | b"100755") {
            continue;
        }
        let object_id = std::str::from_utf8(object_id)
            .map_err(|_| index_entry_parse_error(command_name))?
            .to_owned();
        let relative_path_bytes = record[separator + 1..].to_owned();
        let relative_path = safe_git_relative_path(&relative_path_bytes, command_name)?;

        blobs.push(GitIndexBlob {
            object_id,
            relative_path,
            relative_path_bytes,
        });
    }

    Ok(blobs)
}

fn index_entry_parse_error(command_name: &str) -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git returned malformed index metadata"),
    }
}

fn read_index_pointer_blob_candidate(
    worktree_root: &Path,
    object_id: &str,
) -> MigrationResult<Option<LfsPointer>> {
    read_history_pointer_blob_candidate(worktree_root, object_id)
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitIndexBlob {
    object_id: String,
    relative_path: PathBuf,
    relative_path_bytes: Vec<u8>,
}


#[cfg(test)]
mod checkout_tests {
    use super::test_support::*;

    #[test]
    fn enumerates_current_checkout_lfs_pointer_files() {
        let repo = TempRepo::new();
        let pointer_object = test_lfs_object('a', 123);
        let non_lfs_pointer_object = test_lfs_object('c', 789);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n*.txt text\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(pointer_object.clone()).to_pointer_file(),
        );
        repo.write_file("asset/empty.bin", "");
        repo.write_file(
            "docs/pointer-example.txt",
            &LfsPointer::new(non_lfs_pointer_object).to_pointer_file(),
        );
        repo.git([
            "add",
            ".gitattributes",
            "asset/empty.bin",
            "asset/model.bin",
            "docs/pointer-example.txt",
        ]);

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should succeed");

        assert_eq!(scan.tracked_path_count, 2);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].relative_path, Path::new("asset/model.bin"));
        assert_eq!(
            scan.pointers[0]
                .path
                .canonicalize()
                .expect("discovered pointer path should canonicalize"),
            repo.path()
                .join("asset/model.bin")
                .canonicalize()
                .expect("expected pointer path should canonicalize")
        );
        assert_eq!(scan.pointers[0].object, pointer_object);
    }

    #[test]
    fn current_checkout_pointer_scan_ignores_untracked_lfs_files() {
        let repo = TempRepo::new();
        let tracked_object = test_lfs_object('a', 123);
        let untracked_object = test_lfs_object('b', 456);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/tracked.bin",
            &LfsPointer::new(tracked_object.clone()).to_pointer_file(),
        );
        repo.write_file(
            "asset/untracked.bin",
            &LfsPointer::new(untracked_object).to_pointer_file(),
        );
        repo.git(["add", ".gitattributes", "asset/tracked.bin"]);

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should succeed");

        assert_eq!(scan.tracked_path_count, 1);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(
            scan.pointers[0].relative_path,
            Path::new("asset/tracked.bin")
        );
        assert_eq!(scan.pointers[0].object, tracked_object);
    }

    #[test]
    fn current_checkout_pointer_scan_reads_missing_tracked_lfs_files_from_index() {
        let repo = TempRepo::new();
        let present_object = test_lfs_object('a', 123);
        let missing_object = test_lfs_object('b', 456);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/present.bin",
            &LfsPointer::new(present_object.clone()).to_pointer_file(),
        );
        repo.write_file(
            "asset/missing.bin",
            &LfsPointer::new(missing_object.clone()).to_pointer_file(),
        );
        repo.git([
            "add",
            ".gitattributes",
            "asset/present.bin",
            "asset/missing.bin",
        ]);
        fs::remove_file(repo.path().join("asset/missing.bin"))
            .expect("tracked checkout file should be removable");

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should succeed");

        assert_eq!(scan.tracked_path_count, 2);
        assert_eq!(scan.pointers.len(), 2);
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/present.bin")
                && pointer.object == present_object
        }));
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/missing.bin")
                && pointer.object == missing_object
        }));
    }

    #[test]
    fn current_checkout_pointer_scan_does_not_lazy_fetch_promisor_blobs() {
        let repo = TempRepo::new();
        let object = test_lfs_object('a', 123);
        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object).to_pointer_file(),
        );
        repo.commit_all("add migration pointer");

        let blob_id = repo.git_stdout(["rev-parse", ":asset/model.bin"]);
        let remote_parent = tempfile::tempdir()
            .expect("temporary promisor remote parent directory should be created");
        let remote_path = remote_parent.path().join("remote.git");
        let clone_output = Command::new("git")
            .args(["clone", "--bare"])
            .arg(repo.path())
            .arg(&remote_path)
            .output()
            .expect("promisor remote clone should start");
        assert!(
            clone_output.status.success(),
            "promisor remote clone failed: {}",
            String::from_utf8_lossy(&clone_output.stderr)
        );

        repo.git([
            "remote",
            "add",
            "origin",
            remote_path
                .to_str()
                .expect("temporary remote path should be UTF-8"),
        ]);
        repo.git(["config", "remote.origin.promisor", "true"]);
        repo.git(["config", "remote.origin.partialclonefilter", "blob:none"]);

        let local_blob_path = repo
            .path()
            .join(".git/objects")
            .join(&blob_id[..2])
            .join(&blob_id[2..]);
        fs::remove_file(&local_blob_path).expect("local pointer blob should be removable");

        let error = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect_err("missing promisor blob should remain unavailable during discovery");

        assert!(
            error.to_string().contains("unavailable locally"),
            "unexpected missing-promisor diagnostic: {error}"
        );
        assert!(
            !local_blob_path.exists(),
            "read-only migration discovery must not lazy-fetch the missing blob"
        );
    }

    #[cfg(unix)]
    #[test]
    fn current_checkout_pointer_scan_accepts_non_utf8_lfs_paths() {
        let repo = TempRepo::new();
        let object = test_lfs_object('d', 321);
        let relative_path = PathBuf::from(OsString::from_vec(b"asset/nonutf-\xFF.bin".to_vec()));
        let worktree_file = repo.path().join(&relative_path);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        fs::create_dir_all(worktree_file.parent().expect("path should have parent"))
            .expect("non-UTF-8 path parent should be created");
        if fs::write(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        )
        .is_err()
        {
            return;
        }
        repo.git(["add", "-A"]);

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current checkout pointer scan should accept non-UTF-8 paths");

        assert_eq!(scan.tracked_path_count, 1);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].relative_path, relative_path);
        assert_eq!(scan.pointers[0].object, object);
    }

    #[test]
    fn current_checkout_pointer_scan_accepts_large_attribute_output() {
        let mut stdout = Vec::new();
        for index in 0..8_000 {
            stdout.extend_from_slice(format!("docs/file-{index:05}.txt").as_bytes());
            stdout.extend_from_slice(b"\0filter\0unspecified\0");
        }
        stdout.extend_from_slice(b"asset/model.bin\0filter\0lfs\0");
        assert!(stdout.len() > MAX_MIGRATION_GIT_OUTPUT_BYTES);

        let paths = parse_git_check_attr_filter_stdout(&stdout, "git check-attr test")
            .expect("large check-attr output should not fail before parsing");

        assert_eq!(paths, vec![PathBuf::from("asset/model.bin")]);
    }

    #[test]
    fn rejects_malformed_check_attr_output() {
        assert!(parse_git_check_attr_filter_stdout(b"asset/model.bin\0filter", "test").is_err());
        assert!(parse_git_check_attr_filter_stdout(b"\0filter\0lfs\0", "test").is_err());
    }

    #[test]
    fn malformed_check_attr_output_reports_supplied_command() {
        let error = parse_git_check_attr_filter_stdout(
            b"asset/model.bin\0filter",
            "git check-attr -z --stdin --source=abc123 filter",
        )
        .expect_err("malformed attribute output should fail");

        assert!(matches!(
            error,
            MigrationError::ExternalCommandOutput { command, .. }
                if command == "git check-attr -z --stdin --source=abc123 filter"
        ));
    }

    #[test]
    fn rejects_check_attr_paths_outside_worktree() {
        assert!(
            parse_git_check_attr_filter_stdout(b"/tmp/model.bin\0filter\0lfs\0", "test").is_err()
        );
        assert!(
            parse_git_check_attr_filter_stdout(b"../model.bin\0filter\0lfs\0", "test").is_err()
        );
        assert!(
            parse_git_check_attr_filter_stdout(b"asset/model.bin\0filter\0lfs\0", "test").is_ok()
        );
    }

    #[test]
    fn current_checkout_pointer_scan_accepts_shallow_repository() {
        let repo = TempRepo::new();
        let object = test_lfs_object('c', 321);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add current pointer at shallow boundary");
        repo.mark_head_as_shallow_boundary();

        let scan = enumerate_current_checkout_lfs_pointers(repo.path())
            .expect("current-checkout inventory does not require repository history");

        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].object, object);
    }

}
