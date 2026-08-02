//! Git LFS migration planning, readiness checks, execution, and reporting.

use super::*;

const MIGRATION_TARGET_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MIGRATION_TARGET_RECONCILIATION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
const MIGRATION_TARGET_BATCH_SIZE: usize = 100;
const MIGRATION_TARGET_RESPONSE_LIMIT: usize = 4 * 1024 * 1024;
const MIGRATION_TARGET_UPLOAD_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const MIGRATION_OBJECT_REPORT_LIMIT: usize = 100;
const SOURCE_ENDPOINT_UNSET_LABEL: &str = "<unset>";
const SOURCE_PROVIDER_UNKNOWN_LABEL: &str = "unknown";

fn reject_migration_server_config_path(config_path: Option<&Path>) -> CliResult<()> {
    if config_path.is_none() {
        return Ok(());
    }
    Err(CliError::InvalidArguments {
        message: "migrate no longer reads --config; authenticate with `lfscloud login --server URL` and rerun without --config"
            .to_owned(),
    })
}

pub(super) fn run_migrate_to_stdout(
    command: MigrateCommand,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    if command.dry_run {
        return run_migrate_from_dir(
            command,
            config_path,
            &current_dir,
            &mut stdout,
            probe_server_reachable,
            |lfs_url| {
                GitCredentialLookup::new_with_insecure_http(lfs_url, true)
                    .and_then(|lookup| lookup.lookup().map(|_| ()))
            },
        )
        .map_err(anyhow::Error::from);
    }

    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(run_migrate_execution_from_dir(
            command,
            config_path,
            &current_dir,
            &mut stdout,
            probe_server_reachable,
            |lfs_url| {
                GitCredentialLookup::new_with_insecure_http(lfs_url, true)
                    .and_then(|lookup| lookup.lookup().map(|credential| credential.token().clone()))
            },
        ))
    })
    .map_err(anyhow::Error::from)
}

fn run_migrate_from_dir<W, P, A>(
    command: MigrateCommand,
    config_path: Option<PathBuf>,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut probe_server: P,
    mut lookup_credential: A,
) -> CliResult<()>
where
    W: Write,
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
{
    reject_migration_server_config_path(config_path.as_deref())?;
    if !command.dry_run {
        return Err(CliError::InvalidArguments {
            message: "the migration planning runner requires --dry-run".to_owned(),
        });
    }

    let start_dir = start_dir.as_ref();
    let repository = GitRepository::discover(start_dir)?;
    let source_repository = GitRepository::discover_with_remote(start_dir, &command.source_remote)?;
    if !same_repository_identity(&source_repository.remote, &repository.remote)
        && !command.allow_cross_remote
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "source remote {} identifies {}, but target remote {} identifies {}; rerun with --allow-cross-remote only after confirming this cross-repository migration",
                source_repository.remote.remote_name,
                source_repository.remote.repository_label(),
                repository.remote.remote_name,
                repository.remote.repository_label(),
            ),
        });
    }
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    let discovery = discover_git_lfs_migration_from_remote_excluding_endpoint(
        start_dir,
        &command.source_remote,
        Some(&route.lfs_url),
    )?;
    let scan = migration_pointer_scan(start_dir, &command, &command.source_remote)?;
    let cache_layout = Some(local_cache_layout(command.cache_root.clone())?);
    let availability =
        check_local_migration_objects(start_dir, scan.objects.iter(), cache_layout.as_ref())?;
    let readiness_checks = migration_readiness_checks(
        MigrationTargetReadiness {
            server_url: &command.server,
            lfs_url: &route.lfs_url,
        },
        &discovery,
        &mut probe_server,
        &mut lookup_credential,
    );
    let source_purge = migration_source_purge_report(&discovery, command.purge_source_lfs);
    let report = MigrationDryRunReport {
        discovery,
        source_remote: source_repository.remote,
        target_remote: repository.remote.clone(),
        scan,
        availability,
        route,
        readiness_checks,
        would_touch_files: migration_dry_run_touched_files(&repository)?,
        source_purge,
    };

    write_migration_dry_run_report(output, &report).map_err(output_error)
}

#[derive(Debug)]
struct MigrationExecutionPreparation {
    repository: GitRepository,
    source_remote: GitRemote,
    route: LfsInitRoute,
    discovery: GitLfsMigrationDiscovery,
    cache_layout: LocalCacheLayout,
    purge_source_lfs: bool,
    allow_insecure_http: bool,
}

impl MigrationExecutionPreparation {
    fn scan_fetched_refs(self) -> CliResult<MigrationExecutionContext> {
        let scan = history_pointer_scan(
            MigrationScanMode::AllFetchedRefs,
            enumerate_fetched_ref_lfs_pointers_for_remote(
                &self.repository.worktree_root,
                &self.source_remote.remote_name,
            )?,
        );
        Ok(MigrationExecutionContext {
            repository: self.repository,
            source_remote: self.source_remote,
            route: self.route,
            discovery: self.discovery,
            scan,
            cache_layout: self.cache_layout,
            purge_source_lfs: self.purge_source_lfs,
            allow_insecure_http: self.allow_insecure_http,
        })
    }
}

#[derive(Debug)]
struct MigrationExecutionContext {
    repository: GitRepository,
    source_remote: GitRemote,
    route: LfsInitRoute,
    discovery: GitLfsMigrationDiscovery,
    scan: MigrationPointerScan,
    cache_layout: LocalCacheLayout,
    purge_source_lfs: bool,
    allow_insecure_http: bool,
}

#[derive(Debug)]
struct MigrationExecutionResult {
    source_fetch: MigrationSourceFetch,
    server_upload: MigrationServerUpload,
    config_changes: Vec<GitLfsConfigChange>,
    legacy_source_configured: bool,
}

#[derive(Debug)]
struct MigrationServerUpload {
    uploaded_objects: Vec<LfsObject>,
    already_present_objects: Vec<LfsObject>,
}

#[derive(Debug)]
struct MigrationTargetPlan {
    uploads: Vec<(LfsObject, LfsBatchAction)>,
    already_present_objects: Vec<LfsObject>,
}

async fn run_migrate_execution_from_dir<W, P, A>(
    command: MigrateCommand,
    config_path: Option<PathBuf>,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut probe_server: P,
    mut lookup_credential: A,
) -> CliResult<()>
where
    W: Write,
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<LfsSessionToken>,
{
    reject_migration_server_config_path(config_path.as_deref())?;
    let preparation = prepare_migration_execution(command, start_dir.as_ref())?;

    // Prove the endpoint is usable before fetching source bytes or creating
    // target storage state. The credential was issued separately by `login`,
    // so checking it does not require changing the source LFS configuration.
    probe_server(&preparation.route.server_url)?;
    let token = lookup_credential(&preparation.route.lfs_url)?;
    probe_authenticated_migration_target(
        &preparation.route.lfs_url,
        preparation.allow_insecure_http,
        &token,
    )
    .await?;

    fetch_migration_git_refs(
        &preparation.repository.worktree_root,
        &preparation.source_remote.remote_name,
    )?;
    let context = preparation.scan_fetched_refs()?;
    let result = execute_migration_through_server(&context, &token).await?;
    write_migration_execution_report(output, &context, &result).map_err(output_error)
}

fn prepare_migration_execution(
    command: MigrateCommand,
    start_dir: &Path,
) -> CliResult<MigrationExecutionPreparation> {
    if command.dry_run {
        return Err(CliError::InvalidArguments {
            message: "migration execution cannot be prepared from a --dry-run request".to_owned(),
        });
    }
    if !command.all_refs {
        return Err(CliError::InvalidArguments {
            message: "migration execution requires --all-refs so reconfiguration cannot strand historical LFS objects; use --dry-run for narrower planning"
                .to_owned(),
        });
    }

    let repository = GitRepository::discover(start_dir)?;
    let source_repository = GitRepository::discover_with_remote(start_dir, &command.source_remote)?;
    if !same_repository_identity(&source_repository.remote, &repository.remote)
        && !command.allow_cross_remote
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "source remote {} identifies {}, but target remote {} identifies {}; rerun with --allow-cross-remote only after confirming this cross-repository migration",
                source_repository.remote.remote_name,
                source_repository.remote.repository_label(),
                repository.remote.remote_name,
                repository.remote.repository_label(),
            ),
        });
    }
    let route = LfsInitRoute::resolve_with_insecure_http(
        &command.server,
        &repository.remote,
        command.allow_insecure_http,
    )?;
    let discovery = discover_git_lfs_migration_from_remote_excluding_endpoint(
        start_dir,
        &command.source_remote,
        Some(&route.lfs_url),
    )?;
    if !discovery.installation.installed {
        return Err(CliError::InvalidArguments {
            message: "migration execution requires Git LFS; install it and run `git lfs install` before retrying"
                .to_owned(),
        });
    }
    Ok(MigrationExecutionPreparation {
        repository,
        source_remote: source_repository.remote,
        route,
        discovery,
        cache_layout: local_cache_layout(command.cache_root)?,
        purge_source_lfs: command.purge_source_lfs,
        allow_insecure_http: command.allow_insecure_http,
    })
}

