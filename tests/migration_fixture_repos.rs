//! Fixture-repository migration coverage.
//!
//! These tests exercise migration behavior through public APIs and the built
//! CLI against real temporary Git repositories. They intentionally avoid a
//! `git-lfs` dependency by committing canonical pointer files directly and
//! letting Git attribute checks decide which files count as LFS-owned.

mod support;

use std::{collections::BTreeSet, fs, path::Path, process::Command};

use lfs_cloud::{
    LfsObject, check_local_migration_objects, enumerate_all_fetched_ref_lfs_pointers,
    enumerate_current_checkout_lfs_pointers, enumerate_selected_ref_lfs_pointers,
};
use support::{TempGitRepo, lfs_object_for_bytes, write_lfs_pointer};

// Deliberately inert: dry-run tests pass this through config and CLI parsing,
// but must not make network requests to it.
const SERVER_URL: &str = "http://127.0.0.1:9";

#[test]
fn fixture_repo_current_checkout_scan_uses_git_attributes() {
    let repo = initialized_migration_repo();
    let lfs_object = lfs_object_for_bytes(b"current checkout lfs object");
    let non_lfs_object = lfs_object_for_bytes(b"pointer-shaped docs fixture");

    repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n*.txt text\n");
    write_lfs_pointer(
        &repo,
        "asset/model.bin",
        lfs_object.oid.as_hex(),
        lfs_object.size.bytes(),
    );
    write_lfs_pointer(
        &repo,
        "docs/pointer-example.txt",
        non_lfs_object.oid.as_hex(),
        non_lfs_object.size.bytes(),
    );
    repo.commit_all("add current checkout fixture pointers");

    let scan = enumerate_current_checkout_lfs_pointers(repo.path())
        .expect("current checkout fixture scan should succeed");

    assert_eq!(scan.tracked_path_count, 1);
    assert_eq!(scan.pointers.len(), 1);
    assert_eq!(scan.pointers[0].relative_path, Path::new("asset/model.bin"));
    assert_eq!(scan.pointers[0].object, lfs_object);
}

#[test]
fn fixture_repo_current_checkout_scan_reads_hydrated_pointer_from_index() {
    let repo = initialized_migration_repo();
    let object_bytes = b"hydrated current checkout object";
    let object = lfs_object_for_bytes(object_bytes);

    repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
    write_lfs_pointer(
        &repo,
        "asset/model.bin",
        object.oid.as_hex(),
        object.size.bytes(),
    );
    repo.commit_all("add pointer before hydration");
    fs::write(repo.path().join("asset/model.bin"), object_bytes)
        .expect("fixture pointer should hydrate in the worktree");

    let scan = enumerate_current_checkout_lfs_pointers(repo.path())
        .expect("hydrated current checkout fixture scan should succeed");

    assert_eq!(scan.tracked_path_count, 1);
    assert_eq!(scan.pointers.len(), 1);
    assert_eq!(scan.pointers[0].relative_path, Path::new("asset/model.bin"));
    assert_eq!(scan.pointers[0].object, object);
}

#[test]
fn fixture_repo_current_checkout_scan_reads_sparse_pointer_from_index() {
    let repo = initialized_migration_repo();
    let object = lfs_object_for_bytes(b"sparse current checkout object");

    repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
    write_lfs_pointer(
        &repo,
        "asset/model.bin",
        object.oid.as_hex(),
        object.size.bytes(),
    );
    repo.write_file("docs/README.md", "sparse checkout fixture\n");
    repo.commit_all("add pointer before sparse checkout");
    repo.git(["sparse-checkout", "init", "--cone"]);
    repo.git(["sparse-checkout", "set", "docs"]);
    assert!(
        !repo.path().join("asset/model.bin").exists(),
        "fixture should omit the LFS path from the sparse worktree"
    );

    let scan = enumerate_current_checkout_lfs_pointers(repo.path())
        .expect("sparse current checkout fixture scan should succeed");

    assert_eq!(scan.tracked_path_count, 1);
    assert_eq!(scan.pointers.len(), 1);
    assert_eq!(scan.pointers[0].relative_path, Path::new("asset/model.bin"));
    assert_eq!(scan.pointers[0].object, object);
}

