struct StagedUpload {
    _lease: UploadStagingLease,
    temp_file: tempfile::NamedTempFile,
}

impl StagedUpload {
    fn path(&self) -> &Path {
        self.temp_file.path()
    }
}

#[cfg(test)]
async fn stage_upload_request_body(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
) -> Result<StagedUpload, UploadStagingError> {
    stage_upload_request_body_with_limit(
        expected_oid,
        expected_size,
        request,
        MAX_UPLOAD_OBJECT_BYTES,
    )
    .await
}

#[cfg(test)]
async fn stage_upload_request_body_with_limit(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    max_upload_bytes: u64,
) -> Result<StagedUpload, UploadStagingError> {
    let coordinator = UploadStagingCoordinator::new(1, 1);
    let lease = coordinator.try_acquire("standalone")?;
    stage_upload_request_body_with_lease(
        expected_oid,
        expected_size,
        request,
        UploadStagingGuardrails {
            max_upload_bytes,
            ..UploadStagingGuardrails::default()
        },
        lease,
    )
    .await
}

#[derive(Clone, Copy, Debug)]
struct UploadStagingGuardrails {
    max_upload_bytes: u64,
    min_free_bytes: u64,
    idle_timeout: Duration,
}

impl Default for UploadStagingGuardrails {
    fn default() -> Self {
        Self {
            max_upload_bytes: MAX_UPLOAD_OBJECT_BYTES,
            min_free_bytes: MIN_UPLOAD_STAGING_FREE_BYTES,
            idle_timeout: UPLOAD_STAGING_IDLE_TIMEOUT,
        }
    }
}

#[cfg(test)]
async fn stage_upload_request_body_with_guardrails(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    guardrails: UploadStagingGuardrails,
) -> Result<StagedUpload, UploadStagingError> {
    let coordinator = UploadStagingCoordinator::new(1, 1);
    let lease = coordinator.try_acquire("standalone")?;
    stage_upload_request_body_with_lease(expected_oid, expected_size, request, guardrails, lease)
        .await
}

async fn stage_upload_request_body_with_lease(
    expected_oid: &LfsOid,
    expected_size: Option<u64>,
    request: Request,
    guardrails: UploadStagingGuardrails,
    lease: UploadStagingLease,
) -> Result<StagedUpload, UploadStagingError> {
    let preflight_size = upload_staging_preflight_size(expected_size, guardrails.max_upload_bytes)?;
    let temp_file = tempfile::Builder::new()
        .prefix("lfscloud-upload-")
        .tempfile()
        .map_err(|source| StorageError::Retryable {
            provider: "lfscloud".to_owned(),
            message: format!("upload staging file could not be created: {source}"),
        })?;
    let staging_dir = temp_file
        .path()
        .parent()
        .ok_or_else(|| StorageError::Retryable {
            provider: "lfscloud".to_owned(),
            message: format!(
                "upload staging file {} did not have a parent directory",
                temp_file.path().display()
            ),
        })?;
    // Unknown-size helper callers reserve the full effective upload limit so
    // they cannot skip the temp-space guardrail before streaming begins.
    let lease = lease
        .reserve(staging_dir, preflight_size, guardrails.min_free_bytes)
        .await?;

    let std_file = temp_file
        .reopen()
        .map_err(|source| StorageError::StagedFileRead {
            provider: "lfscloud".to_owned(),
            path: temp_file.path().to_path_buf(),
            source,
        })?;
    let mut file = tokio::fs::File::from_std(std_file);
    let mut stream = request.into_body().into_data_stream();
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;

    loop {
        let Some(chunk) = tokio::time::timeout(guardrails.idle_timeout, stream.next())
            .await
            .map_err(|_| UploadStagingError::TimedOut)?
        else {
            break;
        };
        let chunk = chunk.map_err(|source| StorageError::Retryable {
            provider: "lfscloud".to_owned(),
            message: format!("upload request body could not be read: {source}"),
        })?;
        let next_size = actual_size
            .checked_add(chunk.len() as u64)
            .ok_or(UploadStagingError::PayloadTooLarge)?;
        if next_size > guardrails.max_upload_bytes {
            return Err(UploadStagingError::PayloadTooLarge);
        }
        hasher.update(&chunk);
        actual_size = next_size;
        file.write_all(&chunk)
            .await
            .map_err(|source| upload_staging_file_io_error(source, "written"))?;
    }
    file.flush()
        .await
        .map_err(|source| upload_staging_file_io_error(source, "flushed"))?;
    drop(file);

    let actual_oid = format!("{:x}", hasher.finalize());
    if let Some(expected_size) = expected_size
        && expected_size != actual_size
    {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: expected_oid.as_hex().to_owned(),
            expected_size,
            actual_oid,
            actual_size,
        }
        .into());
    }

    if actual_oid != expected_oid.as_hex() {
        return Err(StorageError::IntegrityMismatch {
            expected_oid: expected_oid.as_hex().to_owned(),
            expected_size: expected_size.unwrap_or(actual_size),
            actual_oid,
            actual_size,
        }
        .into());
    }

    Ok(StagedUpload {
        _lease: lease,
        temp_file,
    })
}

