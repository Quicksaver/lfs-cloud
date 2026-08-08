//! Google Drive storage-provider authentication and object-transfer helpers.
//!
//! Authentication obtains short-lived access tokens from Google Cloud CLI
//! Application Default Credentials (ADC). It does not expose credentials to
//! Git LFS clients.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    fmt,
    fs::{self, File},
    io::{self, BufReader, Read, Seek, SeekFrom},
    path::Path,
    process::{Command as ProcessCommand, Stdio},
    sync::{Arc, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    body::{Body as AxumBody, Bytes},
    response::Response as AxumResponse,
};
use futures_util::StreamExt;
use reqwest::{
    Body as ReqwestBody, Client, StatusCode,
    header::{
        ACCEPT, ACCEPT_ENCODING, AUTHORIZATION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE,
        HeaderMap, HeaderValue, LOCATION, RANGE,
    },
    redirect::Policy,
};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex as AsyncMutex, watch};
use tokio::{
    io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt},
    process::Command,
};
use url::Url;

use crate::{
    BackendIdLookup, GoogleDriveGcloudCredentialsConfig, GoogleDriveStorageConfig, LfsObject,
    ProviderFuture, SanitizedMessage, StorageDeleteOutcome, StorageDownloadResponse, StorageError,
    StorageProvider, StorageResult, StoredObject, StreamingStorageProvider,
    http_transport::{has_exact_loopback_host, read_bounded_lossy_response_body},
};

const GCLOUD_ADC_TOKEN_TIMEOUT: Duration = Duration::from_secs(30);
const GCLOUD_ADC_ACCESS_TOKEN_LIFETIME_SECONDS: u64 = 3_600;
const MAX_GCLOUD_ADC_TOKEN_BYTES: usize = 4 * 1024;
const GOOGLE_APPLICATION_CREDENTIALS_ENV: &str = "GOOGLE_APPLICATION_CREDENTIALS";
const CLOUDSDK_AUTH_ACCESS_TOKEN_ENV: &str = "CLOUDSDK_AUTH_ACCESS_TOKEN";
const CLOUDSDK_CONFIG_ENV: &str = "CLOUDSDK_CONFIG";
const CLOUDSDK_CORE_DISABLE_PROMPTS_ENV: &str = "CLOUDSDK_CORE_DISABLE_PROMPTS";
const GOOGLE_DRIVE_ROOT_VALIDATION_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_OBJECT_METADATA_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const GOOGLE_DRIVE_TRANSFER_READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const GOOGLE_DRIVE_RESUMABLE_UPLOAD_CHUNK_SIZE: usize = 256 * 1024;
const GOOGLE_DRIVE_RESUMABLE_UPLOAD_MAX_RECOVERY_ATTEMPTS: u32 = 4;
const GOOGLE_DRIVE_RESUMABLE_UPLOAD_INITIAL_BACKOFF: Duration = Duration::from_millis(250);
const GOOGLE_ACCESS_TOKEN_REFRESH_SKEW: Duration = Duration::from_secs(60);
const MAX_GOOGLE_ERROR_BODY_LEN: usize = 16 * 1024;
const MIN_REDACTED_SECRET_FRAGMENT_LEN: usize = 6;
const MAX_GOOGLE_DRIVE_CUSTOM_PROPERTY_BYTES: usize = 124;
const GOOGLE_DRIVE_FOLDER_MIME_TYPE: &str = "application/vnd.google-apps.folder";
const GOOGLE_DRIVE_OBJECT_CONTENT_TYPE: &str = "application/octet-stream";
const GOOGLE_DRIVE_OBJECT_VERSION: &str = "1";
const GOOGLE_DRIVE_OBJECT_VERSION_PROPERTY: &str = "lfsCloudVersion";
const GOOGLE_DRIVE_REPO_NAMESPACE_PROPERTY: &str = "lfsCloudRepoNamespace";
const GOOGLE_DRIVE_REPO_NAMESPACE_FORMAT_PROPERTY: &str = "lfsCloudRepoNamespaceFormat";
const GOOGLE_DRIVE_REPO_NAMESPACE_SHA256_FORMAT: &str = "sha256";
const GOOGLE_DRIVE_OBJECT_OID_PROPERTY: &str = "lfsCloudOid";
const GOOGLE_DRIVE_OBJECT_SIZE_PROPERTY: &str = "lfsCloudSize";
const GOOGLE_DRIVE_FOLDER_KIND_PROPERTY: &str = "lfsCloudFolderKind";
const GOOGLE_DRIVE_SHARD_KIND_PROPERTY: &str = GOOGLE_DRIVE_FOLDER_KIND_PROPERTY;
const GOOGLE_DRIVE_SHARD_KIND: &str = "objectShard";
const GOOGLE_DRIVE_SHARD_PREFIX_PROPERTY: &str = "lfsCloudShard";
const GOOGLE_DRIVE_STORAGE_ROOT_KIND: &str = "storageRoot";
const GOOGLE_DRIVE_ROOT_PARENT_ID: &str = "root";

