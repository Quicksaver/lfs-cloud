/// Deterministic Google Drive address metadata for one LFS object.
///
/// The display path is an inspection and cleanup convention under the
/// configured Drive root. Lookups still verify private Drive app properties so
/// the later SQLite metadata database can remain the ownership source of truth.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GoogleDriveObjectKey {
    repo_namespace: String,
    object: LfsObject,
}

impl GoogleDriveObjectKey {
    /// Creates Drive object-addressing metadata for a repository-scoped object.
    ///
    /// # Errors
    ///
    /// Returns [`StorageError`] if the repository namespace is blank or
    /// contains control characters that cannot be rendered safely.
    pub fn new(repo_namespace: impl AsRef<str>, object: LfsObject) -> StorageResult<Self> {
        Ok(Self {
            repo_namespace: validate_repo_namespace(repo_namespace.as_ref())?,
            object,
        })
    }

    /// Returns the repository namespace associated with this object.
    #[must_use]
    pub fn repo_namespace(&self) -> &str {
        &self.repo_namespace
    }

    /// Returns the provider-independent object identity.
    #[must_use]
    pub fn object(&self) -> &LfsObject {
        &self.object
    }

    /// Returns the deterministic Drive file name for this LFS object.
    #[must_use]
    pub fn file_name(&self) -> String {
        format!(
            "sha256-{}-{}.lfs",
            self.object.oid.as_hex(),
            self.object.size.bytes()
        )
    }

    /// Returns the deterministic Drive folder name for this object's shard.
    #[must_use]
    fn shard_folder_name(&self) -> String {
        format!("lfscloud-sha256-{}", self.shard_prefix())
    }

    /// Returns the human-readable object path below the configured Drive root.
    ///
    /// Google Drive addresses files by ID, not POSIX paths. This value is a
    /// deterministic convention for upload placement and operator inspection.
    #[must_use]
    pub fn display_path(&self) -> String {
        format!("{}/{}", self.shard_folder_name(), self.file_name())
    }

    fn expected_app_properties(&self) -> GoogleDriveObjectProperties {
        GoogleDriveObjectProperties {
            repo_namespace: GoogleDriveRepositoryNamespaceProperty::new(&self.repo_namespace),
            oid: self.object.oid.as_hex().to_owned(),
            size: self.object.size.bytes().to_string(),
        }
    }

    fn shard_prefix(&self) -> &str {
        &self.object.oid.as_hex()[..2]
    }
}

enum GoogleDriveRepositoryNamespaceProperty {
    Raw(String),
    Sha256(String),
}

impl GoogleDriveRepositoryNamespaceProperty {
    fn new(repo_namespace: &str) -> Self {
        // Preserve the original value for existing objects whenever Drive can
        // represent it. Oversized values need a tagged digest so a raw
        // namespace that resembles a digest cannot alias another repository.
        if GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len() + repo_namespace.len()
            <= MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
        {
            Self::Raw(repo_namespace.to_owned())
        } else {
            Self::Sha256(format!("{:x}", Sha256::digest(repo_namespace.as_bytes())))
        }
    }

    fn value(&self) -> &str {
        match self {
            Self::Raw(value) | Self::Sha256(value) => value,
        }
    }

    fn format(&self) -> Option<&'static str> {
        match self {
            Self::Raw(_) => None,
            Self::Sha256(_) => Some(GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT),
        }
    }
}

struct GoogleDriveObjectProperties {
    repo_namespace: GoogleDriveRepositoryNamespaceProperty,
    oid: String,
    size: String,
}

impl GoogleDriveObjectProperties {
    fn pairs(&self) -> Vec<(&'static str, &str)> {
        let mut pairs = vec![
            (
                GOOGLE_DRIVE_OBJECT_VERSION_PROPERTY,
                GOOGLE_DRIVE_OBJECT_VERSION,
            ),
            (
                GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY,
                self.repo_namespace.value(),
            ),
            (GOOGLE_DRIVE_OBJECT_OID_PROPERTY, &self.oid),
            (GOOGLE_DRIVE_OBJECT_SIZE_PROPERTY, &self.size),
        ];
        if let Some(format) = self.repo_namespace.format() {
            pairs.push((GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY, format));
        }
        pairs
    }
}

fn drive_upload_metadata(root_folder_id: &str, key: &GoogleDriveObjectKey) -> serde_json::Value {
    let app_properties = key
        .expected_app_properties()
        .pairs()
        .into_iter()
        .map(|(property, value)| (property, value.to_owned()))
        .collect::<BTreeMap<_, _>>();

    serde_json::json!({
        "name": key.file_name(),
        "parents": [root_folder_id],
        "appProperties": app_properties,
    })
}

fn drive_shard_folder_metadata(
    root_folder_id: &str,
    key: &GoogleDriveObjectKey,
) -> serde_json::Value {
    serde_json::json!({
        "name": key.shard_folder_name(),
        "mimeType": GOOGLE_DRIVE_FOLDER_MIME_TYPE,
        "parents": [root_folder_id],
        "appProperties": {
            (GOOGLE_DRIVE_SHARD_KIND_PROPERTY): GOOGLE_DRIVE_SHARD_KIND,
            (GOOGLE_DRIVE_SHARD_PREFIX_PROPERTY): key.shard_prefix(),
        },
    })
}

