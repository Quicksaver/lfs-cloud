//! Worktree registry serialization and mutation.

use super::*;

/// Repository worktree registered as a consumer of the shared local cache.
///
/// `lfscloud gc` uses this kind of record to know which worktrees must be
/// inspected before deleting cached objects. Paths are required to be absolute
/// so the registry does not depend on a future process's current directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalCacheWorktreeRegistration {
    /// Stable repository mapping ID or provider-derived repository identity.
    pub repository_id: String,
    /// Absolute worktree root path.
    #[serde(
        serialize_with = "serialize_worktree_registry_path",
        deserialize_with = "deserialize_worktree_registry_path"
    )]
    pub worktree_root: PathBuf,
    /// Absolute Git directory path for the worktree.
    #[serde(
        serialize_with = "serialize_worktree_registry_path",
        deserialize_with = "deserialize_worktree_registry_path"
    )]
    pub git_dir: PathBuf,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum SerializedWorktreeRegistryPath {
    Legacy(PathBuf),
    Encoded { encoding: String, value: String },
}

#[derive(Serialize)]
struct EncodedWorktreeRegistryPath {
    encoding: &'static str,
    value: String,
}

fn serialize_worktree_registry_path<S>(path: &Path, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    #[cfg(unix)]
    let encoded = EncodedWorktreeRegistryPath {
        encoding: "unix_bytes_base64",
        value: BASE64_STANDARD_NO_PAD.encode(path.as_os_str().as_bytes()),
    };

    #[cfg(windows)]
    let encoded = {
        let wide_bytes = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        EncodedWorktreeRegistryPath {
            encoding: "windows_wide_base64",
            value: BASE64_STANDARD_NO_PAD.encode(wide_bytes),
        }
    };

    #[cfg(not(any(unix, windows)))]
    let encoded = EncodedWorktreeRegistryPath {
        encoding: "utf8",
        value: path
            .to_str()
            .ok_or_else(|| {
                serde::ser::Error::custom(
                    "worktree registry path cannot be represented as UTF-8 on this platform",
                )
            })?
            .to_owned(),
    };

    encoded.serialize(serializer)
}

fn deserialize_worktree_registry_path<'de, D>(deserializer: D) -> Result<PathBuf, D::Error>
where
    D: Deserializer<'de>,
{
    match SerializedWorktreeRegistryPath::deserialize(deserializer)? {
        SerializedWorktreeRegistryPath::Legacy(path) => Ok(path),
        SerializedWorktreeRegistryPath::Encoded { encoding, value } => {
            decode_worktree_registry_path::<D::Error>(&encoding, &value)
        }
    }
}

fn decode_worktree_registry_path<E>(encoding: &str, value: &str) -> Result<PathBuf, E>
where
    E: serde::de::Error,
{
    #[cfg(unix)]
    if encoding == "unix_bytes_base64" {
        let bytes = BASE64_STANDARD_NO_PAD
            .decode(value)
            .map_err(|_| E::custom("invalid base64 in Unix worktree registry path"))?;
        return Ok(PathBuf::from(OsString::from_vec(bytes)));
    }

    #[cfg(windows)]
    if encoding == "windows_wide_base64" {
        let bytes = BASE64_STANDARD_NO_PAD
            .decode(value)
            .map_err(|_| E::custom("invalid base64 in Windows worktree registry path"))?;
        let mut chunks = bytes.chunks_exact(2);
        let wide = chunks
            .by_ref()
            .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<_>>();
        if !chunks.remainder().is_empty() {
            return Err(E::custom(
                "Windows worktree registry path has an incomplete wide unit",
            ));
        }
        return Ok(PathBuf::from(OsString::from_wide(&wide)));
    }

    if encoding == "utf8" {
        return Ok(PathBuf::from(value));
    }

    Err(E::custom(format!(
        "unsupported worktree registry path encoding {encoding:?} on this platform"
    )))
}

