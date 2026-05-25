//! Local end-to-end coverage for the fake-provider MVP path.
//!
//! This test keeps real GitHub, Google Drive, and Git LFS binaries out of the
//! loop while still exercising the repository-init, provider authorization,
//! object transfer, and checkout-materialization boundaries together.

mod support;

use std::fs;

use lfs_cloud::{
    GitLfsConfigTarget, GitRepository, LfsInitRoute, LocalCacheLayout, RepositoryHandle,
    RepositoryPermission, RepositoryProvider, RepositoryUser, StorageProvider,
};
use support::{
    FakeRepositoryProvider, FakeStorageProvider, TempGitRepo, lfs_object_for_bytes,
    lfs_pointer_file,
};

#[tokio::test]
async fn local_init_upload_download_and_checkout_flow_uses_fake_providers() {
    let repo = TempGitRepo::new();
    repo.git([
        "remote",
        "add",
        "origin",
        "https://github.com/owner/repo.git",
    ]);
    repo.write_file(
        ".gitattributes",
        "*.bin filter=lfs diff=lfs merge=lfs -text\n",
    );

    let repository = GitRepository::discover(repo.path()).expect("repository should be discovered");
    let route = LfsInitRoute::resolve("http://127.0.0.1:8080", &repository.remote)
        .expect("init route should resolve");
    let init_change = repository
        .write_lfs_url(GitLfsConfigTarget::WorktreeFile, &route.lfs_url)
        .expect("init should write .lfsconfig");

    assert_eq!(init_change.previous_url, None);
    assert_eq!(
        repo.git(["config", "--file", ".lfsconfig", "--get", "lfs.url"])
            .stdout,
        b"http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs\n"
    );

    let github = FakeRepositoryProvider::new("github-main");
    github.add_repository("github.com", "owner", "repo", Some("repo-123".to_owned()));
    github.grant_permission(
        "github.com",
        "owner",
        "repo",
        "octocat",
        RepositoryPermission::Write,
    );
    let handle = RepositoryHandle::new("github-main", "github.com", "owner", "repo");
    let identity = github
        .repository_identity(&handle)
        .await
        .expect("fake GitHub should resolve the repository");
    let user = RepositoryUser::new("github-main", "octocat", Some("user-123".to_owned()));

    github
        .check_permission(&identity, &user, RepositoryPermission::Write)
        .await
        .expect("fake GitHub should authorize object upload");

    let bytes = b"large model bytes fetched through lfs-cloud";
    let object = lfs_object_for_bytes(bytes);
    let staging = tempfile::tempdir().expect("staging tempdir should be created");
    let staged_object = staging.path().join("model.bin");
    fs::write(&staged_object, bytes).expect("staged object should be written");
    let drive = FakeStorageProvider::new("drive-user-a");

    let uploaded = drive
        .upload_object(&object, &staged_object)
        .await
        .expect("fake Drive should store uploaded bytes");

    assert_eq!(uploaded.object, object);
    assert_eq!(drive.object_bytes(&object), Some(bytes.to_vec()));

    github
        .check_permission(&identity, &user, RepositoryPermission::Read)
        .await
        .expect("fake GitHub should authorize object download");

    let cache_root = tempfile::tempdir().expect("cache tempdir should be created");
    let cache = LocalCacheLayout::new(cache_root.path());
    let cached_object_path = cache.object_path(&object);
    let downloaded = drive
        .download_object(&object, &cached_object_path)
        .await
        .expect("fake Drive should download bytes into the local cache");

    assert_eq!(downloaded.object, object);
    assert_eq!(
        fs::read(&cached_object_path).expect("cached object should be readable"),
        bytes
    );

    let checkout_path = repo.write_file(
        "assets/model.bin",
        &lfs_pointer_file(object.oid.as_hex(), object.size.bytes()),
    );
    let materialized = cache
        .hydrate_pointer_file(&checkout_path)
        .expect("checkout pointer should hydrate from verified cache bytes");

    assert_eq!(materialized.object, object);
    assert_eq!(
        fs::read(repo.path().join("assets/model.bin"))
            .expect("hydrated checkout bytes should be readable"),
        bytes
    );
}