async fn execute_migration_through_server(
    context: &MigrationExecutionContext,
    token: &LfsSessionToken,
) -> CliResult<MigrationExecutionResult> {
    if context.scan.objects.is_empty() {
        return Err(CliError::InvalidArguments {
            message: "migration found no non-empty Git LFS objects across the selected history"
                .to_owned(),
        });
    }
    let target_plan = request_migration_target_plan(
        &context.route.lfs_url,
        context.allow_insecure_http,
        token,
        &context.scan.objects,
    )
    .await?;
    let needed_objects = target_plan
        .uploads
        .iter()
        .map(|(object, _)| object)
        .collect::<Vec<_>>();
    let before = check_local_migration_objects(
        &context.repository.worktree_root,
        needed_objects.iter().copied(),
        Some(&context.cache_layout),
    )?;
    if !before.unavailable_objects().is_empty() && context.discovery.source_endpoint.is_none() {
        return Err(MigrationError::SourceEndpointMissing.into());
    }
    let source_fetch = match context.discovery.source_endpoint.as_ref() {
        Some(source) => fetch_missing_migration_objects_from_remote_at_endpoint(
            &context.repository.worktree_root,
            needed_objects.iter().copied(),
            Some(&context.cache_layout),
            &context.source_remote.remote_name,
            &source.url,
            context.allow_insecure_http,
            MigrationFetchMode::ObjectIds,
        )?,
        None => fetch_missing_migration_objects_from_remote(
            &context.repository.worktree_root,
            needed_objects.iter().copied(),
            Some(&context.cache_layout),
            &context.source_remote.remote_name,
            MigrationFetchMode::ObjectIds,
        )?,
    };
    if let Some(object) = source_fetch.unavailable_objects.first() {
        return Err(MigrationError::SourceObjectMissing {
            oid: object.oid.as_hex().to_owned(),
            size: object.size.bytes(),
        }
        .into());
    }

    let server_upload = upload_migration_objects_through_server(
        &context.route.lfs_url,
        context.allow_insecure_http,
        token,
        &source_fetch.after,
        target_plan,
    )
    .await?;

    let legacy_source_configured = if let Some(source) = context
        .discovery
        .source_endpoint
        .as_ref()
        .filter(|source| source.url != context.route.lfs_url)
    {
        let source_url = crate::migration::validated_migration_source_endpoint(
            &source.url,
            context.allow_insecure_http,
        )?;
        context
            .repository
            .write_worktree_remote_lfs_url(&context.source_remote.remote_name, source_url)?;
        true
    } else {
        false
    };

    // Persist both target forms only after every server-mediated upload has
    // completed. The local override keeps commits predating `.lfsconfig` usable.
    let config_changes = [
        GitLfsConfigTarget::WorktreeFile,
        GitLfsConfigTarget::LocalRepository,
    ]
    .into_iter()
    .map(|target| {
        context
            .repository
            .write_lfs_url(target, &context.route.lfs_url)
    })
    .collect::<CliResult<Vec<_>>>()?;

    Ok(MigrationExecutionResult {
        source_fetch,
        server_upload,
        config_changes,
        legacy_source_configured,
    })
}

async fn request_migration_target_plan(
    lfs_url: &str,
    allow_insecure_http: bool,
    token: &LfsSessionToken,
    objects: &[LfsObject],
) -> CliResult<MigrationTargetPlan> {
    let mut batch_url = crate::init::validate_server_url(lfs_url, allow_insecure_http)?;
    append_url_path_segments(&mut batch_url, "objects/batch")?;
    let client = redirect_free_http_client("failed to create migration batch client")?;
    let mut uploads = Vec::new();
    let mut already_present_objects = Vec::new();

    let mut pending_batches = objects
        .chunks(MIGRATION_TARGET_BATCH_SIZE)
        .map(<[LfsObject]>::to_vec)
        .collect::<std::collections::VecDeque<_>>();
    while let Some(chunk) = pending_batches.pop_front() {
        let request = LfsBatchRequest {
            operation: LfsBatchOperation::Upload,
            transfers: vec![LFS_BASIC_TRANSFER.to_owned()],
            ref_context: None,
            hash_algo: LfsBatchHashAlgorithm::Sha256,
            objects: chunk.clone(),
        };
        let response = client
            .post(batch_url.clone())
            .bearer_auth(token.as_str())
            .header("Accept", "application/vnd.git-lfs+json")
            .header("Content-Type", "application/vnd.git-lfs+json")
            .json(&request)
            .timeout(MIGRATION_TARGET_RECONCILIATION_TIMEOUT)
            .send()
            .await
            .map_err(|source| CliError::Io {
                context: "failed to reconcile migration objects with LFS Cloud".to_owned(),
                source: io::Error::other(source),
            })?;
        let status = response.status();
        let body = crate::http_transport::read_bounded_lossy_response_body(
            response,
            MIGRATION_TARGET_RESPONSE_LIMIT,
        )
        .await
        .map_err(|source| CliError::Io {
            context: "failed to read the LFS Cloud batch response".to_owned(),
            source: io::Error::other(source),
        })?;
        if status == HttpStatusCode::PAYLOAD_TOO_LARGE && chunk.len() > 1 {
            let split = chunk.len() / 2;
            pending_batches.push_front(chunk[split..].to_vec());
            pending_batches.push_front(chunk[..split].to_vec());
            continue;
        }
        if !status.is_success() {
            return Err(CliError::ExternalCommandOutput {
                command: "migration target upload batch".to_owned(),
                message: SanitizedMessage::new(format!(
                    "server returned HTTP status {}: {}",
                    status.as_u16(),
                    sanitized_migration_http_body(&body)
                )),
            });
        }
        let response: LfsBatchResponse =
            serde_json::from_str(&body).map_err(|source| CliError::ExternalCommandOutput {
                command: "migration target upload batch".to_owned(),
                message: SanitizedMessage::new(format!(
                    "server returned invalid Git LFS batch JSON: {source}"
                )),
            })?;
        if response.transfer != LFS_BASIC_TRANSFER {
            return Err(CliError::ExternalCommandOutput {
                command: "migration target upload batch".to_owned(),
                message: SanitizedMessage::new(
                    "server selected an unsupported Git LFS transfer adapter",
                ),
            });
        }

        let mut expected = chunk.iter().cloned().collect::<BTreeSet<_>>();
        for result in response.objects {
            let object = LfsObject::new(result.oid, result.size);
            if !expected.remove(&object) {
                return Err(CliError::ExternalCommandOutput {
                    command: "migration target upload batch".to_owned(),
                    message: SanitizedMessage::new(
                        "server returned an unexpected or duplicate migration object",
                    ),
                });
            }
            if let Some(error) = result.error {
                return Err(CliError::MigrationUploadFailed {
                    failures: 1,
                    oid: object.oid.as_hex().to_owned(),
                    message: SanitizedMessage::new(format!(
                        "server rejected the object with code {}: {}",
                        error.code, error.message
                    )),
                });
            }
            match result.actions.get("upload") {
                Some(action) => uploads.push((object, action.clone())),
                None if result.actions.is_empty() => already_present_objects.push(object),
                None => {
                    return Err(CliError::ExternalCommandOutput {
                        command: "migration target upload batch".to_owned(),
                        message: SanitizedMessage::new(
                            "server returned migration actions without an upload action",
                        ),
                    });
                }
            }
        }
        if !expected.is_empty() {
            return Err(CliError::ExternalCommandOutput {
                command: "migration target upload batch".to_owned(),
                message: SanitizedMessage::new(
                    "server omitted one or more requested migration objects",
                ),
            });
        }
    }

    Ok(MigrationTargetPlan {
        uploads,
        already_present_objects,
    })
}

