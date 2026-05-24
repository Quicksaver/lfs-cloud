//! Shared Git LFS object and batch-protocol types.
//!
//! These types model the provider-independent parts of Git LFS: object
//! identity, pointer-file metadata, and the batch request/response payloads
//! exchanged with Git LFS clients.

use std::{collections::BTreeMap, fmt, num::ParseIntError, str::FromStr};

use serde::{Deserialize, Serialize};

/// Git LFS pointer file version supported by this package.
pub const LFS_POINTER_VERSION: &str = "https://git-lfs.github.com/spec/v1";

const SHA256_PREFIX: &str = "sha256:";
const SHA256_HEX_LENGTH: usize = 64;

/// Error returned when Git LFS object or pointer metadata is invalid.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LfsObjectError {
    /// The object identifier was empty.
    #[error("LFS object identifier is empty")]
    EmptyOid,

    /// The object identifier used a hash algorithm other than SHA-256.
    #[error("unsupported LFS object hash algorithm: {algorithm}")]
    UnsupportedOidAlgorithm {
        /// Hash algorithm prefix found before the colon.
        algorithm: String,
    },

    /// The SHA-256 object identifier had the wrong number of hexadecimal bytes.
    #[error("invalid SHA-256 object identifier length: expected 64 hex characters, got {actual}")]
    InvalidSha256Length {
        /// Number of characters supplied.
        actual: usize,
    },

    /// The SHA-256 object identifier contained a non-hexadecimal character.
    #[error("invalid SHA-256 object identifier: non-hex character at byte {index}")]
    InvalidSha256Hex {
        /// Byte index of the first invalid character.
        index: usize,
    },

    /// The pointer file was missing the required version line.
    #[error("LFS pointer is missing the version line")]
    PointerMissingVersion,

    /// The pointer file used an unsupported version URL.
    #[error("unsupported LFS pointer version: {version}")]
    PointerInvalidVersion {
        /// Version URL found in the pointer file.
        version: String,
    },

    /// The pointer file was missing the required object identifier line.
    #[error("LFS pointer is missing the oid line")]
    PointerMissingOid,

    /// The pointer file object identifier did not use the required `sha256:` prefix.
    #[error("LFS pointer oid is missing the sha256: prefix: {oid}")]
    PointerOidMissingSha256Prefix {
        /// Object identifier text found after the `oid` key.
        oid: String,
    },

    /// The pointer file was missing the required size line.
    #[error("LFS pointer is missing the size line")]
    PointerMissingSize,

    /// The pointer file size value was not a valid unsigned integer.
    #[error("invalid LFS pointer size {value:?}: {source}")]
    PointerInvalidSize {
        /// Text found after the `size` key.
        value: String,
        /// Integer parsing failure.
        #[source]
        source: ParseIntError,
    },

    /// The pointer file included an unsupported line.
    #[error("unexpected LFS pointer line: {line}")]
    PointerUnexpectedLine {
        /// Unsupported pointer line.
        line: String,
    },

    /// The pointer extension key used a reserved name or invalid character.
    #[error("invalid LFS pointer extension key: {key}")]
    PointerInvalidExtensionKey {
        /// Extension key that could not be safely rendered in a pointer file.
        key: String,
    },

    /// The pointer extension value was not a valid pointer OID.
    #[error("invalid LFS pointer extension value for {key}: expected sha256 object id")]
    PointerInvalidExtensionValue {
        /// Extension key associated with the invalid value.
        key: String,
    },
}

/// A validated SHA-256 Git LFS object identifier without the `sha256:` prefix.
///
/// # Examples
///
/// ```
/// use std::str::FromStr;
///
/// use lfs_cloud::LfsOid;
///
/// let oid = LfsOid::from_str("sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")?;
/// assert_eq!(oid.as_hex(), "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
/// # Ok::<(), lfs_cloud::LfsObjectError>(())
/// ```
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LfsOid(String);