impl LocalCacheWorktreeRegistration {
    /// Creates a validated worktree registration.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the repository identity is blank or
    /// either path is relative. Callers should resolve symlinks or Git-specific
    /// path forms before registration when that distinction matters.
    pub fn new(
        repository_id: impl Into<String>,
        worktree_root: impl Into<PathBuf>,
        git_dir: impl Into<PathBuf>,
    ) -> LocalCacheResult<Self> {
        let registration = Self {
            repository_id: repository_id.into(),
            worktree_root: worktree_root.into(),
            git_dir: git_dir.into(),
        };

        registration.validate()?;

        Ok(registration)
    }

    pub(super) fn validate(&self) -> LocalCacheResult<()> {
        let trimmed_repository_id = self.repository_id.trim();
        if trimmed_repository_id.is_empty() {
            return Err(LocalCacheError::InvalidWorktreeRegistration {
                field: "repository_id",
                message: "must not be blank".to_owned(),
            });
        }
        if trimmed_repository_id != self.repository_id {
            return Err(LocalCacheError::InvalidWorktreeRegistration {
                field: "repository_id",
                message: "must not be padded".to_owned(),
            });
        }
        validate_absolute_path("worktree_root", &self.worktree_root)?;
        validate_absolute_path("git_dir", &self.git_dir)?;

        Ok(())
    }
}

/// In-memory view of registered local cache worktrees.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalCacheWorktreeRegistry {
    version: u32,
    worktrees: Vec<LocalCacheWorktreeRegistration>,
}

impl LocalCacheWorktreeRegistry {
    /// Creates an empty registry using the current schema version.
    #[must_use]
    pub fn new() -> Self {
        Self {
            version: WORKTREE_REGISTRY_VERSION,
            worktrees: Vec::new(),
        }
    }

    /// Returns the registered worktrees in stable worktree-path order.
    #[must_use]
    pub fn worktrees(&self) -> &[LocalCacheWorktreeRegistration] {
        &self.worktrees
    }

    /// Returns whether the registry has no worktree registrations.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.worktrees.is_empty()
    }

    pub(super) fn validate_for_path(&self, path: &Path) -> LocalCacheResult<()> {
        if !(LEGACY_WORKTREE_REGISTRY_VERSION..=WORKTREE_REGISTRY_VERSION).contains(&self.version) {
            return Err(LocalCacheError::UnsupportedWorktreeRegistryVersion {
                path: path.to_path_buf(),
                version: self.version,
                supported_version: WORKTREE_REGISTRY_VERSION,
            });
        }

        let mut worktree_roots = BTreeSet::new();
        for registration in &self.worktrees {
            registration.validate()?;
            let key = normalized_path_key(&registration.worktree_root);
            if !worktree_roots.insert(key.clone()) {
                return Err(LocalCacheError::InvalidWorktreeRegistration {
                    field: "worktree_root",
                    message: format!(
                        "duplicate worktree root in registry: {}",
                        registration.worktree_root.display()
                    ),
                });
            }
        }

        Ok(())
    }

    pub(super) fn upsert(
        &mut self,
        registration: LocalCacheWorktreeRegistration,
    ) -> LocalCacheWorktreeRegistrationStatus {
        let key = normalized_path_key(&registration.worktree_root);
        if let Some(existing) = self
            .worktrees
            .iter_mut()
            .find(|existing| normalized_path_key(&existing.worktree_root) == key)
        {
            if *existing == registration {
                return LocalCacheWorktreeRegistrationStatus::Unchanged;
            }

            *existing = registration;
            self.sort();
            return LocalCacheWorktreeRegistrationStatus::Updated;
        }

        self.worktrees.push(registration);
        self.sort();
        LocalCacheWorktreeRegistrationStatus::Added
    }

    pub(super) fn remove(
        &mut self,
        worktree_root: &Path,
    ) -> Option<LocalCacheWorktreeRegistration> {
        let key = normalized_path_key(worktree_root);
        let index = self
            .worktrees
            .iter()
            .position(|registration| normalized_path_key(&registration.worktree_root) == key)?;

        Some(self.worktrees.remove(index))
    }

    pub(super) fn sort(&mut self) {
        self.worktrees
            .sort_by_cached_key(|registration| normalized_path_key(&registration.worktree_root));
    }
}

