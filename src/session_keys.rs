//! Server-managed encryption keys for durable LFS sessions.

use std::time::{SystemTime, UNIX_EPOCH};

use ring::rand::{SecureRandom, SystemRandom};

use crate::{MetadataDatabase, ServerError, ServerResult};

const NATIVE_KEY_SERVICE: &str = "io.github.Quicksaver.lfscloud.sessions";
const MANAGED_SESSION_KEY_BYTES: usize = 32;

pub(crate) trait SessionEncryptionKeyStore: Send + Sync {
    fn load(&self, account: &str) -> ServerResult<Option<Vec<u8>>>;
    fn store(&self, account: &str, secret: &[u8]) -> ServerResult<()>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct NativeSessionEncryptionKeyStore;

impl NativeSessionEncryptionKeyStore {
    fn entry(account: &str) -> ServerResult<keyring::Entry> {
        keyring::Entry::new(NATIVE_KEY_SERVICE, account).map_err(native_key_store_error)
    }
}

impl SessionEncryptionKeyStore for NativeSessionEncryptionKeyStore {
    fn load(&self, account: &str) -> ServerResult<Option<Vec<u8>>> {
        match Self::entry(account)?.get_secret() {
            Ok(secret) => Ok(Some(secret)),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(error) => Err(native_key_store_error(error)),
        }
    }

    fn store(&self, account: &str, secret: &[u8]) -> ServerResult<()> {
        Self::entry(account)?
            .set_secret(secret)
            .map_err(native_key_store_error)
    }
}

pub(crate) fn load_or_create_managed_session_key(
    database: &MetadataDatabase,
    key_store: &dyn SessionEncryptionKeyStore,
) -> ServerResult<Vec<u8>> {
    let account = database.instance_id()?;
    if let Some(secret) = key_store.load(&account)? {
        if secret.len() != MANAGED_SESSION_KEY_BYTES {
            return Err(ServerError::InvalidConfiguration {
                message: "native session encryption key has an invalid length; run `lfscloud sessions generate-key` to invalidate sessions and replace it".to_owned(),
            });
        }
        return Ok(secret);
    }

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| ServerError::Internal {
            message: "system clock precedes the Unix epoch".to_owned(),
        })?;
    let now = i64::try_from(now.as_secs()).map_err(|_| ServerError::Internal {
        message: "system clock exceeds the durable session timestamp range".to_owned(),
    })?;
    database.delete_expired_sessions(now)?;
    if database.session_count()? > 0 {
        return Err(ServerError::InvalidConfiguration {
            message: "native session encryption key is missing while durable sessions exist; restore the native credential or run `lfscloud sessions generate-key` to invalidate current sessions".to_owned(),
        });
    }

    let secret = generate_managed_session_key()?;
    key_store.store(&account, &secret)?;
    Ok(secret)
}

pub(crate) fn rotate_managed_session_key(
    database: &MetadataDatabase,
    key_store: &dyn SessionEncryptionKeyStore,
) -> ServerResult<usize> {
    let account = database.instance_id()?;
    let invalidated = database.delete_all_sessions()?;
    let secret = generate_managed_session_key()?;
    key_store.store(&account, &secret)?;
    Ok(invalidated)
}

fn generate_managed_session_key() -> ServerResult<Vec<u8>> {
    let mut secret = vec![0_u8; MANAGED_SESSION_KEY_BYTES];
    SystemRandom::new()
        .fill(&mut secret)
        .map_err(|_| ServerError::Internal {
            message: "operating-system randomness could not generate a session encryption key"
                .to_owned(),
        })?;
    Ok(secret)
}