impl LfsOid {
    /// Validates a SHA-256 object identifier.
    ///
    /// The input may be either the raw 64-character hex digest or the pointer
    /// form prefixed with `sha256:`.
    pub fn new(value: impl AsRef<str>) -> Result<Self, LfsObjectError> {
        let value = value.as_ref().trim();

        if value.is_empty() {
            return Err(LfsObjectError::EmptyOid);
        }

        let hex = match value.split_once(':') {
            Some(("sha256", hex)) => hex,
            Some((algorithm, _)) => {
                return Err(LfsObjectError::UnsupportedOidAlgorithm {
                    algorithm: algorithm.to_owned(),
                });
            }
            None => value,
        };

        Self::from_sha256_hex(hex)
    }

    fn from_sha256_hex(hex: &str) -> Result<Self, LfsObjectError> {
        if hex.len() != SHA256_HEX_LENGTH {
            return Err(LfsObjectError::InvalidSha256Length { actual: hex.len() });
        }

        if let Some((index, _)) = hex
            .bytes()
            .enumerate()
            .find(|(_, byte)| !byte.is_ascii_hexdigit())
        {
            return Err(LfsObjectError::InvalidSha256Hex { index });
        }

        Ok(Self(hex.to_ascii_lowercase()))
    }

    /// Returns the raw 64-character SHA-256 hex digest.
    #[must_use]
    pub fn as_hex(&self) -> &str {
        &self.0
    }

    /// Returns the pointer-file form, including the `sha256:` prefix.
    #[must_use]
    pub fn as_pointer_oid(&self) -> String {
        format!("{SHA256_PREFIX}{}", self.0)
    }
}

impl fmt::Display for LfsOid {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_hex())
    }
}

impl FromStr for LfsOid {
    type Err = LfsObjectError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::new(value)
    }
}

impl Serialize for LfsOid {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(self.as_hex())
    }
}

impl<'de> Deserialize<'de> for LfsOid {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Exact size in bytes of a Git LFS object.
///
/// # Examples
///
/// ```
/// use lfs_cloud::LfsObjectSize;
///
/// let size = LfsObjectSize::new(42);
/// assert_eq!(size.bytes(), 42);
/// ```
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct LfsObjectSize(u64);

impl LfsObjectSize {
    /// Creates an object size from an exact byte count.
    #[must_use]
    pub fn new(bytes: u64) -> Self {
        Self(bytes)
    }

    /// Returns the exact byte count.
    #[must_use]
    pub fn bytes(self) -> u64 {
        self.0
    }
}

impl fmt::Display for LfsObjectSize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

/// Provider-independent identity of a Git LFS object.
#[derive(Clone, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
pub struct LfsObject {
    /// SHA-256 object identifier.
    pub oid: LfsOid,
    /// Exact object size in bytes.
    pub size: LfsObjectSize,
}

impl LfsObject {
    /// Creates object identity metadata from a validated OID and byte size.
    #[must_use]
    pub fn new(oid: LfsOid, size: LfsObjectSize) -> Self {
        Self { oid, size }
    }
}

/// Parsed Git LFS pointer-file metadata.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LfsPointer {
    /// Pointer version URL.
    pub version: &'static str,
    /// Extension records keyed by pointer key.
    extensions: BTreeMap<String, String>,
    /// Object referenced by the pointer file.
    pub object: LfsObject,
}

impl LfsPointer {
    /// Creates a pointer for the supported Git LFS pointer version.
    #[must_use]
    pub fn new(object: LfsObject) -> Self {
        Self {
            version: LFS_POINTER_VERSION,
            extensions: BTreeMap::new(),
            object,
        }
    }

    /// Returns extension pointer records preserved from the pointer file.
    #[must_use]
    pub fn extensions(&self) -> &BTreeMap<String, String> {
        &self.extensions
    }

