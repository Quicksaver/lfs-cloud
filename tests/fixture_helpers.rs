//! Tests for reusable integration-test fixtures.

mod support;

use lfs_cloud::{
    LfsPointer, RepositoryAuthentication, RepositoryIdentity, RepositoryPermission,
    RepositoryProvider, RepositoryProviderError, RepositoryUser, ServerError, StorageError,
    StorageProvider,
};
use support::{
    FakeRepositoryProvider, FakeStorageProvider, TEST_OID_A, TEST_OID_B, TempGitRepo, lfs_object,
    lfs_object_for_bytes, lfs_pointer_file, write_lfs_pointer,
};

const TEST_REPOSITORY_NAMESPACE: &str = "github-main:owner/repo";

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

    let identity = repository_identity("github-main", "github.com", "owner", "repo", "repo-123");
    let reader = RepositoryUser::new("github-main", "reader", Some("user-123".to_owned()));
    let authentication = RepositoryAuthentication::new(reader, "provider-token");

    let authorization = provider
        .check_permission(&identity, &authentication, RepositoryPermission::Read)
        .await
        .expect("read permission should authorize reads");
    let denied = provider
        .check_permission(&identity, &authentication, RepositoryPermission::Write)
        .await
        .expect_err("read permission should not authorize writes");

    assert_eq!(identity.stable_id.as_deref(), Some("repo-123"));
    assert_eq!(authorization.granted, RepositoryPermission::Read);
    assert!(matches!(
        denied,
        ServerError::RepositoryProvider {
            source: RepositoryProviderError::PermissionDenied {
                required: RepositoryPermission::Write,
                ..
            }
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

    let wrong_provider =
        repository_identity("github-alt", "github.com", "owner", "repo", "repo-123");
    let wrong_host = repository_identity(
        "github-main",
        "gitlab.example.com",
        "owner",
        "repo",
        "repo-123",
    );
    let wrong_stable_id =
        repository_identity("github-main", "github.com", "owner", "repo", "repo-456");
    let wrong_provider_user =
        RepositoryUser::new("github-alt", "reader", Some("user-123".to_owned()));
    let reader_authentication = RepositoryAuthentication::new(reader, "provider-token");
    let wrong_provider_authentication =
        RepositoryAuthentication::new(wrong_provider_user, "provider-token");

    assert!(matches!(
        provider
            .check_permission(
                &wrong_provider,
                &reader_authentication,
                RepositoryPermission::Read,
            )
            .await,
        Err(ServerError::RepositoryProvider {
            source: RepositoryProviderError::RepositoryNotFound { .. }
        })
    ));
    assert!(matches!(
        provider
            .check_permission(
                &wrong_host,
                &reader_authentication,
                RepositoryPermission::Read,
            )
            .await,
        Err(ServerError::RepositoryProvider {
            source: RepositoryProviderError::RepositoryNotFound { .. }
        })
    ));
    assert!(matches!(
        provider
            .check_permission(
                &wrong_stable_id,
                &reader_authentication,
                RepositoryPermission::Read,
            )
            .await,
        Err(ServerError::RepositoryProvider {
            source: RepositoryProviderError::RepositoryNotFound { .. }
        })
    ));
    let identity = repository_identity("github-main", "github.com", "owner", "repo", "repo-123");
    assert!(matches!(
        provider
            .check_permission(
                &identity,
                &wrong_provider_authentication,
                RepositoryPermission::Read,
            )
            .await,
        Err(ServerError::RepositoryProvider {
            source: RepositoryProviderError::PermissionDenied { .. }
        })
    ));
}

fn repository_identity(
    provider_id: &str,
    host: &str,
    owner: &str,
    name: &str,
    stable_id: &str,
) -> RepositoryIdentity {
    RepositoryIdentity {
        provider_id: provider_id.to_owned(),
        stable_id: Some(stable_id.to_owned()),
        host: host.to_owned(),
        owner: owner.to_owned(),
        name: name.to_owned(),
    }
}

#[tokio::test]
async fn fake_storage_provider_uploads_downloads_and_deletes_bytes() {
    let repo = TempGitRepo::new();
    let bytes = b"large file bytes";
    let source = repo.write_file("objects/source.bin", "large file bytes");
    let destination = repo.path().join("downloads/source.bin");
    let object = lfs_object_for_bytes(bytes);
    let provider = FakeStorageProvider::new("drive-user-a");

    let uploaded = provider
        .upload_object(TEST_REPOSITORY_NAMESPACE, &object, &source)
        .await
        .expect("fixture upload should succeed");
    let downloaded = provider
        .download_object(TEST_REPOSITORY_NAMESPACE, &object, &destination)
        .await
        .expect("fixture download should succeed");

    assert!(uploaded.backend_id.contains(object.oid.as_hex()));
    assert_eq!(downloaded.object, object);
    assert_eq!(repo.read_file("downloads/source.bin"), "large file bytes");
    assert_eq!(
        provider.object_bytes(TEST_REPOSITORY_NAMESPACE, &object),
        Some(bytes.to_vec())
    );

    let deletion = provider
        .delete_or_mark_object(TEST_REPOSITORY_NAMESPACE, &object)
        .await
        .expect("fixture deletion should succeed");

    assert_eq!(deletion, lfs_cloud::StorageDeleteOutcome::Deleted);
    assert!(
        !provider
            .object_exists(TEST_REPOSITORY_NAMESPACE, &object)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn fake_storage_provider_rejects_size_mismatched_uploads() {
    let repo = TempGitRepo::new();
    let source = repo.write_file("objects/source.bin", "large file bytes");
    let object = lfs_object(TEST_OID_B, 15);
    let provider = FakeStorageProvider::new("drive-user-a");

    let error = provider
        .upload_object(TEST_REPOSITORY_NAMESPACE, &object, &source)
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
    assert_eq!(
        provider.object_bytes(TEST_REPOSITORY_NAMESPACE, &object),
        None
    );
}

#[tokio::test]
async fn fake_storage_provider_rejects_oid_mismatched_uploads() {
    let repo = TempGitRepo::new();
    let source = repo.write_file("objects/source.bin", "large file bytes");
    let object = lfs_object(TEST_OID_B, 16);
    let provider = FakeStorageProvider::new("drive-user-a");

    let error = provider
        .upload_object(TEST_REPOSITORY_NAMESPACE, &object, &source)
        .await
        .expect_err("fixture upload should enforce exact LFS object oid");

    assert!(matches!(
        error,
        StorageError::IntegrityMismatch {
            expected_oid,
            expected_size: 16,
            actual_oid,
            actual_size: 16,
        } if expected_oid == TEST_OID_B && actual_oid != TEST_OID_B
    ));
    assert_eq!(
        provider.object_bytes(TEST_REPOSITORY_NAMESPACE, &object),
        None
    );
}

#[tokio::test]
async fn fake_storage_provider_reports_missing_objects() {
    let repo = TempGitRepo::new();
    let provider = FakeStorageProvider::new("drive-user-a");
    let object = lfs_object(TEST_OID_A, 42);
    let destination = repo.path().join("missing.bin");

    let error = provider
        .download_object(TEST_REPOSITORY_NAMESPACE, &object, &destination)
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

    let delete_error = provider
        .delete_or_mark_object(TEST_REPOSITORY_NAMESPACE, &object)
        .await
        .expect_err("missing object deletion should fail");

    assert!(matches!(
        delete_error,
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