fn drive_object_lookup_query(
    root_folder_id: &str,
    key: &GoogleDriveObjectKey,
    expected_properties: &GoogleDriveObjectProperties,
) -> String {
    let mut query = format!(
        "'{}' in parents and trashed = false and name = '{}'",
        escape_drive_query_string(root_folder_id),
        escape_drive_query_string(&key.file_name())
    );

    for (property, value) in expected_properties.pairs() {
        query.push_str(&format!(
            " and appProperties has {{ key='{}' and value='{}' }}",
            escape_drive_query_string(property),
            escape_drive_query_string(value)
        ));
    }

    query
}

fn drive_shard_folder_lookup_query(root_folder_id: &str, key: &GoogleDriveObjectKey) -> String {
    format!(
        "'{}' in parents and trashed = false and name = '{}' and mimeType = '{}' and appProperties has {{ key='{}' and value='{}' }} and appProperties has {{ key='{}' and value='{}' }}",
        escape_drive_query_string(root_folder_id),
        escape_drive_query_string(&key.shard_folder_name()),
        GOOGLE_DRIVE_FOLDER_MIME_TYPE,
        GOOGLE_DRIVE_SHARD_KIND_PROPERTY,
        GOOGLE_DRIVE_SHARD_KIND,
        GOOGLE_DRIVE_SHARD_PREFIX_PROPERTY,
        key.shard_prefix(),
    )
}

fn validate_repo_namespace(value: &str) -> StorageResult<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(drive_upstream_error(
            "google_drive",
            "repository namespace must not be blank",
        ));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(drive_upstream_error(
            "google_drive",
            "repository namespace must not contain control characters",
        ));
    }

    Ok(trimmed.to_owned())
}

fn escape_drive_query_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\'' | '\\' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
}


#[cfg(test)]
pub(super) mod object_key_tests {
    use super::*;

    #[test]
    fn drive_object_key_defines_stable_display_path_and_file_name() {
        let key = GoogleDriveObjectKey::new("github.com/Owner Repo/repo.git", lfs_object())
            .expect("object key should build");

        assert_eq!(key.repo_namespace(), "github.com/Owner Repo/repo.git");
        assert_eq!(key.object(), &lfs_object());
        assert_eq!(key.file_name(), format!("sha256-{OBJECT_OID}-42.lfs"));
        assert_eq!(key.shard_folder_name(), "lfscloud-sha256-aa");
        assert_eq!(
            key.display_path(),
            format!("lfscloud-sha256-aa/sha256-{OBJECT_OID}-42.lfs")
        );
    }

    #[test]
    fn drive_object_properties_preserve_namespace_at_property_byte_limit() {
        let namespace_len = super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
            - super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len();
        let namespace = "r".repeat(namespace_len);
        let key = GoogleDriveObjectKey::new(&namespace, lfs_object())
            .expect("maximum raw namespace should build");
        let properties = key.expected_app_properties();
        let pairs = properties.pairs();

        assert!(pairs.contains(&(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY,
            namespace.as_str()
        )));
        assert!(
            !pairs
                .iter()
                .any(|(key, _)| *key == super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY)
        );
        assert!(pairs.iter().all(|(key, value)| {
            key.len() + value.len() <= super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
        }));
    }

    #[test]
    fn drive_object_properties_digest_oversized_namespace() {
        let namespace_byte_limit = super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
            - super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len();
        let namespace = format!("{}é", "r".repeat(namespace_byte_limit - 1));
        assert_eq!(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY.len() + namespace.len(),
            super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES + 1
        );
        let expected_digest = format!("{:x}", Sha256::digest(namespace.as_bytes()));
        let key = GoogleDriveObjectKey::new(&namespace, lfs_object())
            .expect("oversized raw namespace should build with digest metadata");
        let properties = key.expected_app_properties();
        let pairs = properties.pairs();

        assert!(pairs.contains(&(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY,
            expected_digest.as_str()
        )));
        assert!(pairs.contains(&(
            super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY,
            super::GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT
        )));
        assert!(pairs.iter().all(|(key, value)| {
            key.len() + value.len() <= super::MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES
        }));

        let metadata = super::drive_upload_metadata("drive-root", &key);
        assert_eq!(
            metadata["appProperties"][super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY],
            expected_digest
        );
        assert_eq!(
            metadata["appProperties"][super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY],
            super::GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT
        );

        let query = super::drive_object_lookup_query("drive-root", &key, &properties);
        assert!(query.contains(&format!(
            "appProperties has {{ key='{}' and value='{expected_digest}' }}",
            super::GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY
        )));
        assert!(query.contains(&format!(
            "appProperties has {{ key='{}' and value='{}' }}",
            super::GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY,
            super::GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT
        )));
    }

    #[test]
    fn drive_object_key_rejects_blank_namespace() {
        let error = GoogleDriveObjectKey::new(" \t\n", lfs_object())
            .expect_err("blank namespace should fail");

        assert!(
            error
                .to_string()
                .contains("repository namespace must not be blank")
        );
    }

    #[test]
    fn drive_object_key_rejects_control_characters_in_namespace() {
        let error = GoogleDriveObjectKey::new("github.com/owner\nrepo", lfs_object())
            .expect_err("control character should fail");

        assert!(
            error
                .to_string()
                .contains("repository namespace must not contain control characters")
        );
    }

}