async fn upload_migration_objects_through_server(
    lfs_url: &str,
    allow_insecure_http: bool,
    token: &LfsSessionToken,
    availability: &LocalMigrationObjectAvailability,
    plan: MigrationTargetPlan,
) -> CliResult<MigrationServerUpload> {
    let target_url = Url::parse(lfs_url).map_err(|_| CliError::InvalidArguments {
        message: "migration target LFS URL is invalid".to_owned(),
    })?;
    let client = redirect_free_http_client("failed to create migration upload client")?;
    let mut uploaded_objects = Vec::new();

    for (object, action) in plan.uploads {
        let local_object = availability
            .objects
            .iter()
            .find(|local| local.object == object)
            .ok_or_else(|| MigrationError::SourceObjectMissing {
                oid: object.oid.as_hex().to_owned(),
                size: object.size.bytes(),
            })?;
        let source = crate::migration::verified_migration_upload_source_path(local_object)?;
        crate::migration::verify_migration_upload_source(source, &object).await?;
        let upload_url = validated_migration_upload_action_url(
            &target_url,
            &action.href,
            allow_insecure_http,
            &object,
        )?;
        let headers = migration_action_headers(&action, token)?;
        let file = tokio::fs::File::open(source)
            .await
            .map_err(|source_error| CliError::Io {
                context: format!("failed to open migration object sha256:{}", object.oid),
                source: source_error,
            })?;
        let response = client
            .put(upload_url)
            .headers(headers)
            .header(CONTENT_LENGTH, object.size.bytes())
            .body(ReqwestBody::wrap_stream(ReaderStream::new(file)))
            .timeout(MIGRATION_TARGET_UPLOAD_TIMEOUT)
            .send()
            .await
            .map_err(|source| CliError::Io {
                context: format!(
                    "failed to upload migration object sha256:{} through LFS Cloud",
                    object.oid
                ),
                source: io::Error::other(source),
            })?;
        let status = response.status();
        if !status.is_success() {
            let body = crate::http_transport::read_bounded_lossy_response_body(
                response,
                MIGRATION_TARGET_RESPONSE_LIMIT,
            )
            .await
            .map_err(|source| CliError::Io {
                context: "failed to read the LFS Cloud upload response".to_owned(),
                source: io::Error::other(source),
            })?;
            return Err(CliError::MigrationUploadFailed {
                failures: 1,
                oid: object.oid.as_hex().to_owned(),
                message: SanitizedMessage::new(format!(
                    "server returned HTTP status {}: {}",
                    status.as_u16(),
                    sanitized_migration_http_body(&body)
                )),
            });
        }
        uploaded_objects.push(object);
    }

    Ok(MigrationServerUpload {
        uploaded_objects,
        already_present_objects: plan.already_present_objects,
    })
}

fn validated_migration_upload_action_url(
    target_url: &Url,
    action_url: &str,
    allow_insecure_http: bool,
    object: &LfsObject,
) -> CliResult<Url> {
    let parsed = Url::parse(action_url).map_err(|_| CliError::ExternalCommandOutput {
        command: "migration target upload batch".to_owned(),
        message: SanitizedMessage::new("server returned an invalid upload action URL"),
    })?;
    let same_origin = parsed.scheme() == target_url.scheme()
        && parsed.host() == target_url.host()
        && parsed.port_or_known_default() == target_url.port_or_known_default();
    let target_path = target_url.path().trim_end_matches('/');
    let expected_path = format!("{target_path}/objects/{}", object.oid);
    let query = parsed.query_pairs().collect::<Vec<_>>();
    let expected_size = object.size.bytes().to_string();
    if !same_origin
        || parsed.path() != expected_path
        || query.len() != 1
        || query[0].0 != "size"
        || query[0].1 != expected_size
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || (!allow_insecure_http && !crate::http_transport::uses_protected_http_transport(&parsed))
    {
        return Err(CliError::ExternalCommandOutput {
            command: "migration target upload batch".to_owned(),
            message: SanitizedMessage::new(
                "server returned an unsafe or out-of-scope upload action URL",
            ),
        });
    }
    Ok(parsed)
}

fn migration_action_headers(
    action: &LfsBatchAction,
    token: &LfsSessionToken,
) -> CliResult<ReqwestHeaderMap> {
    let mut headers = ReqwestHeaderMap::new();
    for (name, value) in &action.header {
        let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| {
            CliError::ExternalCommandOutput {
                command: "migration target upload batch".to_owned(),
                message: SanitizedMessage::new("server returned an invalid upload action header"),
            }
        })?;
        if name != reqwest::header::AUTHORIZATION {
            return Err(CliError::ExternalCommandOutput {
                command: "migration target upload batch".to_owned(),
                message: SanitizedMessage::new(
                    "server returned an unsupported upload action header",
                ),
            });
        }
        let value = HeaderValue::from_str(value).map_err(|_| CliError::ExternalCommandOutput {
            command: "migration target upload batch".to_owned(),
            message: SanitizedMessage::new("server returned an invalid upload action header"),
        })?;
        let expected = HeaderValue::from_str(&format!("Bearer {}", token.as_str()))
            .expect("validated session tokens always form a valid authorization header");
        if value != expected {
            return Err(CliError::ExternalCommandOutput {
                command: "migration target upload batch".to_owned(),
                message: SanitizedMessage::new(
                    "server returned upload authorization that does not match the active session",
                ),
            });
        }
        headers.insert(name, value);
    }
    if !headers.contains_key(reqwest::header::AUTHORIZATION) {
        return Err(CliError::ExternalCommandOutput {
            command: "migration target upload batch".to_owned(),
            message: SanitizedMessage::new("server omitted upload action authorization"),
        });
    }
    Ok(headers)
}

fn sanitized_migration_http_body(body: &str) -> String {
    let normalized = body.replace(['\r', '\n'], " ");
    let normalized = normalized.trim();
    if normalized.is_empty() {
        "<no response body>".to_owned()
    } else {
        normalized.chars().take(1024).collect()
    }
}

fn write_migration_execution_report<W>(
    output: &mut W,
    context: &MigrationExecutionContext,
    result: &MigrationExecutionResult,
) -> io::Result<()>
where
    W: Write,
{
    let already_local = result.source_fetch.before.available_objects().len();
    writeln!(output, "lfscloud migrate complete")?;
    writeln!(output, "  mode: {}", context.scan.mode.label())?;
    writeln!(
        output,
        "  source remote: {} ({})",
        context.source_remote.remote_name,
        context.source_remote.repository_label()
    )?;
    writeln!(
        output,
        "  source: {}",
        source_endpoint_display(&context.discovery)
    )?;
    writeln!(
        output,
        "  target: {}",
        redacted_url_for_display(&context.route.lfs_url)
    )?;
    writeln!(
        output,
        "  repository route: {}",
        context.repository.remote.repository_label()
    )?;
    writeln!(
        output,
        "  refs scanned: {}",
        context.scan.refs_scanned.len()
    )?;
    writeln!(
        output,
        "  objects discovered: {} ({} bytes total)",
        context.scan.objects.len(),
        migration_objects_total_bytes(context.scan.objects.iter())
    )?;
    writeln!(
        output,
        "  source objects: {} already local, {} fetched",
        already_local,
        result.source_fetch.fetched_objects.len()
    )?;
    writeln!(
        output,
        "  target objects: {} uploaded, {} already present",
        result.server_upload.uploaded_objects.len(),
        result.server_upload.already_present_objects.len()
    )?;
    writeln!(output, "  repository configuration:")?;
    for change in &result.config_changes {
        writeln!(
            output,
            "    {}: {}",
            change.target.label(),
            change.path.display()
        )?;
    }
    if result.legacy_source_configured {
        writeln!(
            output,
            "    .lfsconfig: remote.{}.lfsurl (legacy migration source)",
            context.source_remote.remote_name
        )?;
    }
    writeln!(
        output,
        "  next step: commit .lfsconfig so new clones use LFS Cloud"
    )?;
    if context.purge_source_lfs {
        writeln!(output, "  source purge:")?;
        writeln!(output, "    automatic purge: unsupported")?;
        writeln!(
            output,
            "    verified candidates: {}",
            context.scan.objects.len()
        )?;
        writeln!(
            output,
            "    confirm the destination inventory independently before using the source provider's cleanup process"
        )?;
    }
    Ok(())
}

#[derive(Debug)]
struct MigrationPointerScan {
    mode: MigrationScanMode,
    refs_scanned: Vec<String>,
    pointer_file_count: usize,
    objects: Vec<LfsObject>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MigrationScanMode {
    CurrentCheckout,
    SelectedRefs,
    AllFetchedRefs,
}

impl MigrationScanMode {
    fn label(self) -> &'static str {
        match self {
            Self::CurrentCheckout => "current-checkout",
            Self::SelectedRefs => "selected-refs",
            Self::AllFetchedRefs => "all-refs",
        }
    }

    fn scope_label(self) -> &'static str {
        match self {
            Self::CurrentCheckout => "current checkout index only",
            Self::SelectedRefs => "selected refs only",
            Self::AllFetchedRefs => {
                "all local branches, tags, and fetched refs for the source remote"
            }
        }
    }

    fn scope_warning(self) -> Option<&'static str> {
        match self {
            Self::CurrentCheckout => Some(
                "other refs were not scanned and may reference additional LFS objects; use --all-refs for a full provider move",
            ),
            Self::SelectedRefs | Self::AllFetchedRefs => None,
        }
    }
}

