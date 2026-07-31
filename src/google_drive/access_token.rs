/// Short-lived Google Drive OAuth access token.
#[derive(Clone, Eq, PartialEq)]
pub struct GoogleDriveAccessToken {
    access_token: String,
    token_type: String,
    expires_in_seconds: Option<u64>,
    scope: Vec<String>,
}

impl GoogleDriveAccessToken {
    /// Creates a deterministic bearer token for unit-test provider adapters.
    #[cfg(test)]
    pub(crate) fn for_test(access_token: impl Into<String>) -> Self {
        Self {
            access_token: access_token.into(),
            token_type: "Bearer".to_owned(),
            expires_in_seconds: Some(GCLOUD_ADC_ACCESS_TOKEN_LIFETIME_SECONDS),
            scope: Vec::new(),
        }
    }

    /// Returns the raw bearer token secret for provider HTTP requests.
    ///
    /// Callers must not log this value or return it to Git LFS clients.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.access_token
    }

    /// Returns an HTTP `Authorization` header value for this bearer token.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the token cannot be represented as an HTTP
    /// header value.
    pub fn authorization_header_value(&self, provider: &str) -> StorageResult<HeaderValue> {
        HeaderValue::from_str(&format!("Bearer {}", self.access_token)).map_err(|_| {
            drive_upstream_error(
                provider,
                "Google OAuth access token could not be encoded as an HTTP header",
            )
        })
    }

    /// Returns the token lifetime reported by Google, when present.
    #[must_use]
    pub fn expires_in_seconds(&self) -> Option<u64> {
        self.expires_in_seconds
    }

    /// Returns the OAuth scopes reported by Google for this access token.
    #[must_use]
    pub fn scope(&self) -> &[String] {
        &self.scope
    }
}

impl fmt::Debug for GoogleDriveAccessToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GoogleDriveAccessToken")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("expires_in_seconds", &self.expires_in_seconds)
            .field("scope", &self.scope)
            .finish()
    }
}

/// Source of short-lived Google Drive access tokens for runtime storage work.
pub(crate) trait GoogleDriveAccessTokenSource: Send + Sync {
    /// Returns a token suitable for the configured Google Drive provider.
    fn access_token<'a>(
        &'a self,
        storage: &'a GoogleDriveStorageConfig,
    ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>>;
}

impl GoogleDriveAccessTokenSource for GoogleDriveGcloudTokenProvider {
    fn access_token<'a>(
        &'a self,
        storage: &'a GoogleDriveStorageConfig,
    ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
        Box::pin(async move { self.access_token(&storage.id, &storage.credentials).await })
    }
}

/// Single-flight cache for short-lived Google Drive access tokens.
#[derive(Clone, Default)]
pub(crate) struct GoogleDriveAccessTokenCache {
    tokens: Arc<AsyncMutex<HashMap<String, GoogleDriveAccessTokenSlot>>>,
}

type GoogleDriveAccessTokenSlot = Arc<AsyncMutex<Option<CachedGoogleDriveAccessToken>>>;

#[derive(Clone)]
struct CachedGoogleDriveAccessToken {
    token: GoogleDriveAccessToken,
    refresh_at: Instant,
}

impl GoogleDriveAccessTokenCache {
    /// Returns a cached token or refreshes it shortly before expiry.
    pub(crate) async fn get_or_refresh(
        &self,
        storage: &GoogleDriveStorageConfig,
        token_source: &dyn GoogleDriveAccessTokenSource,
    ) -> StorageResult<GoogleDriveAccessToken> {
        self.get_or_refresh_at(storage, token_source, Instant::now())
            .await
    }

