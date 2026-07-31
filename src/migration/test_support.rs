// Shared fixtures for the focused test modules.

#[cfg(unix)]
pub(super) use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
pub(super) use std::{
    collections::BTreeSet,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

pub(super) use tempfile::TempDir;

pub(super) use sha2::{Digest, Sha256};

pub(super) use crate::{
    LfsObject, LfsObjectSize, LfsOid, LfsPointer, LocalCacheLayout, ProviderFuture,
    StorageDeleteOutcome, StorageError, StorageProvider, StorageResult, StoredObject,
};

pub(super) use super::configure_process_tree;
pub(super) use super::{
    GitLfsSourceEndpointSource, LocalMigrationObject, LocalMigrationObjectLocation,
    LocalMigrationObjectLocationKind, LocalMigrationObjectLocationStatus, MAX_GIT_ATTRIBUTES_BYTES,
    MAX_MIGRATION_GIT_OUTPUT_BYTES, MigrationError, MigrationFetchMode,
    MigrationObjectUploadStatus, MigrationStorageUploadOptions, check_local_migration_objects,
    default_lfs_endpoint_for_remote_url, discover_git_lfs_migration,
    discover_git_lfs_migration_from_remote, display_git_command,
    enumerate_all_fetched_ref_lfs_pointers, enumerate_current_checkout_lfs_pointers,
    enumerate_fetched_ref_lfs_pointers_for_remote, enumerate_selected_ref_lfs_pointers,
    enumerate_selected_ref_lfs_pointers_with_metrics, fetch_migration_git_refs,
    fetch_missing_migration_objects, fetch_missing_migration_objects_with_runner,
    git_lfs_object_path, hash_migration_object_file, migration_source_fetch_command,
    parse_git_check_attr_filter_stdout, parse_lfs_patterns_from_attributes,
    repo_relative_path_from_git_output, split_gitattributes_line,
    upload_migration_objects_to_storage, upload_migration_objects_to_storage_with_options,
    validate_historical_scan_git_version, validate_history_ref_name,
    verified_migration_upload_source_path, wait_for_git_command,
};

pub(super) const TEST_REPOSITORY_NAMESPACE: &str = "github-main:owner/repo";

pub(super) struct TempRepo {
    root: TempDir,
}

impl TempRepo {
    pub(super) fn new() -> Self {
        let root = tempfile::tempdir().expect("temporary repository directory should be created");
        let repo = Self { root };
        repo.git(["init", "--initial-branch", "main"]);
        repo.git(["config", "user.email", "lfscloud@example.invalid"]);
        repo.git(["config", "user.name", "LFS Cloud Test"]);
        repo
    }

    pub(super) fn path(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    pub(super) fn write_file(&self, relative_path: impl AsRef<Path>, contents: &str) {
        self.write_bytes(relative_path, contents.as_bytes());
    }

    pub(super) fn write_bytes(&self, relative_path: impl AsRef<Path>, contents: &[u8]) {
        let path = self.root.path().join(relative_path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("test file parent should be created");
        }
        fs::write(path, contents).expect("test file should be written");
    }

    pub(super) fn commit_all(&self, message: &str) {
        self.git(["add", "-A"]);
        self.git(["commit", "-m", message]);
    }

    pub(super) fn git<const N: usize>(&self, args: [&str; N]) {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root.path())
            .output()
            .expect("git command should start");

        assert!(
            output.status.success(),
            "git command failed: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    pub(super) fn git_stdout<const N: usize>(&self, args: [&str; N]) -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(self.root.path())
            .output()
            .expect("git command should start");

        assert!(
            output.status.success(),
            "git command failed: {}\nstderr: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );

        String::from_utf8(output.stdout)
            .expect("git stdout should be UTF-8")
            .trim()
            .to_owned()
    }

    pub(super) fn mark_head_as_shallow_boundary(&self) {
        let head = self.git_stdout(["rev-parse", "HEAD"]);
        fs::write(self.root.path().join(".git/shallow"), format!("{head}\n"))
            .expect("shallow boundary should be written");
    }
}

pub(super) fn test_lfs_object(hex_digit: char, size: u64) -> LfsObject {
    let oid = hex_digit.to_string().repeat(64);
    LfsObject::new(
        LfsOid::new(oid).expect("test OID should be valid"),
        LfsObjectSize::new(size),
    )
}

pub(super) fn test_lfs_object_from_bytes(bytes: &[u8]) -> LfsObject {
    let oid = format!("{:x}", Sha256::digest(bytes));
    LfsObject::new(
        LfsOid::new(oid).expect("test OID should be valid"),
        LfsObjectSize::new(bytes.len() as u64),
    )
}

pub(super) fn history_scan_objects(
    pointers: &[super::GitLfsHistoryPointer],
) -> BTreeSet<LfsObject> {
    pointers
        .iter()
        .map(|pointer| pointer.object.clone())
        .collect()
}

pub(super) fn write_git_lfs_source_object(repo: &TempRepo, object: &LfsObject, contents: &[u8]) {
    write_git_lfs_source_object_in(&repo.path().join(".git/lfs/objects"), object, contents);
}

pub(super) fn write_git_lfs_source_object_in(
    git_lfs_objects_dir: &Path,
    object: &LfsObject,
    contents: &[u8],
) {
    write_file(
        &git_lfs_object_path(git_lfs_objects_dir, &object.oid)
            .expect("test object path should be valid"),
        contents,
    );
}

pub(super) fn write_file(path: &Path, contents: &[u8]) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("test file parent should be created");
    }
    fs::write(path, contents).expect("test file should be written");
}

pub(super) fn require_git_lfs() {
    let output = Command::new("git")
        .args(["lfs", "version"])
        .output()
        .expect("Git LFS is required to run the manual migration integration test");
    assert!(
        output.status.success(),
        "Git LFS is required to run the manual migration integration test: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

pub(super) fn assert_git_status_clean(worktree_root: &Path) {
    let output = Command::new("git")
        .args(["status", "--short"])
        .current_dir(worktree_root)
        .output()
        .expect("git status should start");

    assert!(
        output.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stdout.is_empty(),
        "worktree should stay clean after migration source fetch: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