#[derive(Debug)]
struct MigrationDryRunReport {
    discovery: GitLfsMigrationDiscovery,
    source_remote: GitRemote,
    target_remote: GitRemote,
    scan: MigrationPointerScan,
    availability: LocalMigrationObjectAvailability,
    route: LfsInitRoute,
    readiness_checks: Vec<MigrationReadinessCheck>,
    would_touch_files: Vec<PathBuf>,
    source_purge: Option<MigrationSourcePurgeReport>,
}

#[derive(Debug)]
struct MigrationReadinessCheck {
    name: &'static str,
    level: StatusLevel,
    message: String,
}

#[derive(Clone, Copy, Debug)]
struct MigrationTargetReadiness<'a> {
    server_url: &'a str,
    lfs_url: &'a str,
}

#[derive(Debug)]
enum MigrationSourcePurgeReport {
    GitHub,
    NotConfigured,
    Unsupported { host: String },
}

fn migration_pointer_scan(
    start_dir: &Path,
    command: &MigrateCommand,
    source_remote: &str,
) -> CliResult<MigrationPointerScan> {
    if command.all_refs {
        let history = enumerate_fetched_ref_lfs_pointers_for_remote(start_dir, source_remote)?;
        return Ok(history_pointer_scan(
            MigrationScanMode::AllFetchedRefs,
            history,
        ));
    }

    if !command.refs.is_empty() {
        let history = enumerate_selected_ref_lfs_pointers(start_dir, command.refs.iter())?;
        return Ok(history_pointer_scan(
            MigrationScanMode::SelectedRefs,
            history,
        ));
    }

    let checkout = enumerate_current_checkout_lfs_pointers(start_dir)?;
    let objects = dedupe_lfs_objects(checkout.pointers.iter().map(|pointer| &pointer.object));

    Ok(MigrationPointerScan {
        mode: MigrationScanMode::CurrentCheckout,
        refs_scanned: vec!["current checkout".to_owned()],
        pointer_file_count: checkout.pointers.len(),
        objects,
    })
}

fn same_repository_identity(left: &GitRemote, right: &GitRemote) -> bool {
    left.host.eq_ignore_ascii_case(&right.host)
        && left.owner.eq_ignore_ascii_case(&right.owner)
        && left.name.eq_ignore_ascii_case(&right.name)
}

fn history_pointer_scan(
    mode: MigrationScanMode,
    history: GitLfsHistoryPointers,
) -> MigrationPointerScan {
    let objects = dedupe_lfs_objects(history.pointers.iter().map(|pointer| &pointer.object));

    MigrationPointerScan {
        mode,
        refs_scanned: history
            .refs
            .into_iter()
            .map(|scanned| scanned.name)
            .collect(),
        pointer_file_count: history.pointers.len(),
        objects,
    }
}

fn dedupe_lfs_objects<'a>(objects: impl IntoIterator<Item = &'a LfsObject>) -> Vec<LfsObject> {
    objects
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .cloned()
        .collect()
}

fn migration_readiness_checks<P, A>(
    target: MigrationTargetReadiness<'_>,
    discovery: &GitLfsMigrationDiscovery,
    probe_server: &mut P,
    lookup_credential: &mut A,
) -> Vec<MigrationReadinessCheck>
where
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
{
    let mut checks = Vec::new();
    checks.push(migration_git_lfs_readiness_check(discovery));
    checks.push(migration_filter_readiness_check(discovery));
    checks.push(migration_source_readiness_check(discovery));
    checks.push(migration_target_readiness_check(
        target.server_url,
        probe_server,
    ));

    checks.push(match lookup_credential(target.lfs_url) {
        Ok(()) => MigrationReadinessCheck {
            name: "lfs-credential",
            level: StatusLevel::Ok,
            message: "local LFS credential found; server acceptance not probed".to_owned(),
        },
        Err(error) => MigrationReadinessCheck {
            name: "lfs-credential",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        },
    });

    checks.push(MigrationReadinessCheck {
        name: "storage",
        level: StatusLevel::Warning,
        message: "storage is server-owned and was not probed; execution checks it through the authenticated LFS route"
            .to_owned(),
    });

    checks
}

fn migration_git_lfs_readiness_check(
    discovery: &GitLfsMigrationDiscovery,
) -> MigrationReadinessCheck {
    if discovery.installation.installed {
        return MigrationReadinessCheck {
            name: "git-lfs",
            level: StatusLevel::Ok,
            message: discovery
                .installation
                .version
                .clone()
                .unwrap_or_else(|| "git lfs is available locally".to_owned()),
        };
    }

    MigrationReadinessCheck {
        name: "git-lfs",
        level: StatusLevel::Warning,
        message: discovery.installation.diagnostic.as_ref().map_or_else(
            || "git lfs is not available locally".to_owned(),
            ToString::to_string,
        ),
    }
}

fn migration_filter_readiness_check(
    discovery: &GitLfsMigrationDiscovery,
) -> MigrationReadinessCheck {
    let filters = &discovery.filters;
    let missing = [
        ("filter.lfs.clean", filters.clean.is_none()),
        ("filter.lfs.smudge", filters.smudge.is_none()),
        ("filter.lfs.process", filters.process.is_none()),
        ("filter.lfs.required", filters.required.is_none()),
    ]
    .into_iter()
    .filter_map(|(name, is_missing)| is_missing.then_some(name))
    .collect::<Vec<_>>();

    if missing.is_empty() {
        MigrationReadinessCheck {
            name: "lfs-filters",
            level: StatusLevel::Ok,
            message: "clean, smudge, process, and required filters are configured locally"
                .to_owned(),
        }
    } else {
        MigrationReadinessCheck {
            name: "lfs-filters",
            level: StatusLevel::Warning,
            message: format!(
                "missing local Git LFS filter settings: {}",
                missing.join(", ")
            ),
        }
    }
}

fn migration_target_readiness_check<P>(
    server_url: &str,
    probe_server: &mut P,
) -> MigrationReadinessCheck
where
    P: FnMut(&str) -> CliResult<()>,
{
    let display = redacted_url_for_display(server_url);
    match probe_server(server_url) {
        Ok(()) => MigrationReadinessCheck {
            name: "server-tcp",
            level: StatusLevel::Ok,
            message: format!(
                "{display} TCP endpoint is reachable; server authentication and repository access not probed"
            ),
        },
        Err(error) => MigrationReadinessCheck {
            name: "server-tcp",
            level: StatusLevel::Warning,
            message: format!(
                "{display} TCP endpoint is unreachable: {error}; server authentication and repository access not probed"
            ),
        },
    }
}

fn migration_source_readiness_check(
    discovery: &GitLfsMigrationDiscovery,
) -> MigrationReadinessCheck {
    match &discovery.source_endpoint {
        Some(endpoint) => MigrationReadinessCheck {
            name: "source-config",
            level: StatusLevel::Ok,
            message: format!(
                "{} ({}); source repository access not probed",
                redacted_url_for_display(&endpoint.url),
                source_endpoint_source_label(endpoint.source)
            ),
        },
        None => MigrationReadinessCheck {
            name: "source-config",
            level: StatusLevel::Warning,
            message:
                "source Git LFS endpoint is not configured; source repository access not probed"
                    .to_owned(),
        },
    }
}

fn migration_dry_run_touched_files(repository: &GitRepository) -> CliResult<Vec<PathBuf>> {
    Ok(vec![
        repository.worktree_root.join(".lfsconfig"),
        repository.local_git_config_path()?,
    ])
}

fn migration_source_purge_report(
    discovery: &GitLfsMigrationDiscovery,
    requested: bool,
) -> Option<MigrationSourcePurgeReport> {
    if !requested {
        return None;
    }

    match discovery
        .source_endpoint
        .as_ref()
        .map(|endpoint| source_endpoint_provider_label(&endpoint.url))
    {
        Some(label) if label.eq_ignore_ascii_case("github.com") => {
            Some(MigrationSourcePurgeReport::GitHub)
        }
        Some(label) => Some(MigrationSourcePurgeReport::Unsupported { host: label }),
        None => Some(MigrationSourcePurgeReport::NotConfigured),
    }
}

fn source_endpoint_provider_label(url: &str) -> String {
    Url::parse(url)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_owned))
        .unwrap_or_else(|| SOURCE_PROVIDER_UNKNOWN_LABEL.to_owned())
}

fn source_endpoint_display(discovery: &GitLfsMigrationDiscovery) -> String {
    discovery
        .source_endpoint
        .as_ref()
        .map(|endpoint| redacted_url_for_display(&endpoint.url))
        .unwrap_or_else(|| SOURCE_ENDPOINT_UNSET_LABEL.to_owned())
}