    /// Inserts an extension pointer record.
    ///
    /// The `version`, `oid`, and `size` keys are owned by the core pointer
    /// fields. Extension keys and values must use Git LFS extension syntax so
    /// the pointer can be rendered back into its canonical sorted form.
    pub fn insert_extension(
        &mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Result<(), LfsObjectError> {
        let key = key.into();
        let value = value.into();

        validate_extension_key(&key)?;
        let value = normalize_extension_value(&key, &value)?;

        self.extensions.insert(key, value);
        Ok(())
    }

    /// Parses a Git LFS pointer file.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::LfsPointer;
    ///
    /// let pointer = LfsPointer::parse(
    ///     "version https://git-lfs.github.com/spec/v1\n\
    ///      oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
    ///      size 42\n",
    /// )?;
    ///
    /// assert_eq!(pointer.object.size.bytes(), 42);
    /// # Ok::<(), lfs_cloud::LfsObjectError>(())
    /// ```
    pub fn parse(contents: &str) -> Result<Self, LfsObjectError> {
        let mut lines = contents.lines().filter(|line| !line.trim().is_empty());

        let version_line = lines.next().ok_or(LfsObjectError::PointerMissingVersion)?;
        let version = version_line.strip_prefix("version ").ok_or_else(|| {
            LfsObjectError::PointerUnexpectedLine {
                line: version_line.to_owned(),
            }
        })?;

        match version {
            LFS_POINTER_VERSION => {}
            version => {
                return Err(LfsObjectError::PointerInvalidVersion {
                    version: version.to_owned(),
                });
            }
        }

        let mut previous_key = None;
        let mut extensions = BTreeMap::new();
        let mut oid = None;
        let mut size = None;

        for line in lines {
            let (key, value) =
                line.split_once(' ')
                    .ok_or_else(|| LfsObjectError::PointerUnexpectedLine {
                        line: line.to_owned(),
                    })?;

            if previous_key
                .as_deref()
                .is_some_and(|previous| key <= previous)
            {
                return Err(LfsObjectError::PointerUnexpectedLine {
                    line: line.to_owned(),
                });
            }
            previous_key = Some(key.to_owned());

            match key {
                "oid" => {
                    oid = Some(Self::parse_pointer_oid(value)?);
                }
                "size" => {
                    let parsed_size = value.parse::<u64>().map_err(|source| {
                        LfsObjectError::PointerInvalidSize {
                            value: value.to_owned(),
                            source,
                        }
                    })?;
                    size = Some(LfsObjectSize::new(parsed_size));
                }
                extension_key if is_valid_extension_key(extension_key) => {
                    let extension_value = normalize_extension_value(extension_key, value)?;
                    extensions.insert(extension_key.to_owned(), extension_value);
                }
                _ => {
                    return Err(LfsObjectError::PointerUnexpectedLine {
                        line: line.to_owned(),
                    });
                }
            }
        }

        Ok(Self {
            version: LFS_POINTER_VERSION,
            extensions,
            object: LfsObject::new(
                oid.ok_or(LfsObjectError::PointerMissingOid)?,
                size.ok_or(LfsObjectError::PointerMissingSize)?,
            ),
        })
    }

    /// Renders this pointer in the canonical Git LFS form.
    #[must_use]
    pub fn to_pointer_file(&self) -> String {
        let mut pointer = format!("version {}\n", self.version);
        let mut fields = self.extensions.clone();

        fields.insert("oid".to_owned(), self.object.oid.as_pointer_oid());
        fields.insert("size".to_owned(), self.object.size.to_string());

        for (key, value) in fields {
            pointer.push_str(&format!("{key} {value}\n"));
        }
        pointer
    }

    fn parse_pointer_oid(value: &str) -> Result<LfsOid, LfsObjectError> {
        let oid_hex = value.strip_prefix(SHA256_PREFIX).ok_or_else(|| {
            LfsObjectError::PointerOidMissingSha256Prefix {
                oid: value.to_owned(),
            }
        })?;

        LfsOid::from_sha256_hex(oid_hex)
    }
}

fn validate_extension_key(key: &str) -> Result<(), LfsObjectError> {
    if is_valid_extension_key(key) {
        Ok(())
    } else {
        Err(LfsObjectError::PointerInvalidExtensionKey {
            key: key.to_owned(),
        })
    }
}

fn is_valid_extension_key(key: &str) -> bool {
    let Some(extension_name) = key.strip_prefix("ext-") else {
        return false;
    };
    let Some((priority, name)) = extension_name.split_once('-') else {
        return false;
    };

    let starts_with_name_char = name
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_');

    priority.len() == 1
        && priority.bytes().all(|byte| byte.is_ascii_digit())
        && starts_with_name_char
        && !name.bytes().any(|byte| byte.is_ascii_whitespace())
}

fn normalize_extension_value(key: &str, value: &str) -> Result<String, LfsObjectError> {
    if value.contains('\r') || value.contains('\n') {
        return Err(LfsObjectError::PointerInvalidExtensionValue {
            key: key.to_owned(),
        });
    }

    let Some(oid_hex) = value.strip_prefix(SHA256_PREFIX) else {
        return Err(LfsObjectError::PointerInvalidExtensionValue {
            key: key.to_owned(),
        });
    };

    LfsOid::from_sha256_hex(oid_hex)
        .map(|oid| oid.as_pointer_oid())
        .map_err(|_| LfsObjectError::PointerInvalidExtensionValue {
            key: key.to_owned(),
        })
}

impl fmt::Display for LfsPointer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.to_pointer_file())
    }
}

