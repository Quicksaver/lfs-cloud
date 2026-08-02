// This file is included by `mod.rs` so the migration API remains in one module.

/// Result of uploading locally available migration objects into LFS Cloud storage.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationStorageUpload {
    /// Configured storage provider ID that received the upload checks.
    pub storage_provider_id: String,
    /// Stable repository namespace used for every storage operation.
    pub repository_namespace: String,
    /// Objects skipped because the configured storage provider already has them.
    pub already_present_objects: Vec<LfsObject>,
    /// Objects uploaded during this run or restored from its durable checkpoint.
    pub uploaded_objects: Vec<StoredObject>,
    /// Objects that could not complete, with retry-safe diagnostics.
    pub failed_objects: Vec<MigrationObjectUploadFailure>,
    /// One terminal outcome per requested object, in discovery order.
    pub outcomes: Vec<MigrationObjectUploadOutcome>,
    /// Durable checkpoint used to resume completed objects.
    pub checkpoint_path: PathBuf,
}

/// Options controlling bounded migration uploads and durable progress.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MigrationStorageUploadOptions {
    checkpoint_path: PathBuf,
    max_concurrent_uploads: usize,
}

impl MigrationStorageUploadOptions {
    /// Creates upload options using the supplied durable checkpoint path.
    ///
    /// The default concurrency is [`DEFAULT_MIGRATION_UPLOAD_CONCURRENCY`].
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::MigrationStorageUploadOptions;
    ///
    /// let options = MigrationStorageUploadOptions::new(".git/lfs/migration.jsonl")
    ///     .with_max_concurrent_uploads(2);
    /// assert_eq!(options.max_concurrent_uploads(), 2);
    /// ```
    #[must_use]
    pub fn new(checkpoint_path: impl Into<PathBuf>) -> Self {
        Self {
            checkpoint_path: checkpoint_path.into(),
            max_concurrent_uploads: DEFAULT_MIGRATION_UPLOAD_CONCURRENCY,
        }
    }

    /// Sets the maximum number of simultaneous object transfers.
    ///
    /// A zero value is rejected when upload execution starts.
    #[must_use]
    pub fn with_max_concurrent_uploads(mut self, max_concurrent_uploads: usize) -> Self {
        self.max_concurrent_uploads = max_concurrent_uploads;
        self
    }

    /// Returns the durable checkpoint path.
    #[must_use]
    pub fn checkpoint_path(&self) -> &Path {
        &self.checkpoint_path
    }

    /// Returns the maximum number of simultaneous object transfers.
    #[must_use]
    pub fn max_concurrent_uploads(&self) -> usize {
        self.max_concurrent_uploads
    }
}

/// Structured result for one requested migration object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationObjectUploadOutcome {
    /// Object requested by the migration inventory.
    pub object: LfsObject,
    /// Terminal status observed during this run.
    pub status: MigrationObjectUploadStatus,
}

/// Terminal migration-upload status for one object.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MigrationObjectUploadStatus {
    /// Storage already contained the object before upload.
    AlreadyPresent {
        /// True when this completion was restored without contacting storage.
        resumed: bool,
    },
    /// Storage accepted and verified the object.
    Uploaded {
        /// Verified provider metadata returned for the stored object.
        stored_object: StoredObject,
        /// True when this completion was restored without contacting storage.
        resumed: bool,
    },
    /// This object failed while other independent objects continued.
    Failed {
        /// Secret-safe failure diagnostic suitable for a retry report.
        message: SanitizedMessage,
    },
}

/// One migration object that should be retried.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationObjectUploadFailure {
    /// Object whose transfer did not complete durably.
    pub object: LfsObject,
    /// Secret-safe reason the object should be retried.
    pub message: SanitizedMessage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum MigrationUploadCheckpointCompletion {
    AlreadyPresent,
    Uploaded { backend_id: String },
}