fn write_migration_dry_run_report<W>(
    output: &mut W,
    report: &MigrationDryRunReport,
) -> io::Result<()>
where
    W: Write,
{
    let available_objects = report.availability.available_objects();
    let unavailable_objects = report.availability.unavailable_objects();
    let available_count = available_objects.len();
    let fetch_count = unavailable_objects.len();
    let total_bytes = migration_objects_total_bytes(report.scan.objects.iter());
    let available_bytes =
        migration_objects_total_bytes(available_objects.iter().map(|local| &local.object));
    let fetch_bytes =
        migration_objects_total_bytes(unavailable_objects.iter().map(|local| &local.object));

    writeln!(output, "lfscloud migrate dry-run")?;
    writeln!(
        output,
        "  worktree: {}",
        report.discovery.worktree_root.display()
    )?;
    writeln!(output, "  mode: {}", report.scan.mode.label())?;
    writeln!(output, "  scope: {}", report.scan.mode.scope_label())?;
    if let Some(warning) = report.scan.mode.scope_warning() {
        writeln!(output, "  warning: {warning}")?;
    }
    writeln!(
        output,
        "  source remote: {} ({})",
        report.source_remote.remote_name,
        report.source_remote.repository_label()
    )?;
    writeln!(
        output,
        "  target remote: {} ({})",
        report.target_remote.remote_name,
        report.target_remote.repository_label()
    )?;
    writeln!(
        output,
        "  source: {}",
        source_endpoint_display(&report.discovery)
    )?;
    writeln!(
        output,
        "  target: {}",
        redacted_url_for_display(&report.route.lfs_url)
    )?;
    writeln!(
        output,
        "  tracked LFS patterns: {}",
        report.discovery.tracked_patterns.len()
    )?;
    for tracked in &report.discovery.tracked_patterns {
        writeln!(
            output,
            "    {} ({}; {})",
            tracked.pattern,
            tracked.source.display(),
            tracked.attributes.join(" ")
        )?;
    }
    writeln!(output, "  refs scanned: {}", report.scan.refs_scanned.len())?;
    for ref_name in &report.scan.refs_scanned {
        writeln!(output, "    {ref_name}")?;
    }
    writeln!(
        output,
        "  files touched: {} would update",
        report.would_touch_files.len()
    )?;
    for path in &report.would_touch_files {
        writeln!(output, "    {}", path.display())?;
    }
    writeln!(
        output,
        "  pointer files: {}",
        report.scan.pointer_file_count
    )?;
    writeln!(
        output,
        "  objects discovered: {} ({} bytes total)",
        report.scan.objects.len(),
        total_bytes
    )?;
    for object in report
        .scan
        .objects
        .iter()
        .take(MIGRATION_OBJECT_REPORT_LIMIT)
    {
        writeln!(
            output,
            "    sha256:{} ({} bytes)",
            object.oid,
            object.size.bytes()
        )?;
    }
    if report.scan.objects.len() > MIGRATION_OBJECT_REPORT_LIMIT {
        writeln!(
            output,
            "    ... {} more objects omitted",
            report.scan.objects.len() - MIGRATION_OBJECT_REPORT_LIMIT
        )?;
    }
    writeln!(
        output,
        "  objects fetched: {fetch_count} would fetch, {available_count} already local"
    )?;
    writeln!(
        output,
        "    {fetch_bytes} bytes would fetch, {available_bytes} bytes already local"
    )?;
    writeln!(
        output,
        "  source objects: {available_count} local, {fetch_count} missing locally ({available_bytes} local bytes, {fetch_bytes} missing bytes)"
    )?;
    writeln!(
        output,
        "  target objects: 0 confirmed new, 0 confirmed existing, {} unknown ({} bytes unknown)",
        report.scan.objects.len(),
        total_bytes
    )?;
    writeln!(
        output,
        "    target storage not probed during dry-run; execution reconciles through LFS Cloud before fetching source bytes"
    )?;
    writeln!(
        output,
        "  local readiness checks (no remote access probes):"
    )?;
    for check in &report.readiness_checks {
        writeln!(
            output,
            "    {:<10} {:<7} {}",
            check.name,
            check.level.label(),
            check.message
        )?;
    }
    write_migration_dry_run_warnings(output, report, fetch_count, fetch_bytes)?;
    if let Some(source_purge) = &report.source_purge {
        write_migration_source_purge_report(output, source_purge, report)?;
    }

    Ok(())
}

fn migration_objects_total_bytes<'a>(objects: impl IntoIterator<Item = &'a LfsObject>) -> u128 {
    objects
        .into_iter()
        .map(|object| u128::from(object.size.bytes()))
        .sum()
}

fn write_migration_dry_run_warnings<W>(
    output: &mut W,
    report: &MigrationDryRunReport,
    fetch_count: usize,
    fetch_bytes: u128,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(output, "  warnings:")?;
    if report.discovery.tracked_patterns.is_empty() {
        writeln!(
            output,
            "    warning: no tracked LFS patterns were discovered"
        )?;
    }
    if fetch_count > 0 {
        let noun = if fetch_count == 1 {
            "object"
        } else {
            "objects"
        };
        let verb = if fetch_count == 1 { "has" } else { "have" };
        writeln!(
            output,
            "    warning: {fetch_count} {noun} ({fetch_bytes} bytes) {verb} no verified local source; source fetch and remote availability must succeed during execution"
        )?;
    }
    writeln!(
        output,
        "    warning: source and target repository permissions were not probed"
    )?;
    writeln!(
        output,
        "    warning: target storage quota and free capacity were not probed; the server owns those checks"
    )?;
    if let Some(MigrationSourcePurgeReport::NotConfigured) = &report.source_purge {
        writeln!(
            output,
            "    warning: source purge availability is unknown without a configured source endpoint"
        )?;
    } else if let Some(MigrationSourcePurgeReport::Unsupported { host }) = &report.source_purge {
        writeln!(
            output,
            "    warning: automatic source purge is unsupported for {host}"
        )?;
    }

    Ok(())
}

fn write_migration_source_purge_report<W>(
    output: &mut W,
    source_purge: &MigrationSourcePurgeReport,
    report: &MigrationDryRunReport,
) -> io::Result<()>
where
    W: Write,
{
    let total_bytes = migration_objects_total_bytes(report.scan.objects.iter());

    writeln!(output, "  source purge:")?;
    writeln!(
        output,
        "    source: {}",
        source_endpoint_display(&report.discovery)
    )?;
    match source_purge {
        MigrationSourcePurgeReport::GitHub => {
            writeln!(output, "    provider: GitHub")?;
            writeln!(output, "    automatic purge: unsupported")?;
            writeln!(
                output,
                "    planned candidates: {} ({} bytes; upload not verified)",
                report.scan.objects.len(),
                total_bytes
            )?;
            writeln!(output, "    GitHub LFS purge requires GitHub Support.")?;
            writeln!(
                output,
                "    support URL: https://support.github.com/contact-next/product-selection/repositories"
            )?;
            writeln!(
                output,
                "    suggested subject: Purge Git LFS objects after migration"
            )?;
            writeln!(
                output,
                "    instructions: use GitHub's repository support flow or Virtual Agent only after migration execution verifies every object at the destination."
            )?;
            writeln!(
                output,
                "    purge manifest: unavailable during dry-run planning"
            )?;
            writeln!(
                output,
                "    requirement: generate purge input only after successful execution and independent destination verification; planned objects are not proof of upload."
            )?;
        }
        MigrationSourcePurgeReport::NotConfigured => {
            writeln!(output, "    provider: {SOURCE_PROVIDER_UNKNOWN_LABEL}")?;
            writeln!(
                output,
                "    automatic purge: unavailable because no source Git LFS endpoint was detected."
            )?;
        }
        MigrationSourcePurgeReport::Unsupported { host } => {
            writeln!(output, "    provider: {host}")?;
            writeln!(
                output,
                "    automatic purge: unsupported by this helper; no source-provider cleanup will be attempted."
            )?;
        }
    }

    Ok(())
}

fn source_endpoint_source_label(source: GitLfsSourceEndpointSource) -> &'static str {
    match source {
        GitLfsSourceEndpointSource::LocalGitConfig => "local Git config",
        GitLfsSourceEndpointSource::RemoteGitConfig => "remote Git config",
        GitLfsSourceEndpointSource::WorktreeRemoteConfig => "remote-scoped committed .lfsconfig",
        GitLfsSourceEndpointSource::WorktreeLfsConfig => ".lfsconfig",
        GitLfsSourceEndpointSource::RemoteUrlDefault => "remote URL default",
    }
}

