//! Tests for reusable integration-test fixtures.

mod support;

use lfs_cloud::{
    LfsPointer, RepositoryHandle, RepositoryPermission, RepositoryProvider,
    RepositoryProviderError, RepositoryUser, StorageError, StorageProvider,
};
use support::{
    FakeRepositoryProvider, FakeStorageProvider, TEST_OID_A, TEST_OID_B, TempGitRepo, lfs_object,
    lfs_pointer_file, write_lfs_pointer,
};

#[test]
fn temp_git_repo_writes_and_commits_pointer_files() {
    let repo = TempGitRepo::new();
    write_lfs_pointer(&repo, "assets/model.bin", TEST_OID_A, 42);
    repo.commit_all("add pointer");

    let pointer = LfsPointer::parse(&repo.read_file("assets/model.bin"))
        .expect("fixture should render a parseable pointer");
    let log = repo.git(["log", "--oneline"]);

    assert_eq!(pointer.object, lfs_object(TEST_OID_A, 42));
    assert!(repo.path().join(".git").is_dir());
    assert!(String::from_utf8_lossy(&log.stdout).contains("add pointer"));
}

#[tokio::test]
async fn fake_repository_provider_resolves_and_denies_permissions() {
    let provider = FakeRepositoryProvider::new("github-main");
    provider.add_repository("github.com", "owner", "repo", Some("repo-123".to_owned()));
    provider.grant_permission(
        "github.com",
        "owner",
        "repo",
        "reader",
        RepositoryPermission::Read,
    );

    let handle = RepositoryHandle::new("github-main", "github.com", "owner", "repo");
    let identity = provider
        .repository_identity(&handle)
        .await
        .expect("configured repository should resolve");
    let reader = RepositoryUser::new("github-main", "reader", Some("user-123".to_owned()));

    let authorization = provider
        .check_permission(&identity, &reader, RepositoryPermission::Read)
        .await
        .expect("read permission should authorize reads");
    let denied = provider
        .check_permission(&identity, &reader, RepositoryPermission::Write)
        .await
        .expect_err("read permission should not authorize writes");

    assert_eq!(identity.stable_id.as_deref(), Some("repo-123"));
    assert_eq!(authorization.granted, RepositoryPermission::Read);
    assert!(matches!(
        denied,
        RepositoryProviderError::PermissionDenied {
            required: RepositoryPermission::Write,
            ..
        }
    ));
}

#[tokio::test]
async fn fake_repository_provider_requires_exact_repository_identity() {
    let provider = FakeRepositoryProvider::new("github-main");
    provider.add_repository("github.com", "owner", "repo", Some("repo-123".to_owned()));
    provider.grant_permission(
        "github.com",
        "owner",
        "repo",
        "reader",
        RepositoryPermission::Read,
    );
    let reader = RepositoryUser::new("github-main", "reader", Some("user-123".to_owned()));

    let wrong_provider = RepositoryHandle::new("github-alt", "github.com", "owner", "repo");
    let wrong_host = RepositoryHandle::new("github-main", "gitlab.example.com", "owner", "repo");
    let spoofed_identity = lfs_cloud::RepositoryIdentity::from_handle(&wrong_host, None);

    assert!(matches!(
        provider.repository_identity(&wrong_provider).await,
        Err(RepositoryProviderError::RepositoryNotFound { .. })
    ));
    assert!(matches!(
        provider.repository_identity(&wrong_host).await,
        Err(RepositoryProviderError::RepositoryNotFound { .. })
    ));
    assert!(matches!(
        provider
            .check_permission(&spoofed_identity, &reader, RepositoryPermission::Read)
            .await,
        Err(RepositoryProviderError::RepositoryNotFound { .. })
    ));
}

#[tokio::test]
async fn fake_storage_provider_uploads_downloads_and_deletes_bytes() {
    let repo = TempGitRepo::new();
    let source = repo.write_file("objects/source.bin", "large file bytes");
    let destination = repo.path().join("downloads/source.bin");
    let object = lfs_object(TEST_OID_B, 16);
    let provider = FakeStorageProvider::new("drive-user-a");

    let uploaded = provider
        .upload_object(&object, &source)
        .await
        .expect("fixture upload should succeed");
    let downloaded = provider
        .download_object(&object, &destination)
        .await
        .expect("fixture download should succeed");

    assert!(uploaded.backend_id.contains(TEST_OID_B));
    assert_eq!(downloaded.object, object);
    assert_eq!(repo.read_file("downloads/source.bin"), "large file bytes");
    assert_eq!(
        provider.object_bytes(&object),
        Some(b"large file bytes".to_vec())
    );

    let deletion = provider
        .delete_or_mark_object(&object)
        .await
        .expect("fixture deletion should succeed");

    assert_eq!(deletion, lfs_cloud::StorageDeleteOutcome::Deleted);
    assert!(!provider.object_exists(&object).await.unwrap());
}

#[tokio::test]
async fn fake_storage_provider_rejects_size_mismatched_uploads() {
    let repo = TempGitRepo::new();
    let source = repo.write_file("objects/source.bin", "large file bytes");
    let object = lfs_object(TEST_OID_B, 15);
    let provider = FakeStorageProvider::new("drive-user-a");

    let error = provider
        .upload_object(&object, &source)
        .await
        .expect_err("fixture upload should enforce exact LFS object size");

    assert!(matches!(
        error,
        StorageError::IntegrityMismatch {
            expected_size: 15,
            actual_size: 16,
            ..
        }
    ));
    assert_eq!(provider.object_bytes(&object), None);
}

#[tokio::test]
async fn fake_storage_provider_reports_missing_objects() {
    let repo = TempGitRepo::new();
    let provider = FakeStorageProvider::new("drive-user-a");
    let object = lfs_object(TEST_OID_A, 42);
    let destination = repo.path().join("missing.bin");

    let error = provider
        .download_object(&object, &destination)
        .await
        .expect_err("missing object should fail");

    assert!(matches!(
        error,
        StorageError::ObjectNotFound {
            provider,
            size: 42,
            ..
        } if provider == "drive-user-a"
    ));
}

#[test]
fn pointer_file_helper_renders_canonical_lfs_pointer() {
    let pointer_file = lfs_pointer_file(TEST_OID_A, 123);
    let parsed = LfsPointer::parse(&pointer_file).expect("pointer fixture should parse");

    assert_eq!(parsed.object, lfs_object(TEST_OID_A, 123));
    assert_eq!(
        pointer_file,
        format!("version https://git-lfs.github.com/spec/v1\noid sha256:{TEST_OID_A}\nsize 123\n")
    );
}