#[derive(Debug, Deserialize, Serialize)]
struct MigrationUploadCheckpointRecord {
    version: u32,
    storage_provider_id: String,
    repository_namespace: String,
    oid: String,
    size: u64,
    completion: MigrationUploadCheckpointRecordCompletion,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case", tag = "status")]
enum MigrationUploadCheckpointRecordCompletion {
    AlreadyPresent,
    Uploaded { backend_id: String },
}

/// Uploads locally available migration objects to configured LFS Cloud storage.
///
/// The helper is intentionally idempotent: it checks the destination storage
/// provider before uploading each object and reports already-present objects
/// separately. For objects that do need upload, it re-verifies the selected
/// local source bytes against the pointer OID and size immediately before
/// delegating to the storage provider. Every lookup, upload, and durable
/// checkpoint is bound to `repository_namespace`. Checkpointed completions are
/// revalidated against the active storage provider before they are resumed, so
/// a deleted object or a provider configuration change cannot make stale
/// durable state look complete.
///
/// Uploads run with [`DEFAULT_MIGRATION_UPLOAD_CONCURRENCY`] simultaneous
/// transfers. Each success is appended and synchronized to a provider-specific
/// checkpoint under the repository's Git LFS media directory before it is
/// reported. A later invocation resumes those completions after revalidating
/// them against storage, while missing or failed objects are retried. Outcomes
/// retain discovery order even though provider work completes out of order.
///
/// # Errors
///
/// Returns [`MigrationError`] when the checkpoint cannot be initialized or
/// parsed, or when upload options are invalid. Per-object source, provider, and
/// checkpoint-append failures are returned in
/// [`MigrationStorageUpload::failed_objects`] so independent work can finish.
pub async fn upload_migration_objects_to_storage(
    availability: &LocalMigrationObjectAvailability,
    storage: &dyn StorageProvider,
    repository_namespace: &str,
) -> MigrationResult<MigrationStorageUpload> {
    let checkpoint_path = default_migration_upload_checkpoint_path(
        availability,
        storage.provider_id(),
        repository_namespace,
    );
    let options = MigrationStorageUploadOptions::new(checkpoint_path);
    upload_migration_objects_to_storage_with_options(
        availability,
        storage,
        repository_namespace,
        &options,
    )
    .await
}

/// Uploads migration objects with an explicit checkpoint and concurrency bound.
///
/// This variant is useful when a migration coordinator owns the durable state
/// location or needs a provider-specific concurrency limit. Completed outcomes
/// are appended as JSON Lines records and synchronized individually, making the
/// checkpoint safe to reuse after interruption. A partial final line left by a
/// process crash is ignored; malformed complete records fail closed. The
/// checkpoint cannot be reused for another repository namespace, and every
/// restored completion is checked against the active storage target.
///
/// # Errors
///
/// Returns [`MigrationError`] when the concurrency limit is zero, the
/// checkpoint cannot be initialized or parsed, or its completed records do not
/// match the configured storage provider. Per-object failures are reported as
/// structured outcomes instead of aborting the remaining work.
pub async fn upload_migration_objects_to_storage_with_options(
    availability: &LocalMigrationObjectAvailability,
    storage: &dyn StorageProvider,
    repository_namespace: &str,
    options: &MigrationStorageUploadOptions,
) -> MigrationResult<MigrationStorageUpload> {
    if options.max_concurrent_uploads == 0 {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(
                "migration upload concurrency must be greater than zero",
            ),
        });
    }

    let storage_provider_id = storage.provider_id().to_owned();
    let repository_namespace = repository_namespace.to_owned();
    let checkpoint_path = options.checkpoint_path.clone();
    let checkpointed = load_migration_upload_checkpoint(
        checkpoint_path.clone(),
        storage_provider_id.clone(),
        repository_namespace.clone(),
    )
    .await?;
    let mut indexed_outcomes = stream::iter(availability.objects.iter().cloned().enumerate())
        .map(|(index, local_object)| {
            let checkpointed = checkpointed.get(&local_object.object).cloned();
            let checkpoint_path = checkpoint_path.clone();
            let storage_provider_id = storage_provider_id.clone();
            let repository_namespace = repository_namespace.clone();
            async move {
                let outcome = match checkpointed {
                    Some(completion) => {
                        match storage
                            .object_exists(&repository_namespace, &local_object.object)
                            .await
                        {
                            Ok(true) => resumed_migration_upload_outcome(
                                local_object.object.clone(),
                                &storage_provider_id,
                                &repository_namespace,
                                completion,
                            ),
                            Ok(false) => {
                                let status = upload_missing_migration_object(
                                    &local_object,
                                    storage,
                                    &repository_namespace,
                                )
                                .await;
                                checkpoint_migration_upload_outcome(
                                    &checkpoint_path,
                                    &storage_provider_id,
                                    &repository_namespace,
                                    local_object.object.clone(),
                                    status,
                                )
                                .await
                            }
                            Err(error) => {
                                checkpoint_migration_upload_outcome(
                                    &checkpoint_path,
                                    &storage_provider_id,
                                    &repository_namespace,
                                    local_object.object.clone(),
                                    Err(error.into()),
                                )
                                .await
                            }
                        }
                    }
                    None => {
                        let status = upload_one_migration_object(
                            &local_object,
                            storage,
                            &repository_namespace,
                        )
                        .await;
                        checkpoint_migration_upload_outcome(
                            &checkpoint_path,
                            &storage_provider_id,
                            &repository_namespace,
                            local_object.object.clone(),
                            status,
                        )
                        .await
                    }
                };
                (index, outcome)
            }
        })
        .buffer_unordered(options.max_concurrent_uploads)
        .collect::<Vec<_>>()
        .await;
    indexed_outcomes.sort_by_key(|(index, _)| *index);
    let outcomes = indexed_outcomes
        .into_iter()
        .map(|(_, outcome)| outcome)
        .collect::<Vec<_>>();

    let already_present_objects = outcomes
        .iter()
        .filter(|outcome| {
            matches!(
                outcome.status,
                MigrationObjectUploadStatus::AlreadyPresent { .. }
            )
        })
        .map(|outcome| outcome.object.clone())
        .collect();
    let uploaded_objects = outcomes
        .iter()
        .filter_map(|outcome| match &outcome.status {
            MigrationObjectUploadStatus::Uploaded { stored_object, .. } => {
                Some(stored_object.clone())
            }
            MigrationObjectUploadStatus::AlreadyPresent { .. }
            | MigrationObjectUploadStatus::Failed { .. } => None,
        })
        .collect();
    let failed_objects = outcomes
        .iter()
        .filter_map(|outcome| match &outcome.status {
            MigrationObjectUploadStatus::Failed { message } => Some(MigrationObjectUploadFailure {
                object: outcome.object.clone(),
                message: message.clone(),
            }),
            MigrationObjectUploadStatus::AlreadyPresent { .. }
            | MigrationObjectUploadStatus::Uploaded { .. } => None,
        })
        .collect();

    Ok(MigrationStorageUpload {
        storage_provider_id,
        repository_namespace,
        already_present_objects,
        uploaded_objects,
        failed_objects,
        outcomes,
        checkpoint_path,
    })
}