    async fn get_or_refresh_at(
        &self,
        storage: &GoogleDriveStorageConfig,
        token_source: &dyn GoogleDriveAccessTokenSource,
        now: Instant,
    ) -> StorageResult<GoogleDriveAccessToken> {
        let provider_token = {
            let mut tokens = self.tokens.lock().await;
            Arc::clone(
                tokens
                    .entry(storage.id.clone())
                    .or_insert_with(|| Arc::new(AsyncMutex::new(None))),
            )
        };
        // Serialize refreshes only for this provider. A slow gcloud process
        // must not block cached reads or refreshes for unrelated providers.
        let mut cached_token = provider_token.lock().await;
        if let Some(cached) = cached_token.as_ref()
            && cached.refresh_at > now
        {
            return Ok(cached.token.clone());
        }

        let token = token_source.access_token(storage).await?;
        let refresh_at = token
            .expires_in_seconds()
            .and_then(|seconds| {
                Duration::from_secs(seconds).checked_sub(GOOGLE_ACCESS_TOKEN_REFRESH_SKEW)
            })
            .and_then(|lifetime| now.checked_add(lifetime));

        if let Some(refresh_at) = refresh_at
            && refresh_at > now
        {
            *cached_token = Some(CachedGoogleDriveAccessToken {
                token: token.clone(),
                refresh_at,
            });
        } else {
            // Tokens already within the refresh skew are deliberately not
            // cached, preventing reuse of credentials near their expiry.
            *cached_token = None;
        }

        Ok(token)
    }
}

/// Obtains short-lived Google Drive access tokens from Google Cloud CLI ADC.
///
/// The provider runs `gcloud auth application-default print-access-token`
/// against an isolated `CLOUDSDK_CONFIG` directory. It never reads or exposes
/// the generated ADC file in that directory.
#[derive(Clone, Debug, Default)]
pub struct GoogleDriveGcloudTokenProvider;

impl GoogleDriveGcloudTokenProvider {
    /// Creates a Google Cloud CLI ADC token provider.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }

    /// Validates that the configured directory contains generated ADC state.
    ///
    /// This check performs no process launch or network request, making it
    /// suitable for local readiness and migration dry-run diagnostics.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the configured directory is missing, is
    /// not a directory, or lacks `application_default_credentials.json`.
    pub fn validate_configuration(
        &self,
        provider_id: &str,
        credentials: &GoogleDriveGcloudCredentialsConfig,
    ) -> StorageResult<()> {
        validate_gcloud_directory(provider_id, &credentials.config_dir)
    }