impl LocalCacheLayout {
    /// Loads registered worktrees from the local cache registry.
    ///
    /// A missing registry file is treated as an empty registry because older
    /// cache roots and fresh installs will not have one yet.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the registry cannot be read, decoded,
    /// or validated against the current schema.
    pub fn load_worktree_registry(&self) -> LocalCacheResult<LocalCacheWorktreeRegistry> {
        let path = self.worktree_registry_path();

        match File::open(&path) {
            Ok(file) => {
                let mut registry: LocalCacheWorktreeRegistry = serde_json::from_reader(file)
                    .map_err(|source| LocalCacheError::WorktreeRegistryJson {
                        context: "failed to decode local cache worktree registry",
                        path: path.clone(),
                        source,
                    })?;
                registry.validate_for_path(&path)?;
                // In-memory registries always use the latest schema so the
                // next mutation upgrades a legacy v1 file atomically.
                registry.version = WORKTREE_REGISTRY_VERSION;

                Ok(registry)
            }
            Err(source) if source.kind() == io::ErrorKind::NotFound => {
                Ok(LocalCacheWorktreeRegistry::new())
            }
            Err(source) => Err(LocalCacheError::Io {
                context: "failed to open local cache worktree registry",
                path,
                source,
            }),
        }
    }

    /// Registers or refreshes one worktree as a local cache consumer.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when the registration is invalid or the
    /// registry cannot be read or written.
    pub fn register_worktree(
        &self,
        registration: LocalCacheWorktreeRegistration,
    ) -> LocalCacheResult<LocalCacheWorktreeRegistrationChange> {
        registration.validate()?;

        let _lock = self.lock_worktree_registry()?;
        let mut registry = self.load_worktree_registry()?;
        let status = registry.upsert(registration.clone());

        if status != LocalCacheWorktreeRegistrationStatus::Unchanged {
            self.save_worktree_registry(&registry)?;
        }

        Ok(LocalCacheWorktreeRegistrationChange {
            registration,
            status,
        })
    }

    /// Removes one worktree from the local cache registry.
    ///
    /// This is intended for future explicit cleanup and for pruning worktrees
    /// that no longer exist before local cache garbage collection.
    ///
    /// # Errors
    ///
    /// Returns [`LocalCacheError`] when `worktree_root` is relative or the
    /// registry cannot be read or written.
    pub fn remove_worktree_registration(
        &self,
        worktree_root: impl AsRef<Path>,
    ) -> LocalCacheResult<Option<LocalCacheWorktreeRegistration>> {
        let worktree_root = worktree_root.as_ref();
        validate_absolute_path("worktree_root", worktree_root)?;

        let _lock = self.lock_worktree_registry()?;
        let mut registry = self.load_worktree_registry()?;
        let removed = registry.remove(worktree_root);

        if removed.is_some() {
            self.save_worktree_registry(&registry)?;
        }

        Ok(removed)
    }
}
impl Default for LocalCacheWorktreeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod registry_tests {
    use super::*;
    use crate::local_cache::test_support::*;
    #[test]
    fn worktree_registry_path_lives_under_cache_root() {
        let layout = LocalCacheLayout::new("/cache/root");

        assert_eq!(
            layout.worktree_registry_path(),
            PathBuf::from("/cache/root").join(LOCAL_CACHE_WORKTREES_FILE)
        );
    }