async fn upload_one_migration_object(
    local_object: &LocalMigrationObject,
    storage: &dyn StorageProvider,
    repository_namespace: &str,
) -> MigrationResult<MigrationObjectUploadStatus> {
    let object = &local_object.object;
    if storage.object_exists(repository_namespace, object).await? {
        return Ok(MigrationObjectUploadStatus::AlreadyPresent { resumed: false });
    }

    upload_missing_migration_object(local_object, storage, repository_namespace).await
}

async fn upload_missing_migration_object(
    local_object: &LocalMigrationObject,
    storage: &dyn StorageProvider,
    repository_namespace: &str,
) -> MigrationResult<MigrationObjectUploadStatus> {
    let object = &local_object.object;
    let source = verified_migration_upload_source_path(local_object)?;
    verify_migration_upload_source(source, object).await?;
    let stored_object = storage
        .upload_object(repository_namespace, object, source)
        .await?;
    validate_migration_uploaded_object(
        object,
        storage.provider_id(),
        repository_namespace,
        &stored_object,
    )?;
    Ok(MigrationObjectUploadStatus::Uploaded {
        stored_object,
        resumed: false,
    })
}

async fn checkpoint_migration_upload_outcome(
    checkpoint_path: &Path,
    storage_provider_id: &str,
    repository_namespace: &str,
    object: LfsObject,
    status: MigrationResult<MigrationObjectUploadStatus>,
) -> MigrationObjectUploadOutcome {
    let status = match status {
        Ok(status) => {
            let completion = match &status {
                MigrationObjectUploadStatus::AlreadyPresent { .. } => {
                    Some(MigrationUploadCheckpointCompletion::AlreadyPresent)
                }
                MigrationObjectUploadStatus::Uploaded { stored_object, .. } => {
                    Some(MigrationUploadCheckpointCompletion::Uploaded {
                        backend_id: stored_object.backend_id.clone(),
                    })
                }
                MigrationObjectUploadStatus::Failed { .. } => None,
            };
            if let Some(completion) = completion
                && let Err(error) = append_migration_upload_checkpoint(
                    checkpoint_path.to_path_buf(),
                    storage_provider_id.to_owned(),
                    repository_namespace.to_owned(),
                    object.clone(),
                    completion,
                )
                .await
            {
                MigrationObjectUploadStatus::Failed {
                    message: SanitizedMessage::new(format!(
                        "object completed in storage but durable checkpoint failed: {error}"
                    )),
                }
            } else {
                status
            }
        }
        Err(error) => MigrationObjectUploadStatus::Failed {
            message: SanitizedMessage::new(error.to_string()),
        },
    };

    MigrationObjectUploadOutcome { object, status }
}

fn resumed_migration_upload_outcome(
    object: LfsObject,
    storage_provider_id: &str,
    repository_namespace: &str,
    completion: MigrationUploadCheckpointCompletion,
) -> MigrationObjectUploadOutcome {
    let status = match completion {
        MigrationUploadCheckpointCompletion::AlreadyPresent => {
            MigrationObjectUploadStatus::AlreadyPresent { resumed: true }
        }
        MigrationUploadCheckpointCompletion::Uploaded { backend_id } => {
            MigrationObjectUploadStatus::Uploaded {
                stored_object: StoredObject::new(
                    storage_provider_id,
                    repository_namespace,
                    object.clone(),
                    backend_id,
                ),
                resumed: true,
            }
        }
    };
    MigrationObjectUploadOutcome { object, status }
}

fn default_migration_upload_checkpoint_path(
    availability: &LocalMigrationObjectAvailability,
    storage_provider_id: &str,
    repository_namespace: &str,
) -> PathBuf {
    let checkpoint_identity = format!("{storage_provider_id}\0{repository_namespace}");
    let checkpoint_digest = Sha256::digest(checkpoint_identity.as_bytes());
    let filename = format!("lfscloud-migration-upload-{checkpoint_digest:x}.jsonl");
    availability
        .git_lfs_objects_dir
        .parent()
        .unwrap_or(&availability.git_lfs_objects_dir)
        .join(filename)
}

async fn load_migration_upload_checkpoint(
    checkpoint_path: PathBuf,
    storage_provider_id: String,
    repository_namespace: String,
) -> MigrationResult<BTreeMap<LfsObject, MigrationUploadCheckpointCompletion>> {
    tokio::task::spawn_blocking(move || {
        load_migration_upload_checkpoint_blocking(
            &checkpoint_path,
            &storage_provider_id,
            &repository_namespace,
        )
    })
    .await
    .map_err(|error| MigrationError::InvalidInput {
        message: SanitizedMessage::new(format!(
            "migration checkpoint loading task failed: {error}"
        )),
    })?
}

fn load_migration_upload_checkpoint_blocking(
    checkpoint_path: &Path,
    storage_provider_id: &str,
    repository_namespace: &str,
) -> MigrationResult<BTreeMap<LfsObject, MigrationUploadCheckpointCompletion>> {
    create_migration_checkpoint_parent(checkpoint_path)?;
    let mut file = open_migration_checkpoint(checkpoint_path)?;
    FileExt::lock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to lock migration upload checkpoint",
            source,
        )
    })?;
    let result = (|| {
        let mut contents = String::new();
        file.read_to_string(&mut contents).map_err(|source| {
            migration_checkpoint_io_error(
                checkpoint_path,
                "failed to read migration upload checkpoint",
                source,
            )
        })?;
        parse_migration_upload_checkpoint(
            &contents,
            checkpoint_path,
            storage_provider_id,
            repository_namespace,
        )
    })();
    let unlock_result = FileExt::unlock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to unlock migration upload checkpoint",
            source,
        )
    });
    match (result, unlock_result) {
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        (Ok(completed), Ok(())) => Ok(completed),
    }
}