impl FromStr for LfsPointer {
    type Err = LfsObjectError;

    fn from_str(contents: &str) -> Result<Self, Self::Err> {
        Self::parse(contents)
    }
}

/// Git LFS batch operation requested by the client.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LfsBatchOperation {
    /// Request download actions for the listed objects.
    Download,
    /// Request upload actions for the listed objects.
    Upload,
}

/// Optional Git ref context supplied in a batch request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LfsBatchRef {
    /// Fully qualified ref name, such as `refs/heads/main`.
    pub name: String,
}

/// Git LFS batch request payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LfsBatchRequest {
    /// Requested transfer operation.
    pub operation: LfsBatchOperation,
    /// Transfer adapters supported by the client.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub transfers: Vec<String>,
    /// Optional ref context for future ref-aware authorization.
    #[serde(default, rename = "ref", skip_serializing_if = "Option::is_none")]
    pub ref_context: Option<LfsBatchRef>,
    /// Objects included in this batch request.
    pub objects: Vec<LfsObject>,
}

/// Error returned when a Git LFS batch request body cannot be parsed.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum LfsBatchRequestParseError {
    /// The request body was not valid Git LFS batch JSON.
    #[error("invalid Git LFS batch request JSON: {source}")]
    Json {
        /// Underlying JSON or typed deserialization failure.
        #[source]
        source: serde_json::Error,
    },
}

/// Parses a Git LFS batch API request body.
///
/// The Git LFS batch endpoint accepts JSON whose `operation` is `download` or
/// `upload`, whose objects carry SHA-256 OIDs and exact sizes, and whose
/// optional `ref` field is preserved for later authorization decisions.
///
/// # Errors
///
/// Returns [`LfsBatchRequestParseError`] when the body is not valid JSON or
/// does not match the typed Git LFS batch request shape.
///
/// # Examples
///
/// ```
/// use lfs_cloud::{LfsBatchOperation, parse_lfs_batch_request_json};
///
/// let request = parse_lfs_batch_request_json(
///     br#"{
///       "operation": "download",
///       "transfers": ["basic"],
///       "objects": [
///         {
///           "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
///           "size": 42
///         }
///       ]
///     }"#,
/// )?;
///
/// assert_eq!(request.operation, LfsBatchOperation::Download);
/// assert_eq!(request.objects[0].size.bytes(), 42);
/// # Ok::<(), lfs_cloud::LfsBatchRequestParseError>(())
/// ```
pub fn parse_lfs_batch_request_json(
    body: impl AsRef<[u8]>,
) -> Result<LfsBatchRequest, LfsBatchRequestParseError> {
    serde_json::from_slice(body.as_ref())
        .map_err(|source| LfsBatchRequestParseError::Json { source })
}

/// Git LFS batch response payload.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LfsBatchResponse {
    /// Transfer adapter selected by the server.
    pub transfer: String,
    /// Per-object batch results.
    pub objects: Vec<LfsBatchObjectResponse>,
}

/// Per-object result in a Git LFS batch response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LfsBatchObjectResponse {
    /// SHA-256 object identifier.
    pub oid: LfsOid,
    /// Exact object size in bytes.
    pub size: LfsObjectSize,
    /// Whether the returned actions require authentication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authenticated: Option<bool>,
    /// Upload/download/verify actions keyed by action name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub actions: BTreeMap<String, LfsBatchAction>,
    /// Object-level error returned instead of actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<LfsBatchObjectError>,
}