fn upload_staging_preflight_size(
    expected_size: Option<u64>,
    max_upload_bytes: u64,
) -> Result<u64, UploadStagingError> {
    let size = expected_size.unwrap_or(max_upload_bytes);
    if size > max_upload_bytes {
        return Err(UploadStagingError::PayloadTooLarge);
    }

    Ok(size)
}

#[derive(Clone)]
struct UploadStagingCoordinator {
    global_slots: Arc<Semaphore>,
    per_user_limit: usize,
    per_user_slots: Arc<std::sync::Mutex<HashMap<String, Weak<Semaphore>>>>,
    reservations: Arc<std::sync::Mutex<UploadStagingReservationState>>,
}

#[derive(Default)]
struct UploadStagingReservationState {
    available_space_snapshot: Option<u64>,
    reserved_bytes: u64,
}

impl UploadStagingCoordinator {
    fn new(global_limit: usize, per_user_limit: usize) -> Self {
        Self {
            global_slots: Arc::new(Semaphore::new(global_limit)),
            per_user_limit,
            per_user_slots: Arc::new(std::sync::Mutex::new(HashMap::new())),
            reservations: Arc::new(std::sync::Mutex::new(
                UploadStagingReservationState::default(),
            )),
        }
    }

    fn try_acquire(&self, principal: &str) -> Result<UploadStagingLease, UploadStagingError> {
        let global_permit = self
            .global_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| UploadStagingError::ConcurrencyLimit)?;
        let user_slots = {
            let mut slots = self
                .per_user_slots
                .lock()
                .expect("upload staging user-slot map should not be poisoned");
            // Weak entries avoid turning one-off authenticated users into a
            // process-lifetime map while preserving one semaphore per active
            // principal across concurrent admission attempts.
            slots.retain(|_, semaphore| semaphore.strong_count() > 0);
            match slots.get(principal).and_then(Weak::upgrade) {
                Some(semaphore) => semaphore,
                None => {
                    let semaphore = Arc::new(Semaphore::new(self.per_user_limit));
                    slots.insert(principal.to_owned(), Arc::downgrade(&semaphore));
                    semaphore
                }
            }
        };
        let user_permit = user_slots
            .try_acquire_owned()
            .map_err(|_| UploadStagingError::ConcurrencyLimit)?;

        Ok(UploadStagingLease {
            coordinator: self.clone(),
            _global_permit: global_permit,
            _user_permit: user_permit,
            reservation: None,
        })
    }

    fn reserve_with_available_space(
        &self,
        expected_size: u64,
        min_free_bytes: u64,
        available_space: u64,
    ) -> Result<UploadStagingDiskReservation, UploadStagingError> {
        let mut state = self
            .reservations
            .lock()
            .expect("upload staging reservation state should not be poisoned");
        let request_required = expected_size.checked_add(min_free_bytes).ok_or(
            UploadStagingError::InsufficientTempSpace {
                required_space: None,
                available_space: Some(available_space),
            },
        )?;
        if available_space < request_required {
            return Err(UploadStagingError::InsufficientTempSpace {
                required_space: Some(request_required),
                available_space: Some(available_space),
            });
        }

        // Freeze one capacity snapshot while any managed staging file is
        // alive. Every declared size spends that shared budget atomically;
        // the per-request live check above remains a secondary signal for
        // unrelated filesystem pressure.
        let snapshot = *state
            .available_space_snapshot
            .get_or_insert(available_space);
        let aggregate_required = state
            .reserved_bytes
            .checked_add(expected_size)
            .and_then(|reserved| reserved.checked_add(min_free_bytes))
            .ok_or(UploadStagingError::InsufficientTempSpace {
                required_space: None,
                available_space: Some(snapshot),
            })?;
        if snapshot < aggregate_required {
            return Err(UploadStagingError::InsufficientTempSpace {
                required_space: Some(aggregate_required),
                available_space: Some(snapshot),
            });
        }

        state.reserved_bytes = state
            .reserved_bytes
            .checked_add(expected_size)
            .expect("validated upload staging reservation should not overflow");
        Ok(UploadStagingDiskReservation {
            bytes: expected_size,
            reservations: self.reservations.clone(),
        })
    }
}

