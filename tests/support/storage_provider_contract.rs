/// Asserts the object lifecycle and namespace-isolation semantics required of
/// every storage-provider adapter.
///
/// Repository-bound adapters may reject the isolated namespace explicitly.
/// Providers that support multiple namespaces must keep the two object
/// lifecycles independent instead.
pub async fn assert_storage_provider_contract(
    provider: &dyn StorageProvider,
    repository_namespace: &str,
    isolated_repository_namespace: &str,
) {
    let object_bytes = b"shared storage provider contract bytes";
    let object = lfs_object_for_bytes(object_bytes);
    let missing_bytes = b"missing storage provider contract bytes";
    let missing_object = lfs_object_for_bytes(missing_bytes);
    let source_root = tempfile::tempdir().expect("contract source root should be created");
    let source = source_root.path().join("object.bin");
    fs::write(&source, object_bytes).expect("contract source should be written");
    let destination = source_root.path().join("downloads/object.bin");

    let missing_error = provider
        .download_object(repository_namespace, &missing_object, &destination)
        .await
        .expect_err("missing storage object should fail");
    assert!(
        matches!(
            missing_error,
            StorageError::ObjectNotFound {
                provider: ref error_provider,
                ref oid,
                size,
            } if error_provider == StorageProvider::provider_id(provider)
                && oid == missing_object.oid.as_hex()
                && size == missing_object.size.bytes()
        ),
        "missing objects must report ObjectNotFound"
    );

    let invalid_source = source_root.path().join("invalid.bin");
    fs::write(
        &invalid_source,
        b"bytes that do not match the requested object",
    )
    .expect("invalid contract source should be written");
    let invalid_error = provider
        .upload_object(repository_namespace, &object, &invalid_source)
        .await
        .expect_err("mismatched staged bytes should fail");
    assert!(
        matches!(invalid_error, StorageError::IntegrityMismatch { .. }),
        "mismatched staged bytes must report IntegrityMismatch"
    );
    assert!(
        !provider
            .object_exists(repository_namespace, &object)
            .await
            .expect("failed upload existence check should succeed"),
        "a rejected staged file must not create a discoverable backend object"
    );

    let uploaded = provider
        .upload_object(repository_namespace, &object, &source)
        .await
        .expect("valid staged bytes should upload");
    assert_stored_object_identity(provider, repository_namespace, &object, &uploaded);
    assert!(
        provider
            .object_exists(repository_namespace, &object)
            .await
            .expect("uploaded object existence check should succeed")
    );

    let downloaded = provider
        .download_object(repository_namespace, &object, &destination)
        .await
        .expect("uploaded object should download");
    assert_stored_object_identity(provider, repository_namespace, &object, &downloaded);
    assert_eq!(
        fs::read(&destination).expect("contract download should be readable"),
        object_bytes,
        "downloaded bytes must match the staged upload"
    );

    let repeated = provider
        .upload_object(repository_namespace, &object, &source)
        .await
        .expect("re-uploading the same object should succeed");
    assert_stored_object_identity(provider, repository_namespace, &object, &repeated);
    assert_eq!(
        repeated.backend_id, uploaded.backend_id,
        "idempotent re-upload must return the existing backend object"
    );
    let repeated_invalid_error = provider
        .upload_object(repository_namespace, &object, &invalid_source)
        .await
        .expect_err("idempotency must not bypass staged-byte verification");
    assert!(
        matches!(
            repeated_invalid_error,
            StorageError::IntegrityMismatch { .. }
        ),
        "re-upload must verify staged bytes before returning the existing object"
    );

    let isolated_upload = provider
        .upload_object(isolated_repository_namespace, &object, &source)
        .await;
    let isolated_object_was_created = match isolated_upload {
        Ok(stored) => {
            assert_stored_object_identity(
                provider,
                isolated_repository_namespace,
                &object,
                &stored,
            );
            true
        }
        Err(StorageError::RepositoryNamespaceMismatch {
            provider: ref mismatch_provider,
        }) => {
            assert_eq!(mismatch_provider, StorageProvider::provider_id(provider));
            false
        }
        Err(error) => {
            panic!("foreign namespace must be isolated or explicitly rejected, got {error}")
        }
    };

    let deletion = provider
        .delete_or_mark_object(repository_namespace, &object)
        .await
        .expect("cleanup should return a documented outcome");
    match deletion {
        StorageDeleteOutcome::Deleted => assert!(
            !provider
                .object_exists(repository_namespace, &object)
                .await
                .expect("deleted object existence check should succeed"),
            "Deleted must remove the requested namespaced object"
        ),
        StorageDeleteOutcome::Marked { ref marker } => {
            assert!(
                !marker.trim().is_empty(),
                "cleanup marker must be meaningful"
            );
        }
        StorageDeleteOutcome::Retained { ref reason } => {
            assert!(
                !reason.trim().is_empty(),
                "retention reason must be meaningful"
            );
        }
    }

    if isolated_object_was_created {
        assert!(
            provider
                .object_exists(isolated_repository_namespace, &object)
                .await
                .expect("isolated object existence check should succeed"),
            "cleanup in one namespace must not affect another namespace"
        );
    } else {
        let isolated_result = provider
            .object_exists(isolated_repository_namespace, &object)
            .await;
        assert!(
            matches!(
                isolated_result,
                Err(StorageError::RepositoryNamespaceMismatch { .. })
            ),
            "repository-bound provider must keep rejecting the isolated namespace"
        );
    }
}

fn assert_stored_object_identity(
    provider: &dyn StorageProvider,
    repository_namespace: &str,
    object: &LfsObject,
    stored: &StoredObject,
) {
    assert_eq!(stored.provider_id, StorageProvider::provider_id(provider));
    assert_eq!(stored.repository_namespace, repository_namespace);
    assert_eq!(stored.object, *object);
    assert!(
        !stored.backend_id.trim().is_empty(),
        "stored backend identity must not be blank"
    );
}
use super::{
    LfsObject, StorageDeleteOutcome, StorageError, StorageProvider, StoredObject, fs,
    lfs_object_for_bytes,
};