fn parse_migration_upload_checkpoint(
    contents: &str,
    checkpoint_path: &Path,
    storage_provider_id: &str,
    repository_namespace: &str,
) -> MigrationResult<BTreeMap<LfsObject, MigrationUploadCheckpointCompletion>> {
    let mut completed = BTreeMap::new();
    let chunks = contents.split_inclusive('\n').collect::<Vec<_>>();
    for (index, chunk) in chunks.iter().enumerate() {
        let line = chunk
            .strip_suffix('\n')
            .unwrap_or(chunk)
            .trim_end_matches('\r');
        if line.is_empty() {
            continue;
        }
        let record = match serde_json::from_str::<MigrationUploadCheckpointRecord>(line) {
            Ok(record) => record,
            Err(_) if index + 1 == chunks.len() && !chunk.ends_with('\n') => break,
            Err(source) => {
                return Err(MigrationError::InvalidInput {
                    message: SanitizedMessage::new(format!(
                        "migration upload checkpoint {} contains invalid record {}: {source}",
                        checkpoint_path.display(),
                        index + 1
                    )),
                });
            }
        };
        if record.version != MIGRATION_UPLOAD_CHECKPOINT_VERSION {
            return Err(MigrationError::InvalidInput {
                message: SanitizedMessage::new(format!(
                    "migration upload checkpoint {} uses unsupported version {}",
                    checkpoint_path.display(),
                    record.version
                )),
            });
        }
        if record.storage_provider_id != storage_provider_id {
            return Err(MigrationError::InvalidInput {
                message: SanitizedMessage::new(format!(
                    "migration upload checkpoint {} belongs to a different storage provider",
                    checkpoint_path.display()
                )),
            });
        }
        if record.repository_namespace != repository_namespace {
            return Err(MigrationError::InvalidInput {
                message: SanitizedMessage::new(format!(
                    "migration upload checkpoint {} belongs to a different repository namespace",
                    checkpoint_path.display()
                )),
            });
        }
        let oid = LfsOid::from_str(&record.oid).map_err(|source| MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "migration upload checkpoint {} contains an invalid object ID: {source}",
                checkpoint_path.display()
            )),
        })?;
        let object = LfsObject::new(oid, LfsObjectSize::new(record.size));
        let completion = match record.completion {
            MigrationUploadCheckpointRecordCompletion::AlreadyPresent => {
                MigrationUploadCheckpointCompletion::AlreadyPresent
            }
            MigrationUploadCheckpointRecordCompletion::Uploaded { backend_id } => {
                if backend_id.trim().is_empty() {
                    return Err(MigrationError::InvalidInput {
                        message: SanitizedMessage::new(format!(
                            "migration upload checkpoint {} contains an empty backend object ID",
                            checkpoint_path.display()
                        )),
                    });
                }
                MigrationUploadCheckpointCompletion::Uploaded { backend_id }
            }
        };
        completed.insert(object, completion);
    }
    Ok(completed)
}

async fn append_migration_upload_checkpoint(
    checkpoint_path: PathBuf,
    storage_provider_id: String,
    repository_namespace: String,
    object: LfsObject,
    completion: MigrationUploadCheckpointCompletion,
) -> MigrationResult<()> {
    tokio::task::spawn_blocking(move || {
        append_migration_upload_checkpoint_blocking(
            &checkpoint_path,
            &storage_provider_id,
            &repository_namespace,
            &object,
            completion,
        )
    })
    .await
    .map_err(|error| MigrationError::InvalidInput {
        message: SanitizedMessage::new(format!("migration checkpoint write task failed: {error}")),
    })?
}

fn append_migration_upload_checkpoint_blocking(
    checkpoint_path: &Path,
    storage_provider_id: &str,
    repository_namespace: &str,
    object: &LfsObject,
    completion: MigrationUploadCheckpointCompletion,
) -> MigrationResult<()> {
    create_migration_checkpoint_parent(checkpoint_path)?;
    let mut file = open_migration_checkpoint(checkpoint_path)?;
    FileExt::lock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to lock migration upload checkpoint",
            source,
        )
    })?;
    let result = (|| {
        let completion = match completion {
            MigrationUploadCheckpointCompletion::AlreadyPresent => {
                MigrationUploadCheckpointRecordCompletion::AlreadyPresent
            }
            MigrationUploadCheckpointCompletion::Uploaded { backend_id } => {
                MigrationUploadCheckpointRecordCompletion::Uploaded { backend_id }
            }
        };
        let record = MigrationUploadCheckpointRecord {
            version: MIGRATION_UPLOAD_CHECKPOINT_VERSION,
            storage_provider_id: storage_provider_id.to_owned(),
            repository_namespace: repository_namespace.to_owned(),
            oid: object.oid.as_hex().to_owned(),
            size: object.size.bytes(),
            completion,
        };
        let mut encoded =
            serde_json::to_vec(&record).map_err(|source| MigrationError::InvalidInput {
                message: SanitizedMessage::new(format!(
                    "failed to encode migration upload checkpoint record: {source}"
                )),
            })?;
        encoded.push(b'\n');
        file.write_all(&encoded).map_err(|source| {
            migration_checkpoint_io_error(
                checkpoint_path,
                "failed to append migration upload checkpoint",
                source,
            )
        })?;
        file.sync_data().map_err(|source| {
            migration_checkpoint_io_error(
                checkpoint_path,
                "failed to synchronize migration upload checkpoint",
                source,
            )
        })
    })();
    let unlock_result = FileExt::unlock(&file).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to unlock migration upload checkpoint",
            source,
        )
    });
    result.and(unlock_result)
}