struct UploadStagingLease {
    coordinator: UploadStagingCoordinator,
    _global_permit: OwnedSemaphorePermit,
    _user_permit: OwnedSemaphorePermit,
    reservation: Option<UploadStagingDiskReservation>,
}

impl UploadStagingLease {
    async fn reserve(
        self,
        staging_dir: &Path,
        expected_size: u64,
        min_free_bytes: u64,
    ) -> Result<Self, UploadStagingError> {
        let staging_dir = staging_dir.to_path_buf();
        let available_space =
            tokio::task::spawn_blocking(move || fs4::available_space(staging_dir))
                .await
                .map_err(|source| StorageError::Retryable {
                    provider: "lfscloud".to_owned(),
                    message: format!(
                        "upload staging directory free-space check did not complete: {source}"
                    ),
                })?
                .map_err(|source| StorageError::Retryable {
                    provider: "lfscloud".to_owned(),
                    message: format!(
                        "upload staging directory free space could not be inspected: {source}"
                    ),
                })?;

        self.reserve_with_available_space(expected_size, min_free_bytes, available_space)
    }

    fn reserve_with_available_space(
        mut self,
        expected_size: u64,
        min_free_bytes: u64,
        available_space: u64,
    ) -> Result<Self, UploadStagingError> {
        let reservation = self.coordinator.reserve_with_available_space(
            expected_size,
            min_free_bytes,
            available_space,
        )?;
        self.reservation = Some(reservation);
        Ok(self)
    }
}

struct UploadStagingDiskReservation {
    bytes: u64,
    reservations: Arc<std::sync::Mutex<UploadStagingReservationState>>,
}

impl Drop for UploadStagingDiskReservation {
    fn drop(&mut self) {
        let mut state = self
            .reservations
            .lock()
            .expect("upload staging reservation state should not be poisoned");
        state.reserved_bytes = state
            .reserved_bytes
            .checked_sub(self.bytes)
            .expect("upload staging reservations should release exactly once");
        if state.reserved_bytes == 0 {
            state.available_space_snapshot = None;
        }
    }
}

fn upload_staging_file_io_error(source: io::Error, action: &str) -> UploadStagingError {
    if is_temp_space_exhausted(&source) {
        return UploadStagingError::InsufficientTempSpace {
            required_space: None,
            available_space: None,
        };
    }

    StorageError::Retryable {
        provider: "lfscloud".to_owned(),
        message: format!("upload staging file could not be {action}: {source}"),
    }
    .into()
}

fn is_temp_space_exhausted(source: &io::Error) -> bool {
    matches!(
        source.kind(),
        ErrorKind::StorageFull | ErrorKind::QuotaExceeded
    ) || matches!(
        source.raw_os_error(),
        // ENOSPC on Unix, EDQUOT on Linux, and EDQUOT on Darwin/BSD.
        Some(28) | Some(122) | Some(69)
    )
}

#[derive(Debug)]
enum UploadStagingError {
    PayloadTooLarge,
    ConcurrencyLimit,
    InsufficientTempSpace {
        required_space: Option<u64>,
        available_space: Option<u64>,
    },
    TimedOut,
    Storage(StorageError),
}

impl UploadStagingError {
    fn into_storage_error(self) -> StorageError {
        match self {
            Self::PayloadTooLarge => StorageError::QuotaExceeded {
                provider: "lfscloud".to_owned(),
                message: "upload object exceeded request size limit".to_owned(),
            },
            Self::ConcurrencyLimit => StorageError::Retryable {
                provider: "lfscloud".to_owned(),
                message: "upload staging concurrency limit reached".to_owned(),
            },
            Self::InsufficientTempSpace {
                required_space,
                available_space,
            } => {
                let message = match (required_space, available_space) {
                    (Some(required_space), Some(available_space)) => format!(
                        "upload staging directory has {available_space} bytes available but requires {required_space} bytes"
                    ),
                    (None, Some(available_space)) => format!(
                        "upload staging directory has {available_space} bytes available but required space exceeds supported size"
                    ),
                    _ => "upload staging directory does not have enough free space".to_owned(),
                };

                StorageError::QuotaExceeded {
                    provider: "lfscloud".to_owned(),
                    message,
                }
            }
            Self::TimedOut => StorageError::Retryable {
                provider: "lfscloud".to_owned(),
                message: "upload request body timed out while reading".to_owned(),
            },
            Self::Storage(error) => error,
        }
    }
}

impl From<StorageError> for UploadStagingError {
    fn from(error: StorageError) -> Self {
        Self::Storage(error)
    }
}