    /// Validates generated ADC state and availability of the configured CLI.
    ///
    /// The check runs only `gcloud --version`; it neither mints a token nor
    /// contacts Google. Ambient credential overrides are removed just as they
    /// are for token acquisition.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when ADC state is incomplete or the configured
    /// executable cannot run successfully.
    pub fn validate_local_readiness(
        &self,
        provider_id: &str,
        credentials: &GoogleDriveGcloudCredentialsConfig,
    ) -> StorageResult<()> {
        self.validate_configuration(provider_id, credentials)?;
        let status = ProcessCommand::new(&credentials.executable)
            .arg("--version")
            .env(CLOUDSDK_CONFIG_ENV, &credentials.config_dir)
            .env(CLOUDSDK_CORE_DISABLE_PROMPTS_ENV, "1")
            .env_remove(GOOGLE_APPLICATION_CREDENTIALS_ENV)
            .env_remove(CLOUDSDK_AUTH_ACCESS_TOKEN_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map_err(|_| {
                gcloud_credential_error(
                    provider_id,
                    "configured gcloud CLI could not be executed; install the Google Cloud CLI or correct credentials.executable",
                )
            })?;
        if !status.success() {
            return Err(gcloud_credential_error(
                provider_id,
                "configured gcloud CLI failed its local version check",
            ));
        }
        Ok(())
    }

    /// Requests one short-lived access token from the configured `gcloud` CLI.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] when the ADC directory is incomplete, the
    /// executable cannot run, the non-interactive command times out or fails,
    /// or stdout does not contain exactly one valid access token.
    pub async fn access_token(
        &self,
        provider_id: &str,
        credentials: &GoogleDriveGcloudCredentialsConfig,
    ) -> StorageResult<GoogleDriveAccessToken> {
        self.validate_configuration(provider_id, credentials)?;

        let mut command = Command::new(&credentials.executable);
        command
            .args([
                "auth",
                "application-default",
                "print-access-token",
                "--quiet",
            ])
            .env(CLOUDSDK_CONFIG_ENV, &credentials.config_dir)
            .env(CLOUDSDK_CORE_DISABLE_PROMPTS_ENV, "1")
            .env_remove(GOOGLE_APPLICATION_CREDENTIALS_ENV)
            .env_remove(CLOUDSDK_AUTH_ACCESS_TOKEN_ENV)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let output = tokio::time::timeout(GCLOUD_ADC_TOKEN_TIMEOUT, command.output())
            .await
            .map_err(|_| gcloud_credential_error(provider_id, "gcloud ADC token command timed out"))?
            .map_err(|_| {
                gcloud_credential_error(
                    provider_id,
                    "configured gcloud CLI could not be executed; install the Google Cloud CLI or correct credentials.executable",
                )
            })?;

        if !output.status.success() {
            let status = output.status.code().map_or_else(
                || "without an exit code".to_owned(),
                |code| format!("with exit code {code}"),
            );
            return Err(gcloud_credential_error(
                provider_id,
                format!(
                    "gcloud ADC token command failed {status}; rerun gcloud auth application-default login for the configured credentials directory"
                ),
            ));
        }

        parse_gcloud_access_token(provider_id, &output.stdout)
    }
}

fn validate_gcloud_directory(provider_id: &str, config_dir: &Path) -> StorageResult<()> {
    let directory = fs::metadata(config_dir).map_err(|_| {
        gcloud_credential_error(
            provider_id,
            "gcloud ADC credentials directory is missing or unreadable; run gcloud auth application-default login with the configured CLOUDSDK_CONFIG",
        )
    })?;
    if !directory.is_dir() {
        return Err(gcloud_credential_error(
            provider_id,
            "configured gcloud ADC credentials path is not a directory",
        ));
    }

    let adc_path = config_dir.join("application_default_credentials.json");
    if !fs::metadata(adc_path).is_ok_and(|metadata| metadata.is_file()) {
        return Err(gcloud_credential_error(
            provider_id,
            "gcloud ADC credentials directory does not contain application_default_credentials.json; run gcloud auth application-default login with the configured CLOUDSDK_CONFIG",
        ));
    }

    Ok(())
}

fn parse_gcloud_access_token(
    provider_id: &str,
    stdout: &[u8],
) -> StorageResult<GoogleDriveAccessToken> {
    if stdout.len() > MAX_GCLOUD_ADC_TOKEN_BYTES {
        return Err(gcloud_credential_error(
            provider_id,
            "gcloud ADC token output exceeded the accepted size",
        ));
    }
    let output = std::str::from_utf8(stdout).map_err(|_| {
        gcloud_credential_error(provider_id, "gcloud ADC token output was not valid UTF-8")
    })?;
    let access_token = output.trim_ascii();
    if access_token.is_empty() || access_token.chars().any(char::is_whitespace) {
        return Err(gcloud_credential_error(
            provider_id,
            "gcloud ADC token output did not contain one access token",
        ));
    }

    Ok(GoogleDriveAccessToken {
        access_token: access_token.to_owned(),
        token_type: "Bearer".to_owned(),
        expires_in_seconds: Some(GCLOUD_ADC_ACCESS_TOKEN_LIFETIME_SECONDS),
        scope: Vec::new(),
    })
}

fn gcloud_credential_error(provider_id: &str, message: impl Into<String>) -> StorageError {
    StorageError::CredentialLoad {
        provider: provider_id.to_owned(),
        reference: "gcloud".to_owned(),
        message: SanitizedMessage::new(message),
    }
}


#[cfg(test)]
pub(super) mod access_token_tests {
    use super::*;

    struct CountingAccessTokenSource {
        calls: AtomicUsize,
    }

    impl GoogleDriveAccessTokenSource for CountingAccessTokenSource {
        fn access_token<'a>(
            &'a self,
            _storage: &'a GoogleDriveStorageConfig,
        ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
            Box::pin(async move {
                let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
                Ok(GoogleDriveAccessToken::for_test(format!(
                    "access-token-{call}"
                )))
            })
        }
    }

    struct ConcurrentAccessTokenSource {
        active_calls: AtomicUsize,
        max_active_calls: AtomicUsize,
    }

