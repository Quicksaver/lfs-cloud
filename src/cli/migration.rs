//! Git LFS migration planning, readiness checks, execution, and reporting.

use super::*;

const MIGRATION_TARGET_PROBE_TIMEOUT: Duration = Duration::from_secs(30);
const MIGRATION_OBJECT_REPORT_LIMIT: usize = 100;
const SOURCE_ENDPOINT_UNSET_LABEL: &str = "<unset>";
const SOURCE_PROVIDER_UNKNOWN_LABEL: &str = "unknown";
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
            validate_status_storage,
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

fn run_migrate_from_dir<W, P, A, S>(
    command: MigrateCommand,
    config_path: Option<PathBuf>,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut probe_server: P,
    mut lookup_credential: A,
    mut validate_storage: S,
) -> CliResult<()>
where
    W: Write,
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
{
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
    let discovery = discover_git_lfs_migration_from_remote(start_dir, &command.source_remote)?;
    let scan = migration_pointer_scan(start_dir, &command, &command.source_remote)?;
    let cache_layout = Some(local_cache_layout(command.cache_root.clone())?);
    let availability =
        check_local_migration_objects(start_dir, scan.objects.iter(), cache_layout.as_ref())?;
    let config_path = config_path.unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let readiness_checks = migration_readiness_checks(
        &config_path,
        &repository,
        MigrationTargetReadiness {
            server_url: &command.server,
            lfs_url: &route.lfs_url,
        },
        &discovery,
        &mut probe_server,
        &mut lookup_credential,
        &mut validate_storage,
    );
    let source_purge = migration_source_purge_report(&discovery, command.purge_source_lfs);
    let report = MigrationDryRunReport {
        discovery,
        source_remote: source_repository.remote,
        target_remote: repository.remote.clone(),
        scan,
        availability,
        route,
        config_path,
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
}

#[derive(Debug)]
struct MigrationExecutionResult {
    source_fetch: MigrationSourceFetch,
    storage_upload: MigrationStorageUpload,
    config_changes: Vec<GitLfsConfigChange>,
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

    let config_path = config_path.unwrap_or_else(|| ServerConfig::default_path().to_path_buf());
    let (mapping, storage) =
        migration_storage_provider(&config_path, &preparation.repository).await?;
    fetch_migration_git_refs(
        &preparation.repository.worktree_root,
        &preparation.source_remote.remote_name,
    )?;
    let context = preparation.scan_fetched_refs()?;
    let result = execute_migration_with_storage(&context, &mapping, storage.as_ref()).await?;
    write_migration_execution_report(output, &context, &mapping, &result).map_err(output_error)
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
    let discovery = discover_git_lfs_migration_from_remote(start_dir, &command.source_remote)?;
    if !discovery.installation.installed {
        return Err(CliError::InvalidArguments {
            message: "migration execution requires Git LFS; install it and run `git lfs install` before retrying"
                .to_owned(),
        });
    }
    if discovery
        .source_endpoint
        .as_ref()
        .is_some_and(|source| source.url == route.lfs_url)
    {
        return Err(CliError::InvalidArguments {
            message: "source Git LFS endpoint already points at the requested LFS Cloud target"
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

async fn migration_storage_provider(
    config_path: &Path,
    repository: &GitRepository,
) -> CliResult<(RepositoryMapping, Arc<dyn StorageProvider + Send + Sync>)> {
    let config = ServerConfig::load_from_path(config_path)?;
    let mapping = config
        .repository_mapping_for_identity(
            &repository.remote.host,
            &repository.remote.owner,
            &repository.remote.name,
        )
        .cloned()
        .ok_or_else(|| CliError::InvalidArguments {
            message: format!(
                "server config has no repository mapping for {}",
                repository.remote.repository_label()
            ),
        })?;
    let storage = config
        .storage_providers
        .get(&mapping.storage_provider)
        .cloned()
        .ok_or_else(|| CliError::InvalidArguments {
            message: format!(
                "repository mapping {} references unknown storage provider {}",
                mapping.id, mapping.storage_provider
            ),
        })?;
    let metadata = Arc::new(MetadataDatabase::open(&config.server.metadata_path)?);
    metadata.sync_config(&config)?;
    let provider = storage
        .build_provider(mapping.id.clone(), metadata)
        .await
        .map_err(MigrationError::from)?;
    Ok((mapping, provider))
}

async fn execute_migration_with_storage(
    context: &MigrationExecutionContext,
    mapping: &RepositoryMapping,
    storage: &dyn StorageProvider,
) -> CliResult<MigrationExecutionResult> {
    let repository_namespace = storage_namespace_for_context(context, mapping)?;
    if context.scan.objects.is_empty() {
        return Err(CliError::InvalidArguments {
            message: "migration found no non-empty Git LFS objects across the selected history"
                .to_owned(),
        });
    }
    let source_fetch = fetch_missing_migration_objects_from_remote(
        &context.repository.worktree_root,
        context.scan.objects.iter(),
        Some(&context.cache_layout),
        &context.source_remote.remote_name,
        MigrationFetchMode::AllFetchedRefs,
    )?;
    if let Some(object) = source_fetch.unavailable_objects.first() {
        return Err(MigrationError::SourceObjectMissing {
            oid: object.oid.as_hex().to_owned(),
            size: object.size.bytes(),
        }
        .into());
    }

    let storage_upload =
        upload_migration_objects_to_storage(&source_fetch.after, storage, repository_namespace)
            .await?;
    if let Some(first) = storage_upload.failed_objects.first() {
        return Err(CliError::MigrationUploadFailed {
            failures: storage_upload.failed_objects.len(),
            oid: first.object.oid.as_hex().to_owned(),
            message: first.message.clone(),
        });
    }

    // Persist both forms after every object has a synchronized successful
    // checkpoint record. The local override keeps historical commits working
    // even when they predate the newly committed `.lfsconfig` file.
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
        storage_upload,
        config_changes,
    })
}

fn storage_namespace_for_context<'a>(
    context: &MigrationExecutionContext,
    mapping: &'a RepositoryMapping,
) -> CliResult<&'a str> {
    if mapping
        .host
        .eq_ignore_ascii_case(&context.repository.remote.host)
        && mapping
            .owner
            .eq_ignore_ascii_case(&context.repository.remote.owner)
        && mapping
            .name
            .eq_ignore_ascii_case(&context.repository.remote.name)
    {
        Ok(&mapping.id)
    } else {
        Err(CliError::InvalidArguments {
            message: format!(
                "repository mapping {} does not match migration target {}",
                mapping.id,
                context.repository.remote.repository_label()
            ),
        })
    }
}

fn write_migration_execution_report<W>(
    output: &mut W,
    context: &MigrationExecutionContext,
    mapping: &RepositoryMapping,
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
    writeln!(output, "  repository namespace: {}", mapping.id)?;
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
        result.storage_upload.uploaded_objects.len(),
        result.storage_upload.already_present_objects.len()
    )?;
    writeln!(
        output,
        "  durable receipt: {}",
        result.storage_upload.checkpoint_path.display()
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
            "    use the durable receipt above with the source provider's supported cleanup process"
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
    config_path: PathBuf,
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

fn migration_readiness_checks<P, A, S>(
    config_path: &Path,
    repository: &GitRepository,
    target: MigrationTargetReadiness<'_>,
    discovery: &GitLfsMigrationDiscovery,
    probe_server: &mut P,
    lookup_credential: &mut A,
    validate_storage: &mut S,
) -> Vec<MigrationReadinessCheck>
where
    P: FnMut(&str) -> CliResult<()>,
    A: FnMut(&str) -> CliResult<()>,
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
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

    match ServerConfig::load_from_path(config_path) {
        Ok(config) => {
            checks.push(MigrationReadinessCheck {
                name: "config",
                level: StatusLevel::Ok,
                message: format!("loaded {}", config_path.display()),
            });
            migration_config_readiness_checks(&mut checks, &config, repository, validate_storage);
        }
        Err(error) => checks.push(MigrationReadinessCheck {
            name: "config",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        }),
    }

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

fn migration_config_readiness_checks<S>(
    checks: &mut Vec<MigrationReadinessCheck>,
    config: &ServerConfig,
    repository: &GitRepository,
    validate_storage: &mut S,
) where
    S: FnMut(&StorageProviderConfig) -> CliResult<()>,
{
    let Some(mapping) = config.repository_mapping_for_identity(
        &repository.remote.host,
        &repository.remote.owner,
        &repository.remote.name,
    ) else {
        checks.push(MigrationReadinessCheck {
            name: "mapping",
            level: StatusLevel::Warning,
            message: format!(
                "no server config entry for {}",
                repository.remote.repository_label()
            ),
        });
        return;
    };

    checks.push(MigrationReadinessCheck {
        name: "mapping",
        level: StatusLevel::Ok,
        message: format!("{} -> {}", mapping.id, mapping.storage_provider),
    });

    let Some(storage) = config.storage_providers.get(&mapping.storage_provider) else {
        checks.push(MigrationReadinessCheck {
            name: "storage-credential",
            level: StatusLevel::Warning,
            message: format!(
                "mapping {} references unknown storage provider {}",
                mapping.id, mapping.storage_provider
            ),
        });
        return;
    };

    checks.push(match validate_storage(storage) {
        Ok(()) => MigrationReadinessCheck {
            name: "storage-credential",
            level: StatusLevel::Ok,
            message: format!(
                "{} {} credential loads locally; Drive root access not probed",
                storage.provider_type(),
                storage.id()
            ),
        },
        Err(error) => MigrationReadinessCheck {
            name: "storage-credential",
            level: StatusLevel::Warning,
            message: format!("{error}"),
        },
    });
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
    writeln!(output, "  config: {}", report.config_path.display())?;
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
        "    target storage not probed during dry-run; execution checks existence before upload"
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
        "    warning: target storage quota and free capacity were not probed"
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
                "    requirement: generate purge input only from a durable, integrity-verified migration receipt; planned objects are not proof of upload."
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
        http::{HeaderMap, StatusCode},
        routing::post,
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
        GitCredentialRejection, GitLfsConfigChange, GitLfsConfigTarget, GitRepository,
        GoogleDriveStorageConfig, LfsObject, LfsObjectSize, LfsOid, LfsPointer, LfsSessionToken,
        LocalCacheError, LocalCacheLayout, LocalCacheWorktreeRegistration, ProviderFuture,
        RepositoryMapping, SanitizedMessage, ServeOptions, ServerConfig, StorageDeleteOutcome,
        StorageError, StorageProvider, StorageProviderConfig, StorageResult, StoredObject,
    };

    struct RecordingMigrationStorage {
        provider_id: String,
        objects: Mutex<BTreeMap<LfsObject, Vec<u8>>>,
        failing_object: Option<LfsObject>,
    }

    impl RecordingMigrationStorage {
        fn new(provider_id: impl Into<String>) -> Self {
            Self {
                provider_id: provider_id.into(),
                objects: Mutex::new(BTreeMap::new()),
                failing_object: None,
            }
        }

        fn failing(mut self, object: LfsObject) -> Self {
            self.failing_object = Some(object);
            self
        }

        fn object_bytes(&self, object: &LfsObject) -> Option<Vec<u8>> {
            self.objects
                .lock()
                .expect("recording migration storage lock should not poison")
                .get(object)
                .cloned()
        }
    }

    impl StorageProvider for RecordingMigrationStorage {
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
                    .objects
                    .lock()
                    .expect("recording migration storage lock should not poison")
                    .contains_key(object)
                    .then(|| {
                        StoredObject::new(
                            &self.provider_id,
                            repository_namespace,
                            object.clone(),
                            format!("recorded-{}", object.oid.as_hex()),
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
                if self.failing_object.as_ref() == Some(object) {
                    return Err(StorageError::Retryable {
                        provider: self.provider_id.clone(),
                        message: "simulated migration upload failure".to_owned(),
                    });
                }
                let bytes = fs::read(source).map_err(|error| StorageError::StagedFileRead {
                    provider: self.provider_id.clone(),
                    path: source.to_path_buf(),
                    source: error,
                })?;
                self.objects
                    .lock()
                    .expect("recording migration storage lock should not poison")
                    .insert(object.clone(), bytes);
                Ok(StoredObject::new(
                    &self.provider_id,
                    repository_namespace,
                    object.clone(),
                    format!("recorded-{}", object.oid.as_hex()),
                ))
            })
        }

        fn download_object<'a>(
            &'a self,
            _repository_namespace: &'a str,
            object: &'a LfsObject,
            _destination: &'a Path,
        ) -> ProviderFuture<'a, StorageResult<StoredObject>> {
            Box::pin(async move {
                Err(StorageError::ObjectNotFound {
                    provider: self.provider_id.clone(),
                    oid: object.oid.as_hex().to_owned(),
                    size: object.size.bytes(),
                })
            })
        }

        fn delete_or_mark_object<'a>(
            &'a self,
            _repository_namespace: &'a str,
            _object: &'a LfsObject,
        ) -> ProviderFuture<'a, StorageResult<StorageDeleteOutcome>> {
            Box::pin(async {
                Ok(StorageDeleteOutcome::Retained {
                    reason: "test storage retains migration objects".to_owned(),
                })
            })
        }
    }

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

    #[tokio::test]
    async fn migration_provider_readiness_failure_remains_in_migration_domain() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
        assert!(
            !temp.path().join(".gcloud-drive").exists(),
            "fresh test config must use a deterministically absent ADC directory"
        );
        let repository =
            GitRepository::discover(&repo).expect("temporary repository should be discovered");

        let error = match migration_storage_provider(&config_path, &repository).await {
            Ok(_) => panic!("missing Drive credentials should fail provider construction"),
            Err(error) => error,
        };

        let CliError::Migration {
            source:
                crate::MigrationError::Storage {
                    source:
                        StorageError::CredentialLoad {
                            provider,
                            reference,
                            message,
                        },
                },
        } = error
        else {
            panic!("expected migration-wrapped Drive credential error");
        };
        assert_eq!(provider, "drive-user-a");
        assert_eq!(reference, "gcloud");
        assert!(message.as_str().contains("directory is missing"));
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
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
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
            Some(config_path),
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
            |_| Ok(()),
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
        assert!(rendered.contains("storage-credential"));
        assert!(rendered.contains("source repository access not probed"));
        assert!(rendered.contains("server authentication and repository access not probed"));
        assert!(rendered.contains("Drive root access not probed"));
        assert!(rendered.contains("warnings:"));
        assert!(rendered.contains("repository permissions were not probed"));
        assert!(rendered.contains("storage quota and free capacity were not probed"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[tokio::test]
    async fn migrate_execution_uploads_every_historical_asset_version_before_reconfiguring() {
        require_git();
        require_git_lfs();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.name", "LFS Cloud Migration Test"]);
        run_git(
            &repo,
            &["config", "user.email", "migration@example.invalid"],
        );
        run_git(&repo, &["config", "commit.gpgSign", "false"]);
        run_git(&repo, &["lfs", "install", "--local"]);

        let first_bytes = b"historical LFS asset version one\n";
        let latest_bytes = b"latest LFS asset version two with different bytes\n";
        let first_object = object_for_bytes(first_bytes);
        let latest_object = object_for_bytes(latest_bytes);
        write_file(
            &repo.join(".gitattributes"),
            b"assets/*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        write_file(
            &repo.join("assets/model.bin"),
            LfsPointer::new(first_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_git_lfs_source_object(&repo, &first_object, first_bytes);
        run_git(&repo, &["add", ".gitattributes", "assets/model.bin"]);
        run_git(&repo, &["commit", "-m", "Add first LFS asset version"]);
        let first_commit = read_git_config(&repo, &["rev-parse", "HEAD"]);

        write_file(
            &repo.join("assets/model.bin"),
            LfsPointer::new(latest_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_git_lfs_source_object(&repo, &latest_object, latest_bytes);
        run_git(&repo, &["add", "assets/model.bin"]);
        run_git(&repo, &["commit", "-m", "Change LFS asset bytes"]);

        let command = MigrateCommand {
            server: "http://127.0.0.1:8080".to_owned(),
            allow_insecure_http: false,
            cache_root: Some(temp.path().join("cache")),
            source_remote: "origin".to_owned(),
            allow_cross_remote: false,
            refs: Vec::new(),
            all_refs: true,
            dry_run: false,
            purge_source_lfs: false,
        };
        let context = prepare_migration_execution(command, &repo)
            .expect("historical migration execution should prepare")
            .scan_fetched_refs()
            .expect("historical migration execution should scan fetched refs");
        let mapping = RepositoryMapping {
            id: "github-main:owner/repo".to_owned(),
            repo_provider: "github-main".to_owned(),
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
            provider_repository_id: "8675309".to_owned(),
            storage_provider: "drive-user-a".to_owned(),
        };
        let storage = RecordingMigrationStorage::new("drive-user-a");

        let result = execute_migration_with_storage(&context, &mapping, &storage)
            .await
            .expect("historical migration should complete");

        assert_eq!(context.scan.objects.len(), 2);
        assert_eq!(result.storage_upload.uploaded_objects.len(), 2);
        assert!(result.storage_upload.failed_objects.is_empty());
        assert_eq!(
            storage.object_bytes(&first_object).as_deref(),
            Some(first_bytes.as_slice())
        );
        assert_eq!(
            storage.object_bytes(&latest_object).as_deref(),
            Some(latest_bytes.as_slice())
        );
        let receipt = fs::read_to_string(&result.storage_upload.checkpoint_path)
            .expect("durable migration receipt should be readable");
        assert_eq!(receipt.lines().count(), 2);
        assert!(receipt.contains(first_object.oid.as_hex()));
        assert!(receipt.contains(latest_object.oid.as_hex()));

        let target_url = "http://127.0.0.1:8080/github.com/owner/repo.git/info/lfs";
        assert!(
            fs::read_to_string(repo.join(".lfsconfig"))
                .expect("migrated .lfsconfig should be readable")
                .contains(target_url)
        );
        assert_eq!(
            read_git_config(&repo, &["config", "--local", "--get", "lfs.url"]),
            target_url
        );

        run_git(&repo, &["checkout", "--quiet", &first_commit]);
        assert_eq!(
            read_git_config(&repo, &["config", "--local", "--get", "lfs.url"]),
            target_url,
            "the local override must keep pre-.lfsconfig history on LFS Cloud"
        );
        assert_eq!(
            fs::read(repo.join("assets/model.bin"))
                .expect("historical LFS asset should remain materializable"),
            first_bytes
        );
    }

    #[tokio::test]
    async fn migrate_execution_does_not_reconfigure_after_a_partial_upload() {
        require_git();
        require_git_lfs();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.name", "LFS Cloud Migration Test"]);
        run_git(
            &repo,
            &["config", "user.email", "migration@example.invalid"],
        );
        run_git(&repo, &["config", "commit.gpgSign", "false"]);
        run_git(&repo, &["lfs", "install", "--local"]);
        let bytes = b"migration object that will fail at the target\n";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs -text\n");
        write_file(
            &repo.join("asset.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        write_git_lfs_source_object(&repo, &object, bytes);
        run_git(&repo, &["add", ".gitattributes", "asset.bin"]);
        run_git(&repo, &["commit", "-m", "Add LFS asset"]);
        let context = prepare_migration_execution(
            MigrateCommand {
                server: "http://127.0.0.1:8080".to_owned(),
                allow_insecure_http: false,
                cache_root: Some(temp.path().join("cache")),
                source_remote: "origin".to_owned(),
                allow_cross_remote: false,
                refs: Vec::new(),
                all_refs: true,
                dry_run: false,
                purge_source_lfs: false,
            },
            &repo,
        )
        .expect("migration execution should prepare")
        .scan_fetched_refs()
        .expect("migration execution should scan fetched refs");
        let mapping = RepositoryMapping {
            id: "github-main:owner/repo".to_owned(),
            repo_provider: "github-main".to_owned(),
            host: "github.com".to_owned(),
            owner: "owner".to_owned(),
            name: "repo".to_owned(),
            provider_repository_id: "8675309".to_owned(),
            storage_provider: "drive-user-a".to_owned(),
        };
        let storage = RecordingMigrationStorage::new("drive-user-a").failing(object.clone());

        let error = execute_migration_with_storage(&context, &mapping, &storage)
            .await
            .expect_err("partial target upload should fail migration execution");

        assert!(
            matches!(error, CliError::MigrationUploadFailed { failures: 1, oid, .. }
            if oid == object.oid.as_hex())
        );
        assert!(!repo.join(".lfsconfig").exists());
        let local_url = ProcessCommand::new("git")
            .args(["config", "--local", "--get", "lfs.url"])
            .current_dir(&repo)
            .output()
            .expect("local Git config lookup should start");
        assert_eq!(local_url.status.code(), Some(1));
        assert!(local_url.stdout.is_empty());
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
            Some(temp.path().join("missing-config.yml")),
            &repo,
            &mut denied_output,
            |_| Ok(()),
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
            Some(temp.path().join("missing-config.yml")),
            &repo,
            &mut allowed_output,
            |_| Ok(()),
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
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
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
            Some(config_path),
            &repo,
            &mut output,
            |_| {
                Err(CliError::InvalidArguments {
                    message: "probe failed".to_owned(),
                })
            },
            |_| Ok(()),
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
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
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
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
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
        assert!(rendered.contains("durable, integrity-verified migration receipt"));
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
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
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
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
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
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
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
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
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
        let config_path = temp.path().join("lfscloud.yml");
        fs::write(&config_path, status_config("http://127.0.0.1:8080"))
            .expect("status config should be written");
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
            Some(config_path),
            &repo,
            &mut output,
            |_| Ok(()),
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