fn create_migration_checkpoint_parent(checkpoint_path: &Path) -> MigrationResult<()> {
    let parent = checkpoint_path
        .parent()
        .ok_or_else(|| MigrationError::InvalidInput {
            message: SanitizedMessage::new("migration upload checkpoint has no parent directory"),
        })?;
    fs::create_dir_all(parent).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to create migration upload checkpoint directory",
            source,
        )
    })
}

fn open_migration_checkpoint(checkpoint_path: &Path) -> MigrationResult<File> {
    let mut options = OpenOptions::new();
    options.create(true).read(true).append(true);
    #[cfg(unix)]
    options.mode(0o600);
    options.open(checkpoint_path).map_err(|source| {
        migration_checkpoint_io_error(
            checkpoint_path,
            "failed to open migration upload checkpoint",
            source,
        )
    })
}

fn migration_checkpoint_io_error(
    checkpoint_path: &Path,
    context: &str,
    source: io::Error,
) -> MigrationError {
    MigrationError::Io {
        context: format!("{context} {}", checkpoint_path.display()),
        source,
    }
}

pub(crate) fn verified_migration_upload_source_path(
    local_object: &LocalMigrationObject,
) -> MigrationResult<&Path> {
    [
        LocalMigrationObjectLocationKind::GitLfsMedia,
        LocalMigrationObjectLocationKind::SharedCache,
    ]
    .into_iter()
    .find_map(|preferred_kind| {
        local_object
            .locations
            .iter()
            .find(|location| {
                location.kind == preferred_kind
                    && matches!(
                        location.status,
                        LocalMigrationObjectLocationStatus::Available
                    )
            })
            .map(|location| location.path.as_path())
    })
    .ok_or_else(|| MigrationError::SourceObjectMissing {
        oid: local_object.object.oid.as_hex().to_owned(),
        size: local_object.object.size.bytes(),
    })
}

pub(crate) async fn verify_migration_upload_source(
    path: &Path,
    object: &LfsObject,
) -> MigrationResult<()> {
    let path = path.to_path_buf();
    let object = object.clone();
    tokio::task::spawn_blocking(move || verify_migration_upload_source_blocking(&path, &object))
        .await
        .map_err(|error| MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "migration source verification task failed: {error}"
            )),
        })?
}

fn verify_migration_upload_source_blocking(path: &Path, object: &LfsObject) -> MigrationResult<()> {
    let (actual_oid, actual_size) = hash_migration_object_file(path)?;
    if actual_oid == object.oid && actual_size == object.size {
        return Ok(());
    }

    Err(MigrationError::InvalidInput {
        message: SanitizedMessage::new(format!(
            "local migration source {} no longer matches sha256:{} ({} bytes): got sha256:{} ({} bytes)",
            path.display(),
            object.oid,
            object.size.bytes(),
            actual_oid,
            actual_size.bytes()
        )),
    })
}

fn validate_migration_uploaded_object(
    expected: &LfsObject,
    expected_provider_id: &str,
    expected_repository_namespace: &str,
    stored: &StoredObject,
) -> MigrationResult<()> {
    if stored.provider_id != expected_provider_id {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "storage provider returned provider ID {}, expected {}",
                stored.provider_id, expected_provider_id
            )),
        });
    }

    if stored.repository_namespace != expected_repository_namespace {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(
                "storage provider returned a different repository namespace",
            ),
        });
    }

    if stored.backend_id.trim().is_empty() {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "storage provider {expected_provider_id} returned an empty backend object ID"
            )),
        });
    }

    if stored.object != *expected {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(format!(
                "storage provider returned object sha256:{} ({} bytes), expected sha256:{} ({} bytes)",
                stored.object.oid,
                stored.object.size.bytes(),
                expected.oid,
                expected.size.bytes()
            )),
        });
    }

    Ok(())
}


#[cfg(test)]
mod upload_tests {
    use super::test_support::*;

    struct FakeMigrationStorageProvider {
        provider_id: String,
        existing: Mutex<BTreeSet<(String, LfsObject)>>,
        uploaded: Mutex<Vec<LfsObject>>,
        returned_object_override: Mutex<Option<LfsObject>>,
        returned_provider_id_override: Mutex<Option<String>>,
        returned_repository_namespace_override: Mutex<Option<String>>,
        returned_backend_id_override: Mutex<Option<String>>,
        upload_failures: Mutex<BTreeSet<LfsObject>>,
        upload_attempts: Mutex<Vec<LfsObject>>,
        upload_delay: Mutex<Option<Duration>>,
        active_uploads: AtomicUsize,
        max_active_uploads: AtomicUsize,
    }

    impl FakeMigrationStorageProvider {
        fn new(provider_id: impl Into<String>) -> Self {
            Self {
                provider_id: provider_id.into(),
                existing: Mutex::new(BTreeSet::new()),
                uploaded: Mutex::new(Vec::new()),
                returned_object_override: Mutex::new(None),
                returned_provider_id_override: Mutex::new(None),
                returned_repository_namespace_override: Mutex::new(None),
                returned_backend_id_override: Mutex::new(None),
                upload_failures: Mutex::new(BTreeSet::new()),
                upload_attempts: Mutex::new(Vec::new()),
                upload_delay: Mutex::new(None),
                active_uploads: AtomicUsize::new(0),
                max_active_uploads: AtomicUsize::new(0),
            }
        }