    #[test]
    fn missing_worktree_registry_loads_as_empty() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));

        let registry = layout
            .load_worktree_registry()
            .expect("missing registry should be empty");

        assert!(registry.is_empty());
        assert_eq!(registry.worktrees(), &[]);
    }

    #[test]
    fn register_worktree_writes_and_loads_stable_registry() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            temp.path().join("repo"),
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");

        let change = layout
            .register_worktree(registration.clone())
            .expect("worktree should register");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Added);
        assert_eq!(change.registration, registration);

        let registry = layout
            .load_worktree_registry()
            .expect("registry should reload");

        assert_eq!(registry.worktrees(), &[registration]);
        assert!(
            fs::read_to_string(layout.worktree_registry_path())
                .expect("registry should be readable")
                .contains("\"version\": 2")
        );
    }

    #[cfg(unix)]
    #[test]
    fn worktree_registry_round_trips_non_utf8_unix_paths() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let mut worktree_root = temp.path().as_os_str().as_bytes().to_vec();
        worktree_root.extend_from_slice(b"/repo-\xff");
        let worktree_root = PathBuf::from(OsString::from_vec(worktree_root));
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &worktree_root,
            worktree_root.join(".git"),
        )
        .expect("absolute non-UTF-8 registration should validate");

        layout
            .register_worktree(registration.clone())
            .expect("non-UTF-8 worktree should register");

        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            std::slice::from_ref(&registration)
        );
        assert_eq!(
            layout
                .remove_worktree_registration(&worktree_root)
                .expect("non-UTF-8 worktree should remove"),
            Some(registration)
        );
    }

    #[test]
    fn worktree_registry_loads_legacy_utf8_paths_and_upgrades_on_change() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let existing_root = temp.path().join("existing");
        let added_root = temp.path().join("added");
        let legacy_registry = serde_json::json!({
            "version": 1,
            "worktrees": [{
                "repository_id": "github-main:owner/existing",
                "worktree_root": existing_root,
                "git_dir": temp.path().join("existing/.git"),
            }],
        });
        write_file(
            &layout.worktree_registry_path(),
            &serde_json::to_vec_pretty(&legacy_registry)
                .expect("legacy registry fixture should encode"),
        );

        let loaded = layout
            .load_worktree_registry()
            .expect("legacy registry should load");
        assert_eq!(loaded.worktrees()[0].worktree_root, existing_root);

        layout
            .register_worktree(
                LocalCacheWorktreeRegistration::new(
                    "github-main:owner/added",
                    &added_root,
                    added_root.join(".git"),
                )
                .expect("added registration should validate"),
            )
            .expect("registry mutation should upgrade the legacy file");

        let upgraded: serde_json::Value = serde_json::from_slice(
            &fs::read(layout.worktree_registry_path()).expect("registry should be readable"),
        )
        .expect("upgraded registry should decode as JSON");
        assert_eq!(upgraded["version"], 2);
        assert_eq!(
            upgraded["worktrees"][0]["worktree_root"]["encoding"],
            if cfg!(unix) {
                "unix_bytes_base64"
            } else if cfg!(windows) {
                "windows_wide_base64"
            } else {
                "utf8"
            }
        );
    }

    #[test]
    fn register_worktree_updates_existing_path_without_duplicates() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let worktree_root = temp.path().join("repo");
        let first = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &worktree_root,
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");
        let updated = LocalCacheWorktreeRegistration::new(
            "github-main:owner/renamed",
            &worktree_root,
            temp.path().join("repo/.git/worktrees/main"),
        )
        .expect("absolute registration should validate");

        layout
            .register_worktree(first)
            .expect("first worktree should register");
        let change = layout
            .register_worktree(updated.clone())
            .expect("worktree should update");
        let unchanged = layout
            .register_worktree(updated.clone())
            .expect("identical worktree should remain unchanged");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Updated);
        assert_eq!(
            unchanged.status,
            LocalCacheWorktreeRegistrationStatus::Unchanged
        );
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            &[updated]
        );
    }

    #[test]
    fn register_worktree_waits_for_registry_lock() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        fs::create_dir_all(layout.root()).expect("cache root should be created");
        let lock_file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(false)
            .open(layout.worktree_registry_lock_path())
            .expect("registry lock file should open");
        FileExt::lock(&lock_file).expect("registry lock should be acquired by test");
        let contended_lock_file = OpenOptions::new()
            .write(true)
            .open(layout.worktree_registry_lock_path())
            .expect("contended registry lock file should open");
        assert!(
            FileExt::try_lock(&contended_lock_file).is_err(),
            "the platform should report the held registry lock as contended"
        );

        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            temp.path().join("repo"),
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");
        let thread_layout = layout.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx
                .send(())
                .expect("test should receive worker start");
            done_tx
                .send(thread_layout.register_worktree(registration))
                .expect("test should receive registration result");
        });

        started_rx
            .recv()
            .expect("worker should report before attempting registration");
        assert!(
            done_rx.recv_timeout(Duration::from_secs(1)).is_err(),
            "registration should wait while another process holds the registry lock"
        );

        drop(lock_file);
        let change = done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("registration should finish after lock release")
            .expect("worktree should register");
        worker.join().expect("worker should not panic");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Added);
    }

    #[cfg(unix)]
    #[test]
    fn register_and_remove_worktree_use_canonical_path_keys() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let real_worktree_root = temp.path().join("repo");
        let symlink_worktree_root = temp.path().join("repo-link");
        fs::create_dir_all(real_worktree_root.join(".git"))
            .expect("real worktree should be created");
        std::os::unix::fs::symlink(&real_worktree_root, &symlink_worktree_root)
            .expect("worktree symlink should be created");

        let first = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &symlink_worktree_root,
            symlink_worktree_root.join(".git"),
        )
        .expect("symlink registration should validate");
        let updated = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            &real_worktree_root,
            real_worktree_root.join(".git"),
        )
        .expect("real path registration should validate");

        layout
            .register_worktree(first)
            .expect("first worktree should register");
        let change = layout
            .register_worktree(updated.clone())
            .expect("canonical worktree key should update existing record");

        assert_eq!(change.status, LocalCacheWorktreeRegistrationStatus::Updated);
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees(),
            std::slice::from_ref(&updated)
        );
        assert_eq!(
            layout
                .remove_worktree_registration(&symlink_worktree_root)
                .expect("canonical worktree key should remove existing record"),
            Some(updated)
        );
    }

    #[test]
    fn remove_worktree_registration_deletes_matching_absolute_path() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/repo",
            temp.path().join("repo"),
            temp.path().join("repo/.git"),
        )
        .expect("absolute registration should validate");
        layout
            .register_worktree(registration.clone())
            .expect("worktree should register");

        let removed = layout
            .remove_worktree_registration(&registration.worktree_root)
            .expect("worktree should remove");

        assert_eq!(removed, Some(registration));
        assert!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .is_empty()
        );
    }

    #[test]
    fn worktree_registration_rejects_relative_paths() {
        let error =
            LocalCacheWorktreeRegistration::new("github-main:owner/repo", "repo", "/repo/.git")
                .expect_err("relative worktree root should fail");

        assert!(matches!(
            error,
            LocalCacheError::InvalidWorktreeRegistration {
                field: "worktree_root",
                ..
            }
        ));
    }

    #[test]
    fn worktree_registry_rejects_future_schema_version() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        write_file(
            &layout.worktree_registry_path(),
            br#"{"version": 999, "worktrees": []}"#,
        );

        let error = layout
            .load_worktree_registry()
            .expect_err("future registry version should fail");

        assert!(matches!(
            error,
            LocalCacheError::UnsupportedWorktreeRegistryVersion { version: 999, .. }
        ));
    }

    #[test]
    fn worktree_registry_rejects_duplicate_registered_roots() {
        let temp = tempfile::tempdir().expect("temp dir should be created");
        let layout = LocalCacheLayout::new(temp.path().join("cache"));
        let worktree_root = temp.path().join("repo");
        let first_git_dir = temp.path().join("repo/.git");
        let second_git_dir = temp.path().join("repo/.git/worktrees/duplicate");
        let registry = LocalCacheWorktreeRegistry {
            version: WORKTREE_REGISTRY_VERSION,
            worktrees: vec![
                LocalCacheWorktreeRegistration {
                    repository_id: "github-main:owner/repo".to_owned(),
                    worktree_root: worktree_root.clone(),
                    git_dir: first_git_dir,
                },
                LocalCacheWorktreeRegistration {
                    repository_id: "github-main:owner/repo".to_owned(),
                    worktree_root,
                    git_dir: second_git_dir,
                },
            ],
        };
        write_file(
            &layout.worktree_registry_path(),
            &serde_json::to_vec_pretty(&registry).expect("registry fixture should encode"),
        );

        let error = layout
            .load_worktree_registry()
            .expect_err("duplicate registry roots should fail");

        assert!(matches!(
            error,
            LocalCacheError::InvalidWorktreeRegistration {
                field: "worktree_root",
                ..
            }
        ));
    }
}
