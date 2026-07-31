fn classify_common_drive_error(
    storage: &GoogleDriveStorageConfig,
    status: StatusCode,
    diagnostic: &DriveDiagnostic,
) -> Option<StorageError> {
    if status == StatusCode::UNAUTHORIZED
        || diagnostic
            .reasons
            .iter()
            .any(|reason| matches!(reason.as_str(), "authError" | "insufficientPermissions"))
    {
        return Some(StorageError::AuthenticationRequired {
            provider: storage.id.clone(),
        });
    }
    if diagnostic.reasons.iter().any(|reason| {
        matches!(
            reason.as_str(),
            "appNotAuthorizedToFile"
                | "domainPolicy"
                | "insufficientFilePermissions"
                | "teamDriveMembershipRequired"
        )
    }) {
        return Some(StorageError::PermissionDenied {
            provider: storage.id.clone(),
            message: diagnostic.message.clone(),
        });
    }
    if diagnostic.reasons.iter().any(|reason| {
        matches!(
            reason.as_str(),
            "activeItemCreationLimitExceeded"
                | "dailyLimitExceeded"
                | "myDriveHierarchyDepthLimitExceeded"
                | "numChildrenInNonRootLimitExceeded"
                | "quotaExceeded"
                | "storageQuotaExceeded"
                | "teamDriveFileLimitExceeded"
                | "teamDriveHierarchyTooDeep"
        )
    }) {
        return Some(StorageError::QuotaExceeded {
            provider: storage.id.clone(),
            message: diagnostic.message.clone(),
        });
    }
    if status == StatusCode::TOO_MANY_REQUESTS
        || status.is_server_error()
        || diagnostic.reasons.iter().any(|reason| {
            matches!(
                reason.as_str(),
                "rateLimitExceeded" | "sharingRateLimitExceeded" | "userRateLimitExceeded"
            )
        })
    {
        return Some(StorageError::Retryable {
            provider: storage.id.clone(),
            message: diagnostic.message.clone(),
        });
    }

    None
}

fn drive_error_message(token: &GoogleDriveAccessToken, body: &str) -> DriveDiagnostic {
    if let Ok(GoogleDriveErrorResponse { error: Some(error) }) =
        serde_json::from_str::<GoogleDriveErrorResponse>(body)
    {
        let message = error
            .message
            .filter(|value| !value.trim().is_empty())
            .unwrap_or_else(|| "Google Drive request failed".to_owned());
        let reasons = error
            .errors
            .into_iter()
            .filter_map(|detail| detail.reason)
            .filter(|reason| !reason.trim().is_empty())
            .collect();
        return DriveDiagnostic {
            message: sanitize_drive_diagnostic(token, &cap_google_diagnostic(&message)),
            reasons,
        };
    }

    DriveDiagnostic {
        message: sanitize_drive_diagnostic(token, &cap_google_diagnostic(body)),
        reasons: Vec::new(),
    }
}

struct DriveDiagnostic {
    message: String,
    reasons: Vec<String>,
}

fn sanitize_drive_diagnostic(token: &GoogleDriveAccessToken, message: &str) -> String {
    let sanitized = redact_secret_from_message(message, token.as_str());
    if sanitized.trim().is_empty() {
        "Google Drive request failed".to_owned()
    } else {
        sanitized
    }
}

fn drive_transport_error(
    storage: &GoogleDriveStorageConfig,
    token: &GoogleDriveAccessToken,
    source: reqwest::Error,
) -> StorageError {
    StorageError::Retryable {
        provider: storage.id.clone(),
        message: sanitize_drive_diagnostic(
            token,
            &format!("Google Drive request failed: {source}"),
        ),
    }
}

fn cap_google_diagnostic(message: &str) -> String {
    message.chars().take(MAX_GOOGLE_ERROR_BODY_LEN).collect()
}

fn redact_secret_from_message(message: &str, secret: &str) -> String {
    if secret.is_empty() {
        return message.to_owned();
    }

    let mut sanitized = message.replace(secret, "[redacted]");
    if secret.len() < MIN_REDACTED_SECRET_FRAGMENT_LEN {
        return sanitized;
    }

    for prefix_length in (MIN_REDACTED_SECRET_FRAGMENT_LEN..secret.len()).rev() {
        let Some(prefix) = secret.get(..prefix_length) else {
            continue;
        };
        if sanitized.ends_with(prefix) {
            let suffix_start = sanitized.len() - prefix.len();
            sanitized.replace_range(suffix_start.., "[redacted]");
            break;
        }
    }
    sanitized
}