impl LfsBatchObjectResponse {
    /// Creates an action-bearing object response.
    #[must_use]
    pub fn with_actions(
        object: &LfsObject,
        authenticated: bool,
        actions: BTreeMap<String, LfsBatchAction>,
    ) -> Self {
        Self {
            oid: object.oid.clone(),
            size: object.size,
            authenticated: Some(authenticated),
            actions,
            error: None,
        }
    }

    /// Creates an object-level error response.
    #[must_use]
    pub fn with_error(object: &LfsObject, error: LfsBatchObjectError) -> Self {
        Self {
            oid: object.oid.clone(),
            size: object.size,
            authenticated: None,
            actions: BTreeMap::new(),
            error: Some(error),
        }
    }
}

/// HTTP action advertised to a Git LFS client.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LfsBatchAction {
    /// URL the client should call for this action.
    pub href: String,
    /// Additional HTTP headers required for the action.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub header: BTreeMap<String, String>,
    /// RFC 3339 expiration timestamp, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<String>,
    /// Expiration lifetime in seconds, if available.
    ///
    /// The Git LFS batch API allows negative values to represent an already
    /// expired action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<i64>,
}

impl LfsBatchAction {
    /// Creates an action with no extra headers or expiration metadata.
    #[must_use]
    pub fn new(href: impl Into<String>) -> Self {
        Self {
            href: href.into(),
            header: BTreeMap::new(),
            expires_at: None,
            expires_in: None,
        }
    }
}

/// Git LFS object-level error response.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LfsBatchObjectError {
    /// HTTP-like status code for the object-level failure.
    pub code: u16,
    /// Human-readable error message safe to show to the Git LFS client.
    pub message: String,
}