async fn probe_authenticated_migration_target(
    lfs_url: &str,
    allow_insecure_http: bool,
    token: &LfsSessionToken,
) -> CliResult<()> {
    let mut batch_url = crate::init::validate_server_url(lfs_url, allow_insecure_http)?;
    append_url_path_segments(&mut batch_url, "objects/batch")?;

    let client = redirect_free_http_client("failed to create migration target probe client")?;
    let response = client
        .post(batch_url)
        .bearer_auth(token.as_str())
        .header("Accept", "application/vnd.git-lfs+json")
        .header("Content-Type", "application/vnd.git-lfs+json")
        .json(&serde_json::json!({
            "operation": "upload",
            "transfers": ["basic"],
            "objects": [],
        }))
        .timeout(MIGRATION_TARGET_PROBE_TIMEOUT)
        .send()
        .await
        .map_err(|source| CliError::Io {
            context: "failed to authenticate the migration target repository".to_owned(),
            source: io::Error::other(source),
        })?;

    if response.status().is_success() {
        Ok(())
    } else {
        Err(CliError::ExternalCommandOutput {
            command: "migration target repository authentication".to_owned(),
            message: SanitizedMessage::new(format!(
                "server returned HTTP status {}",
                response.status().as_u16()
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    #[allow(unused_imports)]
    use std::ffi::OsString;
    #[cfg(unix)]
    #[allow(unused_imports)]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(unix)]
    #[allow(unused_imports)]
    use std::time::Instant;
    #[allow(unused_imports)]
    use std::{
        collections::BTreeMap,
        fs, io,
        path::{Path, PathBuf},
        process::Command as ProcessCommand,
        sync::{Arc, Mutex},
        time::Duration,
    };

    #[allow(unused_imports)]
    use axum::{
        Json, Router,
        body::Bytes,
        http::{HeaderMap, StatusCode},
        routing::{post, put},
    };
    #[allow(unused_imports)]
    use clap::{CommandFactory, Parser};
    #[allow(unused_imports)]
    use sha2::{Digest, Sha256};
    #[allow(unused_imports)]
    use tempfile::TempDir;

    #[allow(unused_imports)]
    use super::*;
    #[allow(unused_imports)]
    use crate::cli::test_support::*;
    #[allow(unused_imports)]
    use crate::{
        CliError, DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, GitCredentialApproval,
        GitCredentialRejection, GitLfsConfigChange, GitLfsConfigTarget, GitRepository, LfsObject,
        LfsPointer, LfsSessionToken, LocalCacheLayout, SanitizedMessage,
    };

    #[tokio::test]
    async fn migration_target_probe_authenticates_the_repository_batch_route() {
        let observed = Arc::new(Mutex::new(None));
        let observed_for_route = Arc::clone(&observed);
        let app = Router::new().route(
            "/github.com/owner/repo.git/info/lfs/objects/batch",
            post(
                move |headers: HeaderMap, Json(body): Json<serde_json::Value>| {
                    let observed = Arc::clone(&observed_for_route);
                    async move {
                        *observed
                            .lock()
                            .expect("migration target probe record should not poison") = Some((
                            headers
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned),
                            body,
                        ));
                        StatusCode::OK
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("migration target probe listener should bind");
        let address = listener
            .local_addr()
            .expect("migration target probe address should be available");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("migration target probe server should run");
        });
        let token = LfsSessionToken::from_secret("migration-session-token")
            .expect("migration session token should be valid");

        probe_authenticated_migration_target(
            &format!("http://{address}/github.com/owner/repo.git/info/lfs"),
            false,
            &token,
        )
        .await
        .expect("repository-scoped authenticated probe should succeed");
        server.abort();

        let observed = observed
            .lock()
            .expect("migration target probe record should not poison")
            .clone()
            .expect("migration target probe request should be recorded");
        assert_eq!(
            observed.0.as_deref(),
            Some("Bearer migration-session-token")
        );
        assert_eq!(observed.1["operation"], "upload");
        assert_eq!(observed.1["objects"], serde_json::json!([]));
    }

    #[test]
    fn migrate_rejects_the_obsolete_server_config_argument() {
        let mut output = Vec::new();
        let error = run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: None,
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: false,
            },
            Some(PathBuf::from("legacy-lfscloud.yml")),
            ".",
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect_err("migrate must not silently ignore --config");

        assert!(matches!(error, CliError::InvalidArguments { message }
            if message.contains("migrate no longer reads --config")));
        assert!(output.is_empty());
    }

    #[test]
    fn migration_upload_action_must_match_object_and_session() {
        let object = object_for_bytes(b"validated action identity");
        let target = Url::parse("https://cloud.example/github.com/owner/repo.git/info/lfs")
            .expect("target URL should parse");
        let token = LfsSessionToken::from_secret("migration-session-token")
            .expect("session token should parse");
        let valid = LfsBatchAction {
            href: format!(
                "{}/objects/{}?size={}",
                target.as_str().trim_end_matches('/'),
                object.oid,
                object.size.bytes()
            ),
            header: BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer migration-session-token".to_owned(),
            )]),
            expires_at: None,
            expires_in: None,
        };

        validated_migration_upload_action_url(&target, &valid.href, false, &object)
            .expect("matching action identity should be accepted");
        migration_action_headers(&valid, &token)
            .expect("matching action authorization should be accepted");

        let wrong_oid = LfsBatchAction {
            href: valid.href.replace(object.oid.as_hex(), &"f".repeat(64)),
            ..valid.clone()
        };
        assert!(
            validated_migration_upload_action_url(&target, &wrong_oid.href, false, &object)
                .is_err()
        );

        let wrong_token = LfsBatchAction {
            header: BTreeMap::from([(
                "Authorization".to_owned(),
                "Bearer another-session".to_owned(),
            )]),
            ..valid
        };
        assert!(migration_action_headers(&wrong_token, &token).is_err());
    }

    #[tokio::test]
    async fn migration_reconciles_and_uploads_only_server_missing_objects() {
        require_git();
        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let existing = object_for_bytes(b"already on server");
        let missing_bytes = b"upload through LFS Cloud";
        let missing = object_for_bytes(missing_bytes);
        write_git_lfs_source_object(&repo, &missing, missing_bytes);

        let uploaded = Arc::new(Mutex::new(None));
        let uploaded_for_route = Arc::clone(&uploaded);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("migration server listener should bind");
        let address = listener
            .local_addr()
            .expect("migration server address should be available");
        let upload_href = format!(
            "http://{address}/github.com/owner/repo.git/info/lfs/objects/{}?size={}",
            missing.oid,
            missing.size.bytes()
        );
        let existing_for_batch = existing.clone();
        let missing_for_batch = missing.clone();
        let app = Router::new()
            .route(
                "/github.com/owner/repo.git/info/lfs/objects/batch",
                post(move || {
                    let upload_href = upload_href.clone();
                    let existing = existing_for_batch.clone();
                    let missing = missing_for_batch.clone();
                    async move {
                        Json(serde_json::json!({
                            "transfer": "basic",
                            "objects": [
                                {"oid": existing.oid, "size": existing.size},
                                {
                                    "oid": missing.oid,
                                    "size": missing.size,
                                    "authenticated": true,
                                    "actions": {
                                        "upload": {
                                            "href": upload_href,
                                            "header": {
                                                "Authorization": "Bearer migration-session-token"
                                            }
                                        }
                                    }
                                }
                            ]
                        }))
                    }
                }),
            )
            .route(
                &format!(
                    "/github.com/owner/repo.git/info/lfs/objects/{}",
                    missing.oid
                ),
                put(move |headers: HeaderMap, body: Bytes| {
                    let uploaded = Arc::clone(&uploaded_for_route);
                    async move {
                        *uploaded
                            .lock()
                            .expect("migration upload record should not poison") = Some((
                            headers
                                .get("authorization")
                                .and_then(|value| value.to_str().ok())
                                .map(str::to_owned),
                            body.to_vec(),
                        ));
                        StatusCode::OK
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("migration server should run");
        });
        let lfs_url = format!("http://{address}/github.com/owner/repo.git/info/lfs");
        let token = LfsSessionToken::from_secret("migration-session-token")
            .expect("migration token should parse");

        let plan = request_migration_target_plan(
            &lfs_url,
            false,
            &token,
            &[existing.clone(), missing.clone()],
        )
        .await
        .expect("migration target should reconcile objects");
        assert_eq!(plan.already_present_objects, vec![existing.clone()]);
        assert_eq!(plan.uploads.len(), 1);
        let availability = check_local_migration_objects(&repo, [&missing], None)
            .expect("missing target object should be locally available");
        let result =
            upload_migration_objects_through_server(&lfs_url, false, &token, &availability, plan)
                .await
                .expect("migration should upload through the LFS server action");
        server.abort();

        assert_eq!(result.uploaded_objects, vec![missing]);
        assert_eq!(result.already_present_objects, vec![existing]);
        assert_eq!(
            uploaded
                .lock()
                .expect("migration upload record should not poison")
                .clone(),
            Some((
                Some("Bearer migration-session-token".to_owned()),
                missing_bytes.to_vec()
            ))
        );
    }

    #[tokio::test]
    async fn failed_server_upload_leaves_both_target_config_locations_unchanged() {
        require_git();
        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let first_bytes = b"first migration object";
        let second_bytes = b"second migration object";
        let first = object_for_bytes(first_bytes);
        let second = object_for_bytes(second_bytes);
        write_git_lfs_source_object(&repo, &first, first_bytes);
        write_git_lfs_source_object(&repo, &second, second_bytes);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("migration server listener should bind");
        let address = listener
            .local_addr()
            .expect("migration server address should resolve");
        let target_route = "/github.com/owner/repo.git/info/lfs";
        let server_base = format!("http://{address}");
        let server_base_for_batch = server_base.clone();
        let upload_count = Arc::new(Mutex::new(0_usize));
        let upload_count_for_route = Arc::clone(&upload_count);
        let app = Router::new()
            .route(
                &format!("{target_route}/objects/batch"),
                post(move |Json(body): Json<serde_json::Value>| {
                    let server_base = server_base_for_batch.clone();
                    async move {
                        let objects = body["objects"]
                            .as_array()
                            .expect("batch objects should be an array")
                            .iter()
                            .map(|object| {
                                let oid = object["oid"].as_str().expect("OID should be present");
                                let size = object["size"].as_u64().expect("size should be present");
                                serde_json::json!({
                                    "oid": oid,
                                    "size": size,
                                    "actions": {
                                        "upload": {
                                            "href": format!("{server_base}{target_route}/objects/{oid}?size={size}"),
                                            "header": {
                                                "Authorization": "Bearer migration-session-token"
                                            }
                                        }
                                    }
                                })
                            })
                            .collect::<Vec<_>>();
                        Json(serde_json::json!({ "transfer": "basic", "objects": objects }))
                    }
                }),
            )
            .route(
                &format!("{target_route}/objects/{{oid}}"),
                put(move || {
                    let upload_count = Arc::clone(&upload_count_for_route);
                    async move {
                        let mut count = upload_count
                            .lock()
                            .expect("migration upload count should not poison");
                        *count += 1;
                        if *count == 1 {
                            StatusCode::OK
                        } else {
                            StatusCode::INTERNAL_SERVER_ERROR
                        }
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("migration server should run");
        });

        let repository = GitRepository::discover(&repo).expect("repository should be discovered");
        let route = LfsInitRoute::resolve(&server_base, &repository.remote)
            .expect("migration route should resolve");
        let discovery = crate::discover_git_lfs_migration_from_remote(&repo, "origin")
            .expect("migration inputs should be discovered");
        let context = MigrationExecutionContext {
            source_remote: repository.remote.clone(),
            repository,
            route,
            discovery,
            scan: MigrationPointerScan {
                mode: MigrationScanMode::AllFetchedRefs,
                refs_scanned: vec!["refs/heads/main".to_owned()],
                pointer_file_count: 2,
                objects: vec![first, second],
            },
            cache_layout: local_cache_layout(Some(temp.path().join("cache")))
                .expect("cache layout should resolve"),
            purge_source_lfs: false,
            allow_insecure_http: false,
        };
        let token = LfsSessionToken::from_secret("migration-session-token")
            .expect("migration token should parse");

        let error = execute_migration_through_server(&context, &token)
            .await
            .expect_err("second upload should fail migration");
        server.abort();

        assert!(matches!(error, CliError::MigrationUploadFailed { .. }));
        assert_eq!(
            *upload_count
                .lock()
                .expect("migration upload count should not poison"),
            2
        );
        assert!(!repo.join(".lfsconfig").exists());
        let local_lfs_url = ProcessCommand::new("git")
            .args(["config", "--local", "--get", "lfs.url"])
            .current_dir(&repo)
            .output()
            .expect("local Git config should be readable");
        assert!(!local_lfs_url.status.success());
    }

    #[tokio::test]
    async fn migration_target_probe_rejects_an_inactive_session() {
        let app = Router::new().route(
            "/github.com/owner/repo.git/info/lfs/objects/batch",
            post(|| async { StatusCode::UNAUTHORIZED }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("migration target probe listener should bind");
        let address = listener
            .local_addr()
            .expect("migration target probe address should be available");
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .await
                .expect("migration target probe server should run");
        });
        let token = LfsSessionToken::from_secret("expired-migration-session")
            .expect("migration session token should be valid");

        let error = probe_authenticated_migration_target(
            &format!("http://{address}/github.com/owner/repo.git/info/lfs"),
            false,
            &token,
        )
        .await
        .expect_err("inactive migration session should fail before migration work");
        server.abort();

        assert!(
            matches!(error, CliError::ExternalCommandOutput { command, message }
            if command == "migration target repository authentication"
                && message.as_str().contains("401"))
        );
    }

    #[tokio::test]
    async fn migration_target_probe_rejects_non_loopback_http_without_opt_in() {
        let token = LfsSessionToken::from_secret("migration-session-token")
            .expect("migration session token should be valid");

        let error = probe_authenticated_migration_target(
            "http://example.com/github.com/owner/repo.git/info/lfs",
            false,
            &token,
        )
        .await
        .expect_err("non-loopback HTTP should require explicit opt-in");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
    }

    #[test]
    fn migrate_dry_run_reports_current_checkout_plan_without_writes() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        run_git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:Owner/Repo.git",
            ],
        );
        run_git(
            &repo,
            &["config", "filter.lfs.clean", "git-lfs clean -- %f"],
        );
        run_git(
            &repo,
            &["config", "filter.lfs.smudge", "git-lfs smudge -- %f"],
        );
        run_git(
            &repo,
            &["config", "filter.lfs.process", "git-lfs filter-process"],
        );
        run_git(&repo, &["config", "filter.lfs.required", "true"]);
        let local_git_config_path = GitRepository::discover(&repo)
            .expect("temporary repository should be discovered")
            .local_git_config_path()
            .expect("local Git config path should resolve");
        let object = object_for_bytes(b"migration object already local");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        write_git_lfs_source_object(&repo, &object, b"migration object already local");
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: false,
            },
            None,
            &repo,
            &mut output,
            |_| Ok(()),
            |lfs_url| {
                assert_eq!(
                    lfs_url,
                    "http://127.0.0.1:8080/github.com/Owner/Repo.git/info/lfs"
                );
                Ok(())
            },
        )
        .expect("dry-run migration plan should be reported");

        assert!(
            !repo.join(".lfsconfig").exists(),
            "dry-run must not write Git LFS config"
        );
        assert!(
            !cache_root.exists(),
            "dry-run must not create local cache state"
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfscloud migrate dry-run"));
        assert!(rendered.contains("mode: current-checkout"));
        assert!(rendered.contains("scope: current checkout index only"));
        assert!(rendered.contains(
            "warning: other refs were not scanned and may reference additional LFS objects"
        ));
        assert!(rendered.contains("use --all-refs for a full provider move"));
        assert!(rendered.contains("refs scanned: 1"));
        assert!(rendered.contains("current checkout"));
        assert!(rendered.contains("files touched: 2 would update"));
        assert!(rendered.contains(".lfsconfig"));
        assert!(rendered.contains(&local_git_config_path.display().to_string()));
        assert!(rendered.contains("tracked LFS patterns: 1"));
        assert!(rendered.contains("*.bin (.gitattributes; filter=lfs)"));
        assert!(rendered.contains("pointer files: 1"));
        assert!(rendered.contains(&format!(
            "objects discovered: 1 ({} bytes total)",
            object.size.bytes()
        )));
        assert!(rendered.contains("objects fetched: 0 would fetch, 1 already local"));
        assert!(rendered.contains(&format!(
            "0 bytes would fetch, {} bytes already local",
            object.size.bytes()
        )));
        assert!(rendered.contains("source objects: 1 local, 0 missing locally"));
        assert!(
            rendered.contains("target objects: 0 confirmed new, 0 confirmed existing, 1 unknown")
        );
        assert!(rendered.contains("target storage not probed during dry-run"));
        assert!(!rendered.contains("objects uploaded:"));
        assert!(rendered.contains("local readiness checks (no remote access probes):"));
        assert!(rendered.contains("git-lfs"));
        assert!(rendered.contains("lfs-filters ok"));
        assert!(rendered.contains("source-config"));
        assert!(rendered.contains("server-tcp"));
        assert!(rendered.contains("lfs-credential"));
        assert!(rendered.contains("storage"));
        assert!(rendered.contains("source repository access not probed"));
        assert!(rendered.contains("server authentication and repository access not probed"));
        assert!(rendered.contains("storage is server-owned and was not probed"));
        assert!(rendered.contains("warnings:"));
        assert!(rendered.contains("repository permissions were not probed"));
        assert!(rendered.contains("storage quota and free capacity were not probed"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn migrate_requires_acknowledgement_for_cross_remote_identity() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                "git@github.com:target/repo.git",
            ],
        );
        run_git(
            &repo,
            &[
                "remote",
                "add",
                "upstream",
                "git@github.com:source/repo.git",
            ],
        );
        let command = |allow_cross_remote| MigrateCommand {
            server: "http://127.0.0.1:8080".to_owned(),
            allow_insecure_http: false,
            cache_root: Some(temp.path().join("cache")),
            source_remote: "upstream".to_owned(),
            allow_cross_remote,
            refs: Vec::new(),
            all_refs: false,
            dry_run: true,
            purge_source_lfs: false,
        };
        let mut denied_output = Vec::new();

        let error = run_migrate_from_dir(
            command(false),
            None,
            &repo,
            &mut denied_output,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect_err("cross-repository migration should require explicit acknowledgement");

        assert!(matches!(error, CliError::InvalidArguments { message }
            if message.contains("github.com/source/repo")
                && message.contains("github.com/target/repo")
                && message.contains("--allow-cross-remote")));
        assert!(denied_output.is_empty());

        let mut allowed_output = Vec::new();
        run_migrate_from_dir(
            command(true),
            None,
            &repo,
            &mut allowed_output,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("acknowledged cross-repository dry run should report the plan");

        let rendered = String::from_utf8(allowed_output).expect("output should be UTF-8");
        assert!(rendered.contains("source remote: upstream (github.com/source/repo)"));
        assert!(rendered.contains("target remote: origin (github.com/target/repo)"));
        assert!(rendered.contains("source: https://github.com/source/repo.git/info/lfs"));
        assert!(
            rendered.contains("target: http://127.0.0.1:8080/github.com/target/repo.git/info/lfs")
        );
    }

    #[test]
    fn migrate_dry_run_reports_missing_objects_as_would_fetch_without_fetching() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"migration object missing locally");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: false,
            },
            None,
            &repo,
            &mut output,
            |_| {
                Err(CliError::InvalidArguments {
                    message: "probe failed".to_owned(),
                })
            },
            |_| Ok(()),
        )
        .expect("dry-run migration plan should be reported");

        assert!(
            !cache_root.exists(),
            "dry-run must not create cache state while planning fetches"
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("objects fetched: 1 would fetch, 0 already local"));
        assert!(rendered.contains(&format!(
            "{} bytes would fetch, 0 bytes already local",
            object.size.bytes()
        )));
        assert!(rendered.contains("source objects: 0 local, 1 missing locally"));
        assert!(
            rendered.contains("target objects: 0 confirmed new, 0 confirmed existing, 1 unknown")
        );
        assert!(rendered.contains("target storage not probed during dry-run"));
        assert!(!rendered.contains("objects uploaded:"));
        assert!(rendered.contains("server-tcp warning"));
        assert!(rendered.contains(&format!(
            "1 object ({} bytes) has no verified local source",
            object.size.bytes()
        )));
    }

    #[test]
    fn migrate_dry_run_withholds_unverified_github_purge_manifest() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"migration object for GitHub purge report");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: true,
            },
            None,
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration purge helper should be reported");

        assert!(
            !cache_root.exists(),
            "purge helper dry-run must not create local cache state"
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("source purge:"));
        assert!(rendered.contains("    source: https://github.com/owner/repo.git/info/lfs"));
        assert!(rendered.contains("provider: GitHub"));
        assert!(rendered.contains("automatic purge: unsupported"));
        assert!(rendered.contains("GitHub LFS purge requires GitHub Support."));
        assert!(
            rendered
                .contains("https://support.github.com/contact-next/product-selection/repositories")
        );
        assert!(rendered.contains("suggested subject: Purge Git LFS objects after migration"));
        assert!(rendered.contains("planned candidates: 1"));
        assert!(rendered.contains("upload not verified"));
        assert!(rendered.contains("purge manifest: unavailable during dry-run planning"));
        assert!(rendered.contains("successful execution and independent destination verification"));
        assert!(
            !rendered
                .lines()
                .any(|line| line.starts_with("      sha256:"))
        );
        assert!(rendered.contains(object.oid.as_hex()));
        assert!(rendered.contains(&format!("{} bytes", object.size.bytes())));
    }

    #[test]
    fn migrate_dry_run_reports_custom_source_as_unsupported_purge_provider() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        run_git(
            &repo,
            &[
                "config",
                "--local",
                "lfs.url",
                "https://lfs.example.com/owner/repo.git/info/lfs",
            ],
        );
        let object = object_for_bytes(b"migration object from custom source");
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: true,
            },
            None,
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration purge helper should be reported");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("source: https://lfs.example.com/owner/repo.git/info/lfs"));
        assert!(rendered.contains("source purge:"));
        assert!(rendered.contains("    source: https://lfs.example.com/owner/repo.git/info/lfs"));
        assert!(rendered.contains("provider: lfs.example.com"));
        assert!(!rendered.contains("provider: GitHub"));
        assert!(!rendered.contains("GitHub LFS purge requires GitHub Support."));
    }

    #[test]
    fn migrate_dry_run_caps_object_listing_but_keeps_counts() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        for index in 0..=super::MIGRATION_OBJECT_REPORT_LIMIT {
            let bytes = format!("migration object {index}");
            let object = object_for_bytes(bytes.as_bytes());
            write_file(
                &repo.join(format!("asset/model-{index}.bin")),
                LfsPointer::new(object).to_pointer_file().as_bytes(),
            );
        }
        run_git(&repo, &["add", "."]);
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: false,
            },
            None,
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration plan should be reported");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("objects discovered: 101"));
        assert!(rendered.contains("... 1 more objects omitted"));
        assert!(
            rendered.contains("target objects: 0 confirmed new, 0 confirmed existing, 101 unknown")
        );
        assert!(rendered.contains("target storage not probed during dry-run"));
        assert!(!rendered.contains("objects uploaded:"));
    }

    #[test]
    fn migrate_dry_run_purge_report_does_not_bypass_object_listing_limit() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        let cache_root = temp.path().join("cache");
        init_git_repo_with_origin(&repo);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        for index in 0..=super::MIGRATION_OBJECT_REPORT_LIMIT {
            let bytes = format!("migration object {index}");
            let object = object_for_bytes(bytes.as_bytes());
            write_file(
                &repo.join(format!("asset/model-{index}.bin")),
                LfsPointer::new(object).to_pointer_file().as_bytes(),
            );
        }
        run_git(&repo, &["add", "."]);
        let mut output = Vec::new();

        run_migrate_from_dir(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: true,
                purge_source_lfs: true,
            },
            None,
            &repo,
            &mut output,
            |_| Ok(()),
            |_| Ok(()),
        )
        .expect("dry-run migration purge helper should be reported");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        let main_listing_count = rendered
            .lines()
            .filter(|line| line.starts_with("    sha256:") && !line.starts_with("      sha256:"))
            .count();
        assert!(rendered.contains("... 1 more objects omitted"));
        assert_eq!(main_listing_count, super::MIGRATION_OBJECT_REPORT_LIMIT);
        assert!(rendered.contains("planned candidates: 101"));
        assert!(rendered.contains("purge manifest: unavailable during dry-run planning"));
        assert!(
            !rendered
                .lines()
                .any(|line| line.starts_with("      sha256:"))
        );
    }

    #[test]
    fn migrate_execution_requires_all_refs_before_repository_writes() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");

        let error = prepare_migration_execution(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(cache_root.clone()),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: false,
                dry_run: false,
                purge_source_lfs: false,
            },
            temp.path(),
        )
        .expect_err("execution without all-ref coverage should be rejected");

        assert!(matches!(error, CliError::InvalidArguments { message }
                if message.contains("requires --all-refs") && message.contains("historical")));
        assert!(!cache_root.exists());
    }

    #[test]
    fn source_endpoint_provider_label_uses_host_or_unknown() {
        assert_eq!(
            super::source_endpoint_provider_label(
                "https://lfs.example.com/owner/repo.git/info/lfs"
            ),
            "lfs.example.com"
        );
        assert_eq!(
            super::source_endpoint_provider_label("not a url?token=query-secret"),
            super::SOURCE_PROVIDER_UNKNOWN_LABEL
        );
    }
}