#[test]
fn fixture_repo_selected_ref_scan_walks_branch_history() {
    let repo = initialized_migration_repo();
    let main_object = lfs_object_for_bytes(b"main history lfs object");
    let branch_object = lfs_object_for_bytes(b"branch-only lfs object");

    repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
    write_lfs_pointer(
        &repo,
        "asset/main.bin",
        main_object.oid.as_hex(),
        main_object.size.bytes(),
    );
    repo.commit_all("add main pointer");
    repo.git(["checkout", "-b", "feature/assets"]);
    write_lfs_pointer(
        &repo,
        "asset/branch.bin",
        branch_object.oid.as_hex(),
        branch_object.size.bytes(),
    );
    repo.commit_all("add branch pointer");
    repo.git(["checkout", "main"]);

    let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["feature/assets"])
        .expect("selected ref fixture scan should succeed");
    let objects = history_objects(&scan.pointers);

    assert_eq!(scan.refs.len(), 1);
    assert_eq!(scan.refs[0].name, "feature/assets");
    assert!(objects.contains(&main_object));
    assert!(objects.contains(&branch_object));
}

#[test]
fn fixture_repo_all_refs_scan_includes_branches_and_tags() {
    let repo = initialized_migration_repo();
    let main_object = lfs_object_for_bytes(b"all refs main object");
    let branch_object = lfs_object_for_bytes(b"all refs branch object");

    repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
    write_lfs_pointer(
        &repo,
        "asset/main.bin",
        main_object.oid.as_hex(),
        main_object.size.bytes(),
    );
    repo.commit_all("add all-ref main pointer");
    repo.git(["tag", "v-main"]);
    repo.git(["checkout", "-b", "feature/assets"]);
    write_lfs_pointer(
        &repo,
        "asset/branch.bin",
        branch_object.oid.as_hex(),
        branch_object.size.bytes(),
    );
    repo.commit_all("add all-ref branch pointer");
    repo.git(["checkout", "main"]);

    let scan = enumerate_all_fetched_ref_lfs_pointers(repo.path())
        .expect("all refs fixture scan should succeed");
    let ref_names = scan
        .refs
        .iter()
        .map(|scanned_ref| scanned_ref.name.as_str())
        .collect::<BTreeSet<_>>();
    let objects = history_objects(&scan.pointers);

    assert!(ref_names.contains("refs/heads/main"));
    assert!(ref_names.contains("refs/heads/feature/assets"));
    assert!(ref_names.contains("refs/tags/v-main"));
    assert!(objects.contains(&main_object));
    assert!(objects.contains(&branch_object));
}

#[test]
fn fixture_repo_missing_object_availability_reports_fetch_candidates() {
    let repo = initialized_migration_repo();
    let available = lfs_object_for_bytes(b"available migration bytes");
    let missing = lfs_object_for_bytes(b"missing migration bytes");
    write_git_lfs_media_object(&repo, &available, b"available migration bytes");

    let availability = check_local_migration_objects(repo.path(), [&available, &missing], None)
        .expect("fixture availability check should succeed");

    assert_eq!(availability.available_objects().len(), 1);
    assert_eq!(availability.unavailable_objects().len(), 1);
    assert_eq!(availability.available_objects()[0].object, available);
    assert_eq!(availability.unavailable_objects()[0].object, missing);
}