static DEFAULT_GOOGLE_DRIVE_ROOT_VALIDATION_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_OBJECT_METADATA_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_OBJECT_UPLOAD_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();
static DEFAULT_GOOGLE_DRIVE_OBJECT_DOWNLOAD_HTTP_CLIENT: OnceLock<Client> = OnceLock::new();

/// Google Drive API root used for storage-provider metadata and transfer calls.
pub const GOOGLE_DRIVE_API_BASE_URL: &str = "https://www.googleapis.com";

/// MVP Google Drive OAuth scope for app-accessible LFS object storage.
pub const GOOGLE_DRIVE_FILE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";

/// Default Google Drive folder created for LFS Cloud object storage.
pub const GOOGLE_DRIVE_DEFAULT_ROOT_FOLDER_NAME: &str = ".lfscloud";

include!("access_token.rs");
include!("root.rs");
include!("object_key.rs");
include!("object_store.rs");
include!("upload.rs");
include!("download.rs");
include!("drive_api.rs");
include!("diagnostics.rs");

#[cfg(test)]
use std::{
    io::Cursor,
    str::FromStr,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

#[cfg(test)]
use crate::{LfsObjectSize, LfsOid};
#[cfg(test)]
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path as AxumPath, State},
    http::Uri,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
#[cfg(test)]
use tokio_util::io::ReaderStream;

#[cfg(test)]
const OBJECT_OID: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
#[cfg(test)]
fn storage_config(_credential_name: &str) -> GoogleDriveStorageConfig {
    GoogleDriveStorageConfig {
        id: "drive-user-a".to_owned(),
        credentials: GoogleDriveGcloudCredentialsConfig {
            config_dir: ".gcloud-drive".into(),
            executable: "gcloud".into(),
        },
        root_folder_id: "drive-root".to_owned(),
        display_name: None,
    }
}

#[cfg(test)]
fn access_token() -> super::GoogleDriveAccessToken {
    super::GoogleDriveAccessToken {
        access_token: "access-token".to_owned(),
        token_type: "Bearer".to_owned(),
        expires_in_seconds: Some(3600),
        scope: vec![super::GOOGLE_DRIVE_FILE_SCOPE.to_owned()],
    }
}

#[cfg(test)]
fn lfs_object() -> LfsObject {
    LfsObject::new(
        LfsOid::from_str(OBJECT_OID).expect("test OID should parse"),
        LfsObjectSize::new(42),
    )
}

#[cfg(test)]
fn lfs_object_for_bytes(bytes: &[u8]) -> LfsObject {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    LfsObject::new(
        LfsOid::from_str(&format!("{:x}", hasher.finalize())).expect("test OID should parse"),
        LfsObjectSize::new(bytes.len() as u64),
    )
}

#[cfg(test)]
fn drive_folder_json() -> &'static str {
    r#"{
        "id":"drive-root",
        "name":"LFS Cloud Root",
        "mimeType":"application/vnd.google-apps.folder",
        "trashed":false,
        "capabilities":{"canAddChildren":true}
    }"#
}

#[cfg(test)]
fn drive_object_list_json(file_id: &str, oid: &str, size: u64) -> String {
    format!(r#"{{"files":[{}]}}"#, drive_object_json(file_id, oid, size))
}

#[cfg(test)]
fn drive_object_json(file_id: &str, oid: &str, size: u64) -> String {
    format!(
        r#"{{
            "id":"{file_id}",
            "name":"sha256-{oid}-{size}.lfs",
            "size":"{size}",
            "parents":["drive-root"],
            "trashed":false,
            "appProperties":{{
                "lfsCloudVersion":"1",
                "lfsCloudRepoNamespace":"github.com/owner/repo",
                "lfsCloudOid":"{oid}",
                "lfsCloudSize":"{size}"
            }}
        }}"#
    )
}

#[cfg(test)]
fn form_pairs(body: &str) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}