        fn insert_existing(&self, object: LfsObject) {
            self.existing
                .lock()
                .expect("fake storage lock should not poison")
                .insert((TEST_REPOSITORY_NAMESPACE.to_owned(), object));
        }

        fn uploaded_objects(&self) -> Vec<LfsObject> {
            self.uploaded
                .lock()
                .expect("fake upload lock should not poison")
                .clone()
        }

        fn fail_upload(&self, object: LfsObject) {
            self.upload_failures
                .lock()
                .expect("fake failure lock should not poison")
                .insert(object);
        }

        fn upload_attempts(&self) -> Vec<LfsObject> {
            self.upload_attempts
                .lock()
                .expect("fake attempt lock should not poison")
                .clone()
        }

        fn delay_uploads_by(&self, delay: Duration) {
            *self
                .upload_delay
                .lock()
                .expect("fake delay lock should not poison") = Some(delay);
        }

        fn max_active_uploads(&self) -> usize {
            self.max_active_uploads.load(Ordering::SeqCst)
        }

        fn override_returned_object(&self, object: LfsObject) {
            *self
                .returned_object_override
                .lock()
                .expect("fake override lock should not poison") = Some(object);
        }

        fn override_returned_provider_id(&self, provider_id: impl Into<String>) {
            *self
                .returned_provider_id_override
                .lock()
                .expect("fake provider override lock should not poison") = Some(provider_id.into());
        }

        fn override_returned_repository_namespace(&self, repository_namespace: impl Into<String>) {
            *self
                .returned_repository_namespace_override
                .lock()
                .expect("fake namespace override lock should not poison") =
                Some(repository_namespace.into());
        }

        fn override_returned_backend_id(&self, backend_id: impl Into<String>) {
            *self
                .returned_backend_id_override
                .lock()
                .expect("fake backend override lock should not poison") = Some(backend_id.into());
        }
    }

    impl StorageProvider for FakeMigrationStorageProvider {
        fn provider_id(&self) -> &str {
            &self.provider_id
        }

        fn lookup_object<'a>(
            &'a self,
            repository_namespace: &'a str,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<Option<StoredObject>>> {
            Box::pin(async move {
                Ok(self
                    .existing
                    .lock()
                    .expect("fake storage lock should not poison")
                    .contains(&(repository_namespace.to_owned(), object.clone()))
                    .then(|| {
                        StoredObject::new(
                            self.provider_id.clone(),
                            repository_namespace,
                            object.clone(),
                            format!("fake-storage-{}", object.oid),
                        )
                    }))
            })
        }

        fn upload_object<'a>(
            &'a self,
            repository_namespace: &'a str,
            object: &'a LfsObject,
            source: &'a Path,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                self.upload_attempts
                    .lock()
                    .expect("fake attempt lock should not poison")
                    .push(object.clone());
                let active_uploads = self.active_uploads.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_active_uploads
                    .fetch_max(active_uploads, Ordering::SeqCst);
                let delay = *self
                    .upload_delay
                    .lock()
                    .expect("fake delay lock should not poison");
                if let Some(delay) = delay {
                    tokio::time::sleep(delay).await;
                }
                self.active_uploads.fetch_sub(1, Ordering::SeqCst);

                if self
                    .upload_failures
                    .lock()
                    .expect("fake failure lock should not poison")
                    .contains(object)
                {
                    return Err(StorageError::Retryable {
                        provider: self.provider_id.clone(),
                        message: "simulated migration upload failure".to_owned(),
                    });
                }

                let (actual_oid, actual_size) =
                    hash_migration_object_file(source).map_err(|source| {
                        StorageError::IntegrityMismatch {
                            expected_oid: object.oid.as_hex().to_owned(),
                            expected_size: object.size.bytes(),
                            actual_oid: format!("migration-source-error:{source}"),
                            actual_size: 0,
                        }
                    })?;

                if actual_oid != object.oid || actual_size != object.size {
                    return Err(StorageError::IntegrityMismatch {
                        expected_oid: object.oid.as_hex().to_owned(),
                        expected_size: object.size.bytes(),
                        actual_oid: actual_oid.as_hex().to_owned(),
                        actual_size: actual_size.bytes(),
                    });
                }

                self.uploaded
                    .lock()
                    .expect("fake upload lock should not poison")
                    .push(object.clone());
                self.existing
                    .lock()
                    .expect("fake storage lock should not poison")
                    .insert((repository_namespace.to_owned(), object.clone()));

                let returned_object = self
                    .returned_object_override
                    .lock()
                    .expect("fake override lock should not poison")
                    .clone()
                    .unwrap_or_else(|| object.clone());
                let returned_provider_id = self
                    .returned_provider_id_override
                    .lock()
                    .expect("fake provider override lock should not poison")
                    .clone()
                    .unwrap_or_else(|| self.provider_id.clone());
                let returned_repository_namespace = self
                    .returned_repository_namespace_override
                    .lock()
                    .expect("fake namespace override lock should not poison")
                    .clone()
                    .unwrap_or_else(|| repository_namespace.to_owned());
                let returned_backend_id = self
                    .returned_backend_id_override
                    .lock()
                    .expect("fake backend override lock should not poison")
                    .clone()
                    .unwrap_or_else(|| format!("fake-storage-{}", object.oid));