#[test]
fn fixture_repo_migrate_dry_run_leaves_repo_and_cache_untouched() {
    let temp = tempfile::tempdir().expect("temporary migration fixture should be created");
    let repo = initialized_migration_repo();
    let cache_root = temp.path().join("cache");
    let config_path = temp.path().join("lfs-cloud.yml");
    let object = lfs_object_for_bytes(b"dry-run fixture object");

    repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
    write_lfs_pointer(
        &repo,
        "asset/model.bin",
        object.oid.as_hex(),
        object.size.bytes(),
    );
    repo.commit_all("add dry-run pointer");
    fs::write(&config_path, server_config(SERVER_URL)).expect("fixture config should be written");
    let before_status = git_status(repo.path());

    let output = Command::new(env!("CARGO_BIN_EXE_lfs-cloud"))
        .args([
            "--config",
            config_path
                .to_str()
                .expect("fixture config path should be valid UTF-8"),
            "migrate",
            "--server",
            SERVER_URL,
            "--cache-root",
            cache_root
                .to_str()
                .expect("fixture cache path should be valid UTF-8"),
            "--dry-run",
        ])
        .current_dir(repo.path())
        .output()
        .expect("lfs-cloud migrate dry-run should start");

    assert!(
        output.status.success(),
        "migrate dry-run failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("dry-run output should be UTF-8");
    assert!(stdout.contains("lfs-cloud migrate dry-run"));
    assert!(stdout.contains("mode: current-checkout"));
    assert!(stdout.contains("objects fetched:"));
    assert!(stdout.contains("1 would fetch"));
    assert!(stdout.contains("0 already local"));
    assert_eq!(git_status(repo.path()), before_status);
    assert!(
        !repo.path().join(".lfsconfig").exists(),
        "dry-run must not write worktree LFS config"
    );
    assert!(
        !cache_root.exists(),
        "dry-run must not create local cache state"
    );
}

fn initialized_migration_repo() -> TempGitRepo {
    let repo = TempGitRepo::new();
    repo.git(["remote", "add", "origin", "git@github.com:owner/repo.git"]);
    repo
}

fn history_objects(pointers: &[lfs_cloud::GitLfsHistoryPointer]) -> BTreeSet<LfsObject> {
    pointers
        .iter()
        .map(|pointer| pointer.object.clone())
        .collect()
}

fn write_git_lfs_media_object(repo: &TempGitRepo, object: &LfsObject, contents: &[u8]) {
    assert_eq!(
        object.size.bytes(),
        contents.len() as u64,
        "fixture media contents must match the declared LFS object size"
    );
    let oid = object.oid.as_hex();
    let first_shard = oid
        .get(..2)
        .expect("validated SHA-256 object IDs always have a first shard");
    let second_shard = oid
        .get(2..4)
        .expect("validated SHA-256 object IDs always have a second shard");
    let path = repo
        .path()
        .join(".git")
        .join("lfs")
        .join("objects")
        .join(first_shard)
        .join(second_shard)
        .join(oid);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("Git LFS media object parent should be created");
    }
    fs::write(path, contents).expect("Git LFS media object should be written");
}

fn git_status(repo: &Path) -> String {
    let output = Command::new("git")
        .args(["status", "--porcelain=v1"])
        .current_dir(repo)
        .output()
        .expect("git status should start");

    assert!(
        output.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("git status should be UTF-8")
}

fn server_config(public_url: &str) -> String {
    let public_url = yaml_single_quoted(public_url);
    format!(
        r#"
server:
  host: 127.0.0.1
  port: 8080
  public_url: {public_url}

repository_providers:
  github-main:
    type: github
    api_url: https://api.github.com
    oauth_client_id: client-id
    oauth_client_secret: client-secret

storage_providers:
  drive-user-a:
    type: google_drive
    credentials_ref: drive-user-a
    root_folder_id: root-folder

repositories:
  - id: github-main:owner/repo
    repo_provider: github-main
    host: github.com
    owner: owner
    name: repo
    provider_repository_id: "8675309"
    storage_provider: drive-user-a
        "#
    )
}

fn yaml_single_quoted(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}