async fn read_google_response_body(response: reqwest::Response) -> Result<String, reqwest::Error> {
    read_bounded_lossy_response_body(response, MAX_GOOGLE_ERROR_BODY_LEN).await
}


#[cfg(test)]
pub(super) mod diagnostics_tests {
    use super::*;
    use super::object_store_tests::DriveFilesListServer;

    #[test]
    fn drive_diagnostics_redact_token_fragments_at_truncation_boundary() {
        let body = format!("{}access", "x".repeat(super::MAX_GOOGLE_ERROR_BODY_LEN - 6));
        let diagnostic = super::drive_error_message(&access_token(), &body);

        assert!(!diagnostic.message.contains("access"));
        assert!(diagnostic.message.ends_with("[redacted]"));
    }

    #[tokio::test]
    async fn object_store_maps_auth_and_rate_limit_failures() {
        let auth_server = DriveFilesListServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"missing scope access-token","errors":[{"reason":"insufficientPermissions"}]}}"#,
        )
        .await;
        let auth_store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &auth_server.base_url,
        )
        .expect("object store should build");

        let auth_error = auth_store
            .object_exists(&lfs_object())
            .await
            .expect_err("insufficient scope should fail");
        assert!(matches!(
            auth_error,
            StorageError::AuthenticationRequired { ref provider } if provider == "drive-user-a"
        ));

        let rate_limit_server = DriveFilesListServer::start(
            StatusCode::FORBIDDEN,
            r#"{"error":{"message":"try later access-token","errors":[{"reason":"rateLimitExceeded"}]}}"#,
        )
        .await;
        let rate_limit_store = GoogleDriveObjectStore::with_client_and_api_base_url(
            storage_config("google-drive-user-a"),
            "github.com/owner/repo",
            access_token(),
            reqwest::Client::new(),
            &rate_limit_server.base_url,
        )
        .expect("object store should build");

        let rate_limit_error = rate_limit_store
            .object_exists(&lfs_object())
            .await
            .expect_err("rate limit should fail");
        assert!(matches!(
            rate_limit_error,
            StorageError::Retryable {
                provider,
                message,
            } if provider == "drive-user-a"
                && message.contains("try later")
                && !message.contains("access-token")
        ));
    }

    #[test]
    fn common_drive_errors_classify_documented_capacity_limits() {
        for reason in [
            "activeItemCreationLimitExceeded",
            "dailyLimitExceeded",
            "myDriveHierarchyDepthLimitExceeded",
            "numChildrenInNonRootLimitExceeded",
            "teamDriveFileLimitExceeded",
            "teamDriveHierarchyTooDeep",
        ] {
            let diagnostic = super::DriveDiagnostic {
                message: format!("Drive capacity limit: {reason}"),
                reasons: vec![reason.to_owned()],
            };

            let error = super::classify_common_drive_error(
                &storage_config("google-drive-user-a"),
                StatusCode::FORBIDDEN,
                &diagnostic,
            )
            .expect("documented capacity reason should be classified");

            assert!(matches!(
                error,
                StorageError::QuotaExceeded {
                    ref provider,
                    ref message,
                } if provider == "drive-user-a" && message.contains(reason)
            ));
        }
    }

    #[test]
    fn common_drive_errors_classify_documented_permission_denials() {
        for reason in [
            "appNotAuthorizedToFile",
            "domainPolicy",
            "insufficientFilePermissions",
            "teamDriveMembershipRequired",
        ] {
            let diagnostic = super::DriveDiagnostic {
                message: format!("Drive permission denied: {reason}"),
                reasons: vec![reason.to_owned()],
            };

            let error = super::classify_common_drive_error(
                &storage_config("google-drive-user-a"),
                StatusCode::FORBIDDEN,
                &diagnostic,
            )
            .expect("documented permission reason should be classified");

            assert!(matches!(
                error,
                StorageError::PermissionDenied {
                    ref provider,
                    ref message,
                } if provider == "drive-user-a" && message.contains(reason)
            ));
        }
    }

    #[test]
    fn common_drive_errors_classify_sharing_rate_limit_as_retryable() {
        let diagnostic = super::DriveDiagnostic {
            message: "Drive sharing rate limit exceeded".to_owned(),
            reasons: vec!["sharingRateLimitExceeded".to_owned()],
        };

        let error = super::classify_common_drive_error(
            &storage_config("google-drive-user-a"),
            StatusCode::FORBIDDEN,
            &diagnostic,
        )
        .expect("documented rate-limit reason should be classified");

        assert!(matches!(
            error,
            StorageError::Retryable {
                ref provider,
                ref message,
            } if provider == "drive-user-a" && message.contains("rate limit")
        ));
    }

}