fn native_key_store_error(error: keyring::Error) -> ServerError {
    ServerError::Internal {
        message: format!(
            "native credential store could not access the managed session encryption key: {error}; configure server.session_encryption_secret for a headless service"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
    };

    use crate::{
        GitHubPersonalAccessToken, LocalLfsSessionStore, MetadataDatabase, RepositoryUser,
    };

    use super::{
        NATIVE_KEY_SERVICE, NativeSessionEncryptionKeyStore, SessionEncryptionKeyStore,
        load_or_create_managed_session_key, rotate_managed_session_key,
    };

    struct NativeCredentialCleanup {
        entry: keyring::Entry,
    }

    impl Drop for NativeCredentialCleanup {
        fn drop(&mut self) {
            let _ = self.entry.delete_credential();
        }
    }

    #[derive(Default)]
    struct MemoryKeyStore {
        keys: Mutex<BTreeMap<String, Vec<u8>>>,
    }

    impl SessionEncryptionKeyStore for MemoryKeyStore {
        fn load(&self, account: &str) -> crate::ServerResult<Option<Vec<u8>>> {
            Ok(self
                .keys
                .lock()
                .expect("test key store should lock")
                .get(account)
                .cloned())
        }

        fn store(&self, account: &str, secret: &[u8]) -> crate::ServerResult<()> {
            self.keys
                .lock()
                .expect("test key store should lock")
                .insert(account.to_owned(), secret.to_vec());
            Ok(())
        }
    }

    fn user() -> RepositoryUser {
        RepositoryUser::new("github", "octocat", Some("1".to_owned()))
    }

    #[test]
    fn managed_key_is_generated_once_and_reused_for_the_database_identity() {
        let database = MetadataDatabase::open_in_memory().expect("metadata should open");
        let key_store = MemoryKeyStore::default();

        let first = load_or_create_managed_session_key(&database, &key_store)
            .expect("first run should generate and store a managed key");
        let second = load_or_create_managed_session_key(&database, &key_store)
            .expect("later runs should load the managed key");

        assert_eq!(first, second);
        assert_eq!(first.len(), 32);
        assert_eq!(
            key_store.keys.lock().expect("key store should lock").len(),
            1
        );
    }

    #[test]
    fn missing_native_key_never_silently_replaces_key_for_active_sessions() {
        let database = Arc::new(MetadataDatabase::open_in_memory().expect("metadata should open"));
        let original_store = MemoryKeyStore::default();
        let key = load_or_create_managed_session_key(&database, &original_store)
            .expect("initial managed key should be generated");
        LocalLfsSessionStore::open_durable(database.clone(), key)
            .expect("durable sessions should open")
            .issue_session_with_github_pat(
                &user(),
                ["repo"],
                GitHubPersonalAccessToken::from_secret("github-pat").expect("PAT should validate"),
            )
            .expect("active session should be stored");

        let missing_store = MemoryKeyStore::default();
        let error = load_or_create_managed_session_key(&database, &missing_store)
            .expect_err("a missing native key must not replace an active key");

        assert!(error.to_string().contains("sessions generate-key"));
        assert!(
            missing_store
                .keys
                .lock()
                .expect("key store should lock")
                .is_empty()
        );
    }

    #[test]
    fn rotation_replaces_the_managed_key_and_invalidates_all_sessions() {
        let database = Arc::new(MetadataDatabase::open_in_memory().expect("metadata should open"));
        let key_store = MemoryKeyStore::default();
        let old_key = load_or_create_managed_session_key(&database, &key_store)
            .expect("initial managed key should be generated");
        LocalLfsSessionStore::open_durable(database.clone(), &old_key)
            .expect("durable sessions should open")
            .issue_session_with_github_pat(
                &user(),
                ["repo"],
                GitHubPersonalAccessToken::from_secret("github-pat").expect("PAT should validate"),
            )
            .expect("active session should be stored");

        let invalidated = rotate_managed_session_key(&database, &key_store)
            .expect("managed key rotation should succeed");
        let new_key = load_or_create_managed_session_key(&database, &key_store)
            .expect("rotated managed key should load");

        assert_eq!(invalidated, 1);
        assert_ne!(new_key, old_key);
        LocalLfsSessionStore::open_durable(database, new_key)
            .expect("rotated key should open after invalidation");
    }

    #[test]
    #[ignore = "mutates the native credential store; requires LFS_CLOUD_RUN_NATIVE_KEYRING_SMOKE=1"]
    fn native_credential_store_generates_reloads_rotates_and_cleans_up() {
        let enabled = std::env::var("LFS_CLOUD_RUN_NATIVE_KEYRING_SMOKE")
            .is_ok_and(|value| matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes"));
        assert!(
            enabled,
            "set LFS_CLOUD_RUN_NATIVE_KEYRING_SMOKE=1 to mutate a disposable native credential"
        );

        let database = Arc::new(MetadataDatabase::open_in_memory().expect("metadata should open"));
        let account = database
            .instance_id()
            .expect("metadata installation identity should load");
        let cleanup = NativeCredentialCleanup {
            entry: keyring::Entry::new(NATIVE_KEY_SERVICE, &account)
                .expect("native credential entry should initialize"),
        };
        let key_store = NativeSessionEncryptionKeyStore;
        let old_key = load_or_create_managed_session_key(&database, &key_store)
            .expect("native store should accept a generated key");
        assert_eq!(
            load_or_create_managed_session_key(&database, &key_store)
                .expect("native store should reload the generated key"),
            old_key
        );
        LocalLfsSessionStore::open_durable(database.clone(), &old_key)
            .expect("durable sessions should open")
            .issue_session_with_github_pat(
                &user(),
                ["repo"],
                GitHubPersonalAccessToken::from_secret("github-pat").expect("PAT should validate"),
            )
            .expect("active session should be stored");

        let invalidated = rotate_managed_session_key(&database, &key_store)
            .expect("native key rotation should succeed");
        let new_key = load_or_create_managed_session_key(&database, &key_store)
            .expect("rotated native key should reload");

        assert_eq!(invalidated, 1);
        assert_ne!(new_key, old_key);
        cleanup
            .entry
            .delete_credential()
            .expect("disposable native credential should be deleted");
    }
}