                Ok(StoredObject::new(
                    returned_provider_id,
                    returned_repository_namespace,
                    returned_object,
                    returned_backend_id,
                ))
            })
        }

        fn download_object<'a>(
            &'a self,
            repository_namespace: &'a str,
            object: &'a LfsObject,
            _destination: &'a Path,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                if self.object_exists(repository_namespace, object).await? {
                    Ok(StoredObject::new(
                        self.provider_id.clone(),
                        repository_namespace,
                        object.clone(),
                        format!("fake-storage-{}", object.oid),
                    ))
                } else {
                    Err(StorageError::ObjectNotFound {
                        provider: self.provider_id.clone(),
                        oid: object.oid.as_hex().to_owned(),
                        size: object.size.bytes(),
                    })
                }
            })
        }

        fn delete_or_mark_object<'a>(
            &'a self,
            repository_namespace: &'a str,
            object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
            Box::pin(async move {
                self.existing
                    .lock()
                    .expect("fake storage lock should not poison")
                    .remove(&(repository_namespace.to_owned(), object.clone()));
                Ok(StorageDeleteOutcome::Deleted)
            })
        }
    }

    #[tokio::test]
    async fn upload_migration_objects_skips_existing_and_uploads_verified_sources() {
        let repo = TempRepo::new();
        let already_present = test_lfs_object_from_bytes(b"already stored migration bytes");
        let missing = test_lfs_object_from_bytes(b"new migration upload bytes");
        write_git_lfs_source_object(&repo, &already_present, b"already stored migration bytes");
        write_git_lfs_source_object(&repo, &missing, b"new migration upload bytes");
        let availability =
            check_local_migration_objects(repo.path(), [&already_present, &missing], None)
                .expect("local migration objects should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.insert_existing(already_present.clone());

        let report =
            upload_migration_objects_to_storage(&availability, &storage, TEST_REPOSITORY_NAMESPACE)
                .await
                .expect("available migration objects should upload");

        assert_eq!(report.storage_provider_id, "drive-user-a");
        assert_eq!(report.repository_namespace, TEST_REPOSITORY_NAMESPACE);
        assert_eq!(
            report.already_present_objects,
            vec![already_present.clone()]
        );
        assert_eq!(report.uploaded_objects.len(), 1);
        assert_eq!(report.uploaded_objects[0].object, missing);
        assert_eq!(storage.uploaded_objects(), vec![missing]);
        assert!(
            storage
                .object_exists(TEST_REPOSITORY_NAMESPACE, &already_present)
                .await
                .expect("exists check should succeed")
        );
    }

    #[tokio::test]
    async fn migration_uploads_use_bounded_concurrency() {
        let repo = TempRepo::new();
        let objects = [
            test_lfs_object_from_bytes(b"parallel migration object one"),
            test_lfs_object_from_bytes(b"parallel migration object two"),
            test_lfs_object_from_bytes(b"parallel migration object three"),
        ];
        for (object, bytes) in objects.iter().zip([
            b"parallel migration object one".as_slice(),
            b"parallel migration object two".as_slice(),
            b"parallel migration object three".as_slice(),
        ]) {
            write_git_lfs_source_object(&repo, object, bytes);
        }
        let availability = check_local_migration_objects(repo.path(), &objects, None)
            .expect("migration objects should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.delay_uploads_by(Duration::from_millis(50));
        let options = MigrationStorageUploadOptions::new(repo.path().join("checkpoint.jsonl"))
            .with_max_concurrent_uploads(2);

        let report = upload_migration_objects_to_storage_with_options(
            &availability,
            &storage,
            TEST_REPOSITORY_NAMESPACE,
            &options,
        )
        .await
        .expect("bounded migration uploads should complete");

        assert!(report.failed_objects.is_empty());
        assert_eq!(report.uploaded_objects.len(), 3);
        assert_eq!(storage.max_active_uploads(), 2);
    }

    #[tokio::test]
    async fn migration_uploads_checkpoint_successes_and_retry_failures() {
        let repo = TempRepo::new();
        let completed = test_lfs_object_from_bytes(b"durably completed migration object");
        let failed = test_lfs_object_from_bytes(b"retryable migration object");
        write_git_lfs_source_object(&repo, &completed, b"durably completed migration object");
        write_git_lfs_source_object(&repo, &failed, b"retryable migration object");
        let availability = check_local_migration_objects(repo.path(), [&completed, &failed], None)
            .expect("migration objects should be available");
        let checkpoint_path = repo.path().join("checkpoint.jsonl");
        let options =
            MigrationStorageUploadOptions::new(&checkpoint_path).with_max_concurrent_uploads(2);
        let first_storage = FakeMigrationStorageProvider::new("drive-user-a");
        first_storage.fail_upload(failed.clone());

        let first_report = upload_migration_objects_to_storage_with_options(
            &availability,
            &first_storage,
            TEST_REPOSITORY_NAMESPACE,
            &options,
        )
        .await
        .expect("one object failure should still return accumulated outcomes");

        assert_eq!(first_report.uploaded_objects.len(), 1);
        assert_eq!(first_report.failed_objects.len(), 1);
        assert_eq!(first_report.failed_objects[0].object, failed);
        assert!(checkpoint_path.is_file());

        let wrong_namespace_storage = FakeMigrationStorageProvider::new("drive-user-a");
        let error = upload_migration_objects_to_storage_with_options(
            &availability,
            &wrong_namespace_storage,
            "github-main:owner/other",
            &options,
        )
        .await
        .expect_err("another repository must not resume this checkpoint");
        assert!(error.to_string().contains("different repository namespace"));

        let resumed_storage = FakeMigrationStorageProvider::new("drive-user-a");
        resumed_storage.insert_existing(completed.clone());
        let resumed_report = upload_migration_objects_to_storage_with_options(
            &availability,
            &resumed_storage,
            TEST_REPOSITORY_NAMESPACE,
            &options,
        )
        .await
        .expect("a resumed upload should reuse durable completions");

        assert!(resumed_report.failed_objects.is_empty());
        assert_eq!(resumed_storage.upload_attempts(), vec![failed.clone()]);
        assert!(matches!(
            resumed_report.outcomes[0].status,
            MigrationObjectUploadStatus::Uploaded { resumed: true, .. }
        ));
        assert!(matches!(
            resumed_report.outcomes[1].status,
            MigrationObjectUploadStatus::Uploaded { resumed: false, .. }
        ));

        let replacement_storage = FakeMigrationStorageProvider::new("drive-user-a");
        let replacement_report = upload_migration_objects_to_storage_with_options(
            &availability,
            &replacement_storage,
            TEST_REPOSITORY_NAMESPACE,
            &options,
        )
        .await
        .expect("missing checkpointed objects should be uploaded again");

        assert!(replacement_report.failed_objects.is_empty());
        assert_eq!(
            replacement_storage
                .upload_attempts()
                .into_iter()
                .collect::<BTreeSet<_>>(),
            [completed.clone(), failed.clone()]
                .into_iter()
                .collect::<BTreeSet<_>>()
        );
        assert!(replacement_report.outcomes.iter().all(|outcome| matches!(
            outcome.status,
            MigrationObjectUploadStatus::Uploaded { resumed: false, .. }
        )));
    }

    #[tokio::test]
    async fn upload_migration_objects_rechecks_source_bytes_before_upload() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"stable source bytes");
        write_git_lfs_source_object(&repo, &object, b"stable source bytes");
        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local migration object should be available before mutation");
        write_git_lfs_source_object(&repo, &object, b"corrupt source bytes");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");

        let report =
            upload_migration_objects_to_storage(&availability, &storage, TEST_REPOSITORY_NAMESPACE)
                .await
                .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("no longer matches")
        );
        assert!(storage.uploaded_objects().is_empty());
    }

    #[tokio::test]
    async fn upload_migration_objects_rejects_returned_object_mismatch() {
        let repo = TempRepo::new();
        let requested = test_lfs_object_from_bytes(b"requested upload bytes");
        let returned = test_lfs_object_from_bytes(b"different returned object");
        write_git_lfs_source_object(&repo, &requested, b"requested upload bytes");
        let availability = check_local_migration_objects(repo.path(), [&requested], None)
            .expect("local migration object should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.override_returned_object(returned);

        let report =
            upload_migration_objects_to_storage(&availability, &storage, TEST_REPOSITORY_NAMESPACE)
                .await
                .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("returned object")
        );
    }

    #[tokio::test]
    async fn upload_migration_objects_rejects_provider_id_mismatch() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"provider mismatch bytes");
        write_git_lfs_source_object(&repo, &object, b"provider mismatch bytes");
        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local migration object should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.override_returned_provider_id("drive-user-b");

        let report =
            upload_migration_objects_to_storage(&availability, &storage, TEST_REPOSITORY_NAMESPACE)
                .await
                .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("returned provider ID drive-user-b")
        );
    }

    #[tokio::test]
    async fn upload_migration_objects_rejects_repository_namespace_mismatch() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"namespace mismatch bytes");
        write_git_lfs_source_object(&repo, &object, b"namespace mismatch bytes");
        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local migration object should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.override_returned_repository_namespace("github-main:owner/other");

        let report =
            upload_migration_objects_to_storage(&availability, &storage, TEST_REPOSITORY_NAMESPACE)
                .await
                .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("different repository namespace")
        );
    }

    #[tokio::test]
    async fn upload_migration_objects_rejects_empty_backend_id() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"empty backend id bytes");
        write_git_lfs_source_object(&repo, &object, b"empty backend id bytes");
        let availability = check_local_migration_objects(repo.path(), [&object], None)
            .expect("local migration object should be available");
        let storage = FakeMigrationStorageProvider::new("drive-user-a");
        storage.override_returned_backend_id(" ");

        let report =
            upload_migration_objects_to_storage(&availability, &storage, TEST_REPOSITORY_NAMESPACE)
                .await
                .expect("per-object failures should return a retry report");

        assert_eq!(report.failed_objects.len(), 1);
        assert!(
            report.failed_objects[0]
                .message
                .as_str()
                .contains("empty backend object ID")
        );
    }

    #[test]
    fn migration_upload_source_prefers_git_lfs_media_over_shared_cache() {
        let temp = tempfile::tempdir().expect("temporary object paths should be created");
        let object = test_lfs_object_from_bytes(b"source preference bytes");
        let shared_cache_path = temp.path().join("shared-cache-object");
        let git_lfs_media_path = temp.path().join("git-lfs-media-object");
        write_file(&shared_cache_path, b"source preference bytes");
        write_file(&git_lfs_media_path, b"source preference bytes");
        let local_object = LocalMigrationObject {
            object,
            locations: vec![
                LocalMigrationObjectLocation {
                    kind: LocalMigrationObjectLocationKind::SharedCache,
                    path: shared_cache_path,
                    status: LocalMigrationObjectLocationStatus::Available,
                },
                LocalMigrationObjectLocation {
                    kind: LocalMigrationObjectLocationKind::GitLfsMedia,
                    path: git_lfs_media_path.clone(),
                    status: LocalMigrationObjectLocationStatus::Available,
                },
            ],
        };

        let selected = verified_migration_upload_source_path(&local_object)
            .expect("available migration source should be selected");

        assert_eq!(selected, git_lfs_media_path);
    }

}