impl LfsBatchObjectError {
    /// Creates an object-level error payload.
    #[must_use]
    pub fn new(code: u16, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::{
        LfsBatchObjectError, LfsBatchObjectResponse, LfsBatchOperation, LfsObject, LfsObjectError,
        LfsObjectSize, LfsOid, LfsPointer, parse_lfs_batch_request_json,
    };

    const OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn oid_accepts_raw_or_prefixed_sha256_and_normalizes_case() {
        let raw = LfsOid::from_str(OID).expect("raw sha256 oid should parse");
        let prefixed = LfsOid::from_str(
            "sha256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
        )
        .expect("prefixed sha256 oid should parse");

        assert_eq!(raw, prefixed);
        assert_eq!(prefixed.as_hex(), OID);
        assert_eq!(prefixed.as_pointer_oid(), format!("sha256:{OID}"));
    }

    #[test]
    fn oid_rejects_invalid_digest_text() {
        let short = LfsOid::from_str("abc").expect_err("short oid should fail");
        let wrong_algorithm =
            LfsOid::from_str(&format!("sha1:{OID}")).expect_err("wrong algorithm should fail");
        let non_hex =
            LfsOid::from_str("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaz")
                .expect_err("non-hex oid should fail");

        assert!(matches!(
            short,
            LfsObjectError::InvalidSha256Length { actual: 3 }
        ));
        assert!(matches!(
            wrong_algorithm,
            LfsObjectError::UnsupportedOidAlgorithm { .. }
        ));
        assert!(matches!(
            non_hex,
            LfsObjectError::InvalidSha256Hex { index: 63 }
        ));
    }

    #[test]
    fn pointer_parses_and_renders_canonical_metadata() {
        let contents = "version https://git-lfs.github.com/spec/v1\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";

        let pointer = LfsPointer::parse(contents).expect("pointer should parse");

        assert_eq!(pointer.object.oid.as_hex(), OID);
        assert_eq!(pointer.object.size.bytes(), 42);
        assert_eq!(pointer.to_pointer_file(), contents);
    }

    #[test]
    fn pointer_parses_and_renders_extension_records() {
        let contents = "version https://git-lfs.github.com/spec/v1\n\
             ext-0-foo sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";

        let pointer = LfsPointer::parse(contents).expect("extended pointer should parse");

        assert_eq!(pointer.object.oid.as_hex(), OID);
        assert_eq!(
            pointer.extensions().get("ext-0-foo").map(String::as_str),
            Some("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(pointer.to_pointer_file(), contents);
    }

    #[test]
    fn pointer_accepts_git_lfs_extension_key_characters() {
        let contents = "version https://git-lfs.github.com/spec/v1\n\
             ext-0-Foo_bar/path+v1 sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";

        let pointer = LfsPointer::parse(contents).expect("extended pointer should parse");

        assert_eq!(
            pointer
                .extensions()
                .get("ext-0-Foo_bar/path+v1")
                .map(String::as_str),
            Some("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(pointer.to_pointer_file(), contents);
    }

    #[test]
    fn pointer_normalizes_extension_oids() {
        let contents = "version https://git-lfs.github.com/spec/v1\n\
             ext-0-foo sha256:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";

        let pointer = LfsPointer::parse(contents).expect("extended pointer should parse");

        assert_eq!(
            pointer.extensions().get("ext-0-foo").map(String::as_str),
            Some("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
        assert_eq!(
            pointer.to_pointer_file(),
            "version https://git-lfs.github.com/spec/v1\n\
             ext-0-foo sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n"
        );
    }

    #[test]
    fn pointer_rejects_unknown_extension_records() {
        let contents = "version https://git-lfs.github.com/spec/v1\n\
             metadata arbitrary extension value\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";

        assert!(matches!(
            LfsPointer::parse(contents),
            Err(LfsObjectError::PointerUnexpectedLine { .. })
        ));
    }

    #[test]
    fn pointer_rejects_extension_values_that_are_not_pointer_oids() {
        let contents = "version https://git-lfs.github.com/spec/v1\n\
             ext-0-foo not-an-oid\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";

        assert!(matches!(
            LfsPointer::parse(contents),
            Err(LfsObjectError::PointerInvalidExtensionValue { .. })
        ));
    }

    #[test]
    fn pointer_rejects_missing_and_unexpected_lines() {
        let missing_size = "version https://git-lfs.github.com/spec/v1\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n";
        let unexpected = "version https://git-lfs.github.com/spec/v1\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n\
             metadata value\n";

        assert!(matches!(
            LfsPointer::parse(missing_size),
            Err(LfsObjectError::PointerMissingSize)
        ));
        assert!(matches!(
            LfsPointer::parse(unexpected),
            Err(LfsObjectError::PointerUnexpectedLine { .. })
        ));
    }

    #[test]
    fn pointer_rejects_raw_oid_without_sha256_prefix() {
        let contents = "version https://git-lfs.github.com/spec/v1\n\
             oid aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";

        assert!(matches!(
            LfsPointer::parse(contents),
            Err(LfsObjectError::PointerOidMissingSha256Prefix { .. })
        ));
    }

    #[test]
    fn pointer_rejects_invalid_version_and_size() {
        let invalid_version = "version https://example.com/spec/v1\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n";
        let invalid_size = "version https://git-lfs.github.com/spec/v1\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size not-a-number\n";

        assert!(matches!(
            LfsPointer::parse(invalid_version),
            Err(LfsObjectError::PointerInvalidVersion { .. })
        ));
        assert!(matches!(
            LfsPointer::parse(invalid_size),
            Err(LfsObjectError::PointerInvalidSize { .. })
        ));
    }

    #[test]
    fn pointer_rejects_out_of_order_and_duplicate_keys() {
        let oid_before_version = "oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             version https://git-lfs.github.com/spec/v1\n\
             size 42\n";
        let duplicate_oid = "version https://git-lfs.github.com/spec/v1\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             oid sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
             size 42\n";

        assert!(matches!(
            LfsPointer::parse(oid_before_version),
            Err(LfsObjectError::PointerUnexpectedLine { .. })
        ));
        assert!(matches!(
            LfsPointer::parse(duplicate_oid),
            Err(LfsObjectError::PointerUnexpectedLine { .. })
        ));
    }

    #[test]
    fn pointer_renders_extension_keys_in_canonical_order() {
        let object = LfsObject::new(LfsOid::from_str(OID).unwrap(), LfsObjectSize::new(42));
        let mut pointer = LfsPointer::new(object);

        pointer
            .insert_extension(
                "ext-9-zz",
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            )
            .expect("extension key should be valid");
        pointer
            .insert_extension(
                "ext-0-aa",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            )
            .expect("extension key should be valid");

        assert_eq!(
            pointer.to_pointer_file(),
            "version https://git-lfs.github.com/spec/v1\n\
             ext-0-aa sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\n\
             ext-9-zz sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc\n\
             oid sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n\
             size 42\n"
        );
    }

    #[test]
    fn pointer_rejects_invalid_extension_keys() {
        let object = LfsObject::new(LfsOid::from_str(OID).unwrap(), LfsObjectSize::new(42));
        let mut pointer = LfsPointer::new(object);

        assert!(matches!(
            pointer.insert_extension(
                "oid",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ),
            Err(LfsObjectError::PointerInvalidExtensionKey { .. })
        ));
        assert!(matches!(
            pointer.insert_extension(
                "metadata",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ),
            Err(LfsObjectError::PointerInvalidExtensionKey { .. })
        ));
        assert!(matches!(
            pointer.insert_extension("ext-test", "bad\nvalue"),
            Err(LfsObjectError::PointerInvalidExtensionKey { .. })
        ));
        assert!(matches!(
            pointer.insert_extension(
                "ext-0--test",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ),
            Err(LfsObjectError::PointerInvalidExtensionKey { .. })
        ));
        assert!(matches!(
            pointer.insert_extension(
                "ext-0-.test",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
            ),
            Err(LfsObjectError::PointerInvalidExtensionKey { .. })
        ));
        assert!(matches!(
            pointer.insert_extension("ext-0-test", "bad\nvalue"),
            Err(LfsObjectError::PointerInvalidExtensionValue { .. })
        ));
        assert!(matches!(
            pointer.insert_extension("ext-0-test", "not-an-oid"),
            Err(LfsObjectError::PointerInvalidExtensionValue { .. })
        ));
    }

    #[test]
    fn batch_object_response_separates_actions_from_errors() {
        let object = LfsObject::new(LfsOid::from_str(OID).unwrap(), LfsObjectSize::new(42));
        let response = LfsBatchObjectResponse::with_error(
            &object,
            LfsBatchObjectError::new(404, "object not found"),
        );

        assert_eq!(response.oid, object.oid);
        assert_eq!(response.size, object.size);
        assert!(response.actions.is_empty());
        assert_eq!(response.error.as_ref().map(|error| error.code), Some(404));
    }

    #[test]
    fn batch_request_json_parses_operation_objects_transfers_and_ref() {
        let request = parse_lfs_batch_request_json(
            br#"{
                "operation": "download",
                "transfers": ["basic"],
                "ref": { "name": "refs/heads/main" },
                "objects": [
                    {
                        "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                        "size": 42
                    }
                ]
            }"#,
        )
        .expect("valid batch request should parse");

        assert_eq!(request.operation, LfsBatchOperation::Download);
        assert_eq!(request.transfers, ["basic"]);
        assert_eq!(
            request
                .ref_context
                .as_ref()
                .map(|ref_context| ref_context.name.as_str()),
            Some("refs/heads/main")
        );
        assert_eq!(request.objects.len(), 1);
        assert_eq!(request.objects[0].oid.as_hex(), OID);
        assert_eq!(request.objects[0].size.bytes(), 42);
    }

    #[test]
    fn batch_request_json_rejects_invalid_shape_and_objects() {
        let unsupported_operation = br#"{
            "operation": "lock",
            "objects": [
                {
                    "oid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                    "size": 42
                }
            ]
        }"#;
        let invalid_object = br#"{
            "operation": "upload",
            "objects": [
                {
                    "oid": "not-a-sha",
                    "size": 42
                }
            ]
        }"#;

        assert!(parse_lfs_batch_request_json(unsupported_operation).is_err());
        assert!(parse_lfs_batch_request_json(invalid_object).is_err());
    }

    #[test]
    fn batch_action_allows_already_expired_lifetime() {
        let mut action = super::LfsBatchAction::new("https://lfs.example.com/object");

        action.expires_in = Some(-1);

        assert_eq!(action.expires_in, Some(-1));
    }
}
