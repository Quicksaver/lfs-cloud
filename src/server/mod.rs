//! HTTP server entrypoint and Git LFS route resolution.
//!
//! This module owns the first server-facing boundary: loading a validated
//! configuration, validating configured storage readiness, binding an Axum
//! listener, reporting reachable URLs, resolving incoming Git LFS request paths
//! to configured repository mappings, and proxying authenticated transfers.

use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet, HashMap},
    future::{Future, IntoFuture},
    io::{self, ErrorKind},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket},
    path::{Path, PathBuf},
    pin::Pin,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{ConnectInfo, OriginalUri, Request, State, connect_info::Connected},
    http::{
        HeaderMap, HeaderValue, Method, StatusCode,
        header::{
            ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, RETRY_AFTER, WWW_AUTHENTICATE,
        },
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::delete,
    serve::IncomingStream,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use futures_util::{StreamExt, stream};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex as AsyncMutex, OwnedSemaphorePermit, Semaphore};
use tokio_util::io::ReaderStream;
use url::{Url, form_urlencoded};

use crate::{
    DEFAULT_GIT_CREDENTIAL_USERNAME, ErrorCategory, GitHubPersonalAccessTokenLoginRouteState,
    LFS_BASIC_TRANSFER, LfsBatchDownloadObject, LfsBatchObjectError, LfsBatchOperation,
    LfsBatchRequest, LfsBatchResponse, LfsBatchUploadObject, LfsObject, LfsObjectSize, LfsOid,
    LfsSessionToken, LocalLfsSessionStore, MetadataDatabase, MetadataObjectVerificationStatus,
    ProviderFuture, RepositoryAuthentication, RepositoryIdentity, RepositoryMapping,
    RepositoryPermission, RepositoryProvider, RepositoryProviderError, RepositoryUser,
    SanitizedMessage, ServerConfig, ServerError, ServerResult, StorageDownloadResponse,
    StorageError, StorageProvider, StoredObject, github_personal_access_token_login_router,
    metadata::{MetadataTransferOperation, MetadataTransferResult},
    parse_lfs_batch_request_json,
    provider_factory::{
        ConfiguredStorageProviders, ServerStorageProvider, ServerStorageProviderFactory,
    },
    sessions::LfsSessionRecord,
};

const LFS_AUTH_CHALLENGE: &str = "Basic realm=\"lfscloud\"";
/// Authenticated endpoint for revoking the presented local LFS session.
pub const LFS_SESSION_REVOKE_PATH: &str = "/auth/session";
const GIT_LFS_JSON_CONTENT_TYPE: &str = "application/vnd.git-lfs+json";
const MAX_UPLOAD_OBJECT_BYTES: u64 = 5 * 1024 * 1024 * 1024;
const MIN_UPLOAD_STAGING_FREE_BYTES: u64 = 64 * 1024 * 1024;
const UPLOAD_STAGING_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_BATCH_BODY_BYTES: usize = 2 * 1024 * 1024;
const BATCH_BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(15);
const BATCH_BODY_TOTAL_TIMEOUT: Duration = Duration::from_secs(60);
const BATCH_STORAGE_LOOKUP_CONCURRENCY: usize = 16;
const AUTHORIZATION_CACHE_TTL: Duration = Duration::from_secs(15);
const SERVER_SHUTDOWN_DRAIN_TIMEOUT: Duration = Duration::from_secs(30);

include!("runtime.rs");
include!("router.rs");
include!("request.rs");
include!("staging.rs");
include!("batch.rs");
include!("routing.rs");

#[cfg(test)]
mod tests {
    include!("test_support.rs");
    server_storage_and_composition_tests!();
    server_routing_and_batch_tests!();
    server_transfer_tests!();
    server_authorization_tests!();
}