    impl GoogleDriveAccessTokenSource for ConcurrentAccessTokenSource {
        fn access_token<'a>(
            &'a self,
            storage: &'a GoogleDriveStorageConfig,
        ) -> ProviderFuture<'a, StorageResult<GoogleDriveAccessToken>> {
            Box::pin(async move {
                let active = self.active_calls.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active_calls.fetch_max(active, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(50)).await;
                self.active_calls.fetch_sub(1, Ordering::SeqCst);
                Ok(GoogleDriveAccessToken::for_test(format!(
                    "access-token-{}",
                    storage.id
                )))
            })
        }
    }

    #[tokio::test]
    async fn access_token_cache_refreshes_before_expiry() {
        let cache = GoogleDriveAccessTokenCache::default();
        let source = CountingAccessTokenSource {
            calls: AtomicUsize::new(0),
        };
        let storage = storage_config("google-drive-user-a");
        let started_at = Instant::now();

        let first = cache
            .get_or_refresh_at(&storage, &source, started_at)
            .await
            .expect("first access token should be minted");
        let cached = cache
            .get_or_refresh_at(&storage, &source, started_at + Duration::from_secs(3_000))
            .await
            .expect("unexpired access token should be reused");
        let refreshed = cache
            .get_or_refresh_at(&storage, &source, started_at + Duration::from_secs(3_540))
            .await
            .expect("access token should refresh at the expiry skew");

        assert_eq!(first.as_str(), "access-token-1");
        assert_eq!(cached.as_str(), first.as_str());
        assert_eq!(refreshed.as_str(), "access-token-2");
        assert_eq!(source.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn access_token_cache_refreshes_distinct_providers_concurrently() {
        let cache = GoogleDriveAccessTokenCache::default();
        let source = ConcurrentAccessTokenSource {
            active_calls: AtomicUsize::new(0),
            max_active_calls: AtomicUsize::new(0),
        };
        let first_storage = storage_config("google-drive-user-a");
        let mut second_storage = storage_config("google-drive-user-b");
        second_storage.id = "drive-user-b".to_owned();

        let (first, second) = tokio::join!(
            cache.get_or_refresh(&first_storage, &source),
            cache.get_or_refresh(&second_storage, &source),
        );

        assert_eq!(
            first.expect("first provider token should refresh").as_str(),
            "access-token-drive-user-a"
        );
        assert_eq!(
            second
                .expect("second provider token should refresh")
                .as_str(),
            "access-token-drive-user-b"
        );
        assert_eq!(source.max_active_calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn gcloud_token_output_parses_without_exposing_scope_claims() {
        let token = parse_gcloud_access_token("drive-user-a", b"ya29.test-token\n")
            .expect("gcloud ADC output should parse");

        assert_eq!(token.as_str(), "ya29.test-token");
        assert_eq!(token.expires_in_seconds(), Some(3_600));
        assert!(token.scope().is_empty());
        assert!(!format!("{token:?}").contains("ya29.test-token"));
    }

    #[test]
    fn gcloud_token_output_rejects_multiple_lines() {
        let error =
            parse_gcloud_access_token("drive-user-a", b"ya29.first-token\nya29.second-token\n")
                .expect_err("multiple token lines should be rejected");

        assert!(matches!(error, StorageError::CredentialLoad { .. }));
        assert!(!error.to_string().contains("ya29.first-token"));
        assert!(!error.to_string().contains("ya29.second-token"));
    }

    #[test]
    fn gcloud_local_readiness_requires_the_configured_executable() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        std::fs::write(
            directory
                .path()
                .join("application_default_credentials.json"),
            "{}",
        )
        .expect("ADC marker file should be written");
        let credentials = GoogleDriveGcloudCredentialsConfig {
            config_dir: directory.path().to_owned(),
            executable: directory.path().join("missing-gcloud"),
        };

        let error = GoogleDriveGcloudTokenProvider::new()
            .validate_local_readiness("drive-user-a", &credentials)
            .expect_err("missing gcloud executable should fail readiness");

        let rendered = error.to_string();
        assert!(rendered.contains("drive-user-a"));
        assert!(rendered.contains("install the Google Cloud CLI"));
        assert!(!rendered.contains("missing-gcloud"));
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn gcloud_provider_launches_windows_command_scripts() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let config_dir = directory.path().join("gcloud-drive");
        std::fs::create_dir(&config_dir).expect("gcloud config directory should be created");
        std::fs::write(
            config_dir.join("application_default_credentials.json"),
            "{}",
        )
        .expect("ADC marker should be written");
        let invocation_path = directory.path().join("invocation.txt");
        let executable = directory.path().join("gcloud-test.cmd");
        std::fs::write(
            &executable,
            format!(
                "@echo off\r\nif \"%~1\"==\"--version\" (\r\n  echo Google Cloud SDK test\r\n  exit /b 0\r\n)\r\n> \"{}\" (\r\n  echo %CLOUDSDK_CONFIG%\r\n  echo %CLOUDSDK_CORE_DISABLE_PROMPTS%\r\n  echo %~1\r\n  echo %~2\r\n  echo %~3\r\n  echo %~4\r\n)\r\necho ya29.gcloud-adc-token\r\n",
                invocation_path.display()
            ),
        )
        .expect("fake gcloud command script should be written");
        let credentials = GoogleDriveGcloudCredentialsConfig {
            config_dir: config_dir.clone(),
            executable,
        };
        let provider = GoogleDriveGcloudTokenProvider::new();

        provider
            .validate_local_readiness("drive-user-a", &credentials)
            .expect("Windows gcloud command script should pass readiness");
        let token = provider
            .access_token("drive-user-a", &credentials)
            .await
            .expect("Windows gcloud command script should return a token");

        assert_eq!(token.as_str(), "ya29.gcloud-adc-token");
        let invocation = std::fs::read_to_string(invocation_path)
            .expect("fake gcloud invocation should be recorded");
        assert_eq!(
            invocation
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>(),
            vec![
                config_dir.to_string_lossy().into_owned(),
                "1".to_owned(),
                "auth".to_owned(),
                "application-default".to_owned(),
                "print-access-token".to_owned(),
                "--quiet".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gcloud_provider_uses_isolated_noninteractive_command() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let config_dir = directory.path().join("gcloud-drive");
        std::fs::create_dir(&config_dir).expect("gcloud config directory should be created");
        std::fs::write(
            config_dir.join("application_default_credentials.json"),
            "{}",
        )
        .expect("ADC marker should be written");
        let invocation_path = directory.path().join("invocation.txt");
        let executable = directory.path().join("gcloud-test");
        std::fs::write(
            &executable,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$CLOUDSDK_CONFIG\" \"$CLOUDSDK_CORE_DISABLE_PROMPTS\" \"$@\" > '{}'\nprintf 'ya29.gcloud-adc-token\\n'\n",
                invocation_path.display()
            ),
        )
        .expect("fake gcloud executable should be written");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake gcloud metadata should load")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions)
            .expect("fake gcloud should become executable");

        let token = GoogleDriveGcloudTokenProvider::new()
            .access_token(
                "drive-user-a",
                &GoogleDriveGcloudCredentialsConfig {
                    config_dir: config_dir.clone(),
                    executable,
                },
            )
            .await
            .expect("fake gcloud should return a token");

        assert_eq!(token.as_str(), "ya29.gcloud-adc-token");
        let invocation = std::fs::read_to_string(invocation_path)
            .expect("fake gcloud invocation should be recorded");
        assert_eq!(
            invocation
                .lines()
                .map(ToOwned::to_owned)
                .collect::<Vec<_>>(),
            vec![
                config_dir.to_string_lossy().into_owned(),
                "1".to_owned(),
                "auth".to_owned(),
                "application-default".to_owned(),
                "print-access-token".to_owned(),
                "--quiet".to_owned(),
            ]
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn gcloud_provider_redacts_failed_command_stderr() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let config_dir = directory.path().join("gcloud-drive");
        std::fs::create_dir(&config_dir).expect("gcloud config directory should be created");
        std::fs::write(
            config_dir.join("application_default_credentials.json"),
            "{}",
        )
        .expect("ADC marker should be written");
        let executable = directory.path().join("gcloud-test");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf 'sensitive-credential' >&2\nexit 42\n",
        )
        .expect("fake gcloud executable should be written");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake gcloud metadata should load")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions)
            .expect("fake gcloud should become executable");

        let error = GoogleDriveGcloudTokenProvider::new()
            .access_token(
                "drive-user-a",
                &GoogleDriveGcloudCredentialsConfig {
                    config_dir,
                    executable,
                },
            )
            .await
            .expect_err("failed gcloud command should be reported");

        assert!(error.to_string().contains("exit code 42"));
        assert!(!error.to_string().contains("sensitive-credential"));
    }

}
