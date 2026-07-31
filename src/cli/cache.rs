//! Pull, hydrate, dehydrate, and garbage-collection command handling.

use super::*;

const PULL_FETCH_TIMEOUT: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_PULL_FETCH_OUTPUT_BYTES: usize = 256 * 1024;
pub(super) fn run_pull_to_stdout(command: PullCommand) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_pull_from_dir(command, &current_dir, &mut stdout, fetch_git_lfs_objects)
        .map_err(anyhow::Error::from)
}

pub(super) fn run_hydrate_to_stdout(command: HydrateCommand) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_hydrate_from_dir(command, &current_dir, &mut stdout).map_err(anyhow::Error::from)
}

pub(super) fn run_dehydrate_to_stdout(command: DehydrateCommand) -> anyhow::Result<()> {
    let current_dir = std::env::current_dir().context("failed to determine current directory")?;
    let mut stdout = io::stdout().lock();

    run_dehydrate_from_dir(command, &current_dir, &mut stdout).map_err(anyhow::Error::from)
}

pub(super) fn run_gc_to_stdout(command: GcCommand) -> anyhow::Result<()> {
    let mut stdout = io::stdout().lock();
    let current_dir = std::env::current_dir().context("failed to read current directory")?;

    run_gc_from_dir(command, &current_dir, &mut stdout).map_err(anyhow::Error::from)
}

fn run_pull_from_dir<W, F>(
    command: PullCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
    mut fetch_lfs_objects: F,
) -> CliResult<()>
where
    W: Write,
    F: FnMut(&Path) -> CliResult<()>,
{
    let layout = local_cache_layout(command.cache_root)?;
    let repository = GitRepository::discover(start_dir.as_ref())?;
    let git_lfs_objects_dir = git_lfs_objects_dir(&repository)?;

    fetch_lfs_objects(&repository.worktree_root)?;
    register_current_worktree(&layout, &repository.worktree_root)?;

    let pointer_scan = current_checkout_lfs_pointer_scan(&repository.worktree_root)?;
    writeln!(output, "lfscloud pull").map_err(output_error)?;
    writeln!(output, "  fetched Git LFS objects").map_err(output_error)?;
    writeln!(
        output,
        "  tracked paths: {}",
        pointer_scan.tracked_path_count
    )
    .map_err(output_error)?;
    writeln!(output, "  pointers: {}", pointer_scan.pointer_files.len()).map_err(output_error)?;

    let mut first_failure = None;
    let mut failure_count = 0;
    for pointer_file in pointer_scan.pointer_files {
        let result = layout
            .ingest_git_lfs_object(&git_lfs_objects_dir, &pointer_file.object)
            .map_err(local_cache_cli_error)
            .and_then(|ingest| {
                layout
                    .hydrate_pointer_file(&pointer_file.path)
                    .map_err(local_cache_cli_error)
                    .map(|materialization| (ingest, materialization))
            });

        match result {
            Ok((ingest, materialization)) => {
                write_pull_result(output, &ingest, &materialization).map_err(output_error)?;
            }
            Err(error) => {
                failure_count += 1;
                writeln!(output, "failed {}: {}", pointer_file.path.display(), error)
                    .map_err(output_error)?;
                first_failure.get_or_insert_with(|| {
                    (pointer_file.path, SanitizedMessage::new(error.to_string()))
                });
            }
        }
    }

    if let Some((path, message)) = first_failure {
        return Err(CliError::PullFailed {
            failures: failure_count,
            path,
            message,
        });
    }

    Ok(())
}

fn run_hydrate_from_dir<W>(
    command: HydrateCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    let layout = local_cache_layout(command.cache_root)?;
    let start_dir = start_dir.as_ref();
    let repository = GitRepository::discover(start_dir)?;
    register_worktree(&layout, &repository)?;

    for path in command.paths {
        let path = resolve_cli_path(start_dir, &path);
        let path = contained_worktree_file_path(&repository.worktree_root, &path, "hydration")?;
        let materialization = layout
            .hydrate_pointer_file(&path)
            .map_err(local_cache_cli_error)?;
        write_hydrate_result(output, &materialization).map_err(output_error)?;
    }

    Ok(())
}

fn run_dehydrate_from_dir<W>(
    command: DehydrateCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    let layout = local_cache_layout(command.cache_root)?;
    let start_dir = start_dir.as_ref();
    let repository = GitRepository::discover(start_dir)?;
    register_worktree(&layout, &repository)?;
    let git_lfs_objects_dir = git_lfs_objects_dir(&repository)?;

    for path in command.paths {
        let path = resolve_cli_path(start_dir, &path);
        let path = contained_worktree_file_path(&repository.worktree_root, &path, "dehydration")?;
        let object = indexed_lfs_object_for_dehydration(&repository.worktree_root, &path)?;
        let dehydration = layout
            .dehydrate_file(&object, &path)
            .map_err(local_cache_cli_error)?;
        publish_dehydrated_object_to_git_lfs(&layout, &git_lfs_objects_dir, &dehydration)?;
        write_dehydrate_result(output, &dehydration).map_err(output_error)?;
    }

    Ok(())
}

fn run_gc_from_dir<W>(
    command: GcCommand,
    start_dir: impl AsRef<Path>,
    output: &mut W,
) -> CliResult<()>
where
    W: Write,
{
    let layout = local_cache_layout(command.cache_root)?;
    register_current_worktree_for_gc(&layout, start_dir.as_ref())?;
    let report = layout
        .garbage_collect(command.dry_run, command.prune_unavailable_worktrees)
        .map_err(local_cache_cli_error)?;

    write_gc_result(output, layout.root(), &report).map_err(output_error)
}

fn register_current_worktree_for_gc(layout: &LocalCacheLayout, start_dir: &Path) -> CliResult<()> {
    match register_current_worktree(layout, start_dir) {
        Ok(()) => Ok(()),
        Err(error) if is_git_worktree_discovery_error(&error) => Ok(()),
        Err(error) => Err(error),
    }
}

fn is_git_worktree_discovery_error(error: &CliError) -> bool {
    match error {
        CliError::ExternalCommand {
            command, stderr, ..
        } if command == "git rev-parse --show-toplevel" => {
            let stderr = stderr.as_str();
            stderr.contains("not a git repository")
                || stderr.contains("this operation must be run in a work tree")
        }
        _ => false,
    }
}

fn register_current_worktree(layout: &LocalCacheLayout, start_dir: &Path) -> CliResult<()> {
    let repository = GitRepository::discover(start_dir)?;
    register_worktree(layout, &repository)
}

fn register_worktree(layout: &LocalCacheLayout, repository: &GitRepository) -> CliResult<()> {
    let repository_id = repository.remote.repository_label();
    let git_dir = repository.git_dir_path()?;
    let registration = LocalCacheWorktreeRegistration::new(
        repository_id,
        repository.worktree_root.clone(),
        git_dir,
    )
    .map_err(local_cache_cli_error)?;

    layout
        .register_worktree(registration)
        .map_err(local_cache_cli_error)?;

    Ok(())
}

pub(super) fn local_cache_layout(cache_root: Option<PathBuf>) -> CliResult<LocalCacheLayout> {
    match cache_root {
        Some(cache_root) => Ok(LocalCacheLayout::new(cache_root)),
        None => match default_cache_home_dir() {
            Some(home_dir) => Ok(LocalCacheLayout::from_home_dir(home_dir)),
            None => Err(default_cache_root_error()),
        },
    }
}

pub(super) fn default_cache_home_dir() -> Option<OsString> {
    std::env::var_os("HOME").or_else(|| {
        if cfg!(windows) {
            std::env::var_os("USERPROFILE")
        } else {
            None
        }
    })
}

pub(super) fn default_cache_root_error() -> CliError {
    CliError::InvalidArguments {
        message: if cfg!(windows) {
            "HOME or USERPROFILE is not set and --cache-root was not provided".to_owned()
        } else {
            "HOME is not set and --cache-root was not provided".to_owned()
        },
    }
}

fn resolve_cli_path(start_dir: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        start_dir.join(path)
    }
}

#[derive(Debug, Eq, PartialEq)]
struct CurrentCheckoutLfsPointerFile {
    path: PathBuf,
    object: LfsObject,
}

#[derive(Debug, Eq, PartialEq)]
struct CurrentCheckoutLfsPointerScan {
    tracked_path_count: usize,
    pointer_files: Vec<CurrentCheckoutLfsPointerFile>,
}

fn fetch_git_lfs_objects(worktree_root: &Path) -> CliResult<()> {
    let mut command = ProcessCommand::new("git");
    command.args(["lfs", "fetch"]).current_dir(worktree_root);
    let output = run_bounded_child_command(
        &mut command,
        "git lfs fetch",
        PULL_FETCH_TIMEOUT,
        MAX_PULL_FETCH_OUTPUT_BYTES,
    )?;

    if output.status.success() {
        Ok(())
    } else {
        Err(CliError::ExternalCommand {
            command: "git lfs fetch".to_owned(),
            status: command_status_text(output.status),
            stderr: sanitized_external_failure_output(&output.stderr, &output.stdout),
        })
    }
}

/// Runs a child while bounding its lifetime and retained output.
///
/// Both output streams are drained on separate threads so a chatty stream
/// cannot fill its OS pipe while the parent waits on the other stream. Each
/// reader retains at most `max_output_bytes`; crossing either limit terminates
/// the whole process tree instead of merely truncating an otherwise unbounded
/// producer.
fn run_bounded_child_command(
    command: &mut ProcessCommand,
    command_name: &str,
    timeout: Duration,
    max_output_bytes: usize,
) -> CliResult<ChildProcessOutput> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_tree(command);

    let mut child = command.spawn().map_err(|source| CliError::Io {
        context: format!("failed to start {command_name}"),
        source,
    })?;
    wait_for_child(
        &mut child,
        command_name,
        ChildProcessOptions {
            timeout: Some(timeout),
            stdout: PipeCapture::HardLimit {
                limit: max_output_bytes,
            },
            stderr: PipeCapture::HardLimit {
                limit: max_output_bytes,
            },
            inherited_pipe_is_error: true,
        },
    )
    .map_err(|error| child_process_cli_error(error, command_name))
}

fn child_process_cli_error(error: ChildProcessError, command_name: &str) -> CliError {
    match error {
        ChildProcessError::Io { context, source } => CliError::Io { context, source },
        ChildProcessError::TimedOut {
            timeout,
            stdout,
            stderr,
        } => CliError::ExternalCommand {
            command: command_name.to_owned(),
            status: format!("timed out after {} seconds", timeout.as_secs_f64()),
            stderr: sanitized_external_failure_output(&stderr, &stdout),
        },
        ChildProcessError::OutputLimit { stream, limit } => CliError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new(format!("{stream} exceeded the {limit}-byte limit")),
        },
        ChildProcessError::InheritedPipe => CliError::Io {
            context: format!("timed out draining output from {command_name}"),
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                "child output pipes remained open after process exit",
            ),
        },
    }
}

#[cfg(test)]
fn current_checkout_lfs_pointer_files(
    worktree_root: &Path,
) -> CliResult<Vec<CurrentCheckoutLfsPointerFile>> {
    Ok(current_checkout_lfs_pointer_scan(worktree_root)?.pointer_files)
}

fn current_checkout_lfs_pointer_scan(
    worktree_root: &Path,
) -> CliResult<CurrentCheckoutLfsPointerScan> {
    let lfs_tracked_paths = current_checkout_lfs_tracked_paths(worktree_root)?;
    let mut pointer_files = Vec::new();
    for relative_path in &lfs_tracked_paths {
        let path = worktree_root.join(relative_path);
        let Some(pointer) = read_current_checkout_pointer_candidate(&path)? else {
            continue;
        };
        if pointer.is_empty() {
            continue;
        }

        pointer_files.push(CurrentCheckoutLfsPointerFile {
            path,
            object: pointer.object,
        });
    }

    Ok(CurrentCheckoutLfsPointerScan {
        tracked_path_count: lfs_tracked_paths.len(),
        pointer_files,
    })
}

fn current_checkout_lfs_tracked_paths(worktree_root: &Path) -> CliResult<Vec<PathBuf>> {
    let output = ProcessCommand::new("git")
        .args(["ls-files", "-z"])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git ls-files -z".to_owned(),
            source,
        })?;

    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git ls-files -z".to_owned(),
            status: command_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    let tracked_paths = output.stdout;
    let mut lfs_tracked_paths = Vec::new();
    if tracked_paths.is_empty() {
        return Ok(lfs_tracked_paths);
    }

    let output = git_check_attr_filter(worktree_root, &tracked_paths)?;
    lfs_tracked_paths.extend(
        parse_lfs_filter_attribute_paths(&output.stdout).map_err(|error| {
            cli_git_path_output_error(error, "git check-attr -z --stdin filter")
        })?,
    );

    Ok(lfs_tracked_paths)
}

fn git_check_attr_filter(
    worktree_root: &Path,
    tracked_paths: &[u8],
) -> CliResult<std::process::Output> {
    let mut child = ProcessCommand::new("git")
        .args(["check-attr", "-z", "--stdin", "filter"])
        .current_dir(worktree_root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| CliError::Io {
            context: "failed to start git check-attr -z --stdin filter".to_owned(),
            source,
        })?;

    let mut stdin = child.stdin.take().expect("child stdin should be piped");
    let tracked_paths = tracked_paths.to_owned();
    let stdin_writer = std::thread::spawn(move || {
        let write_result = stdin.write_all(&tracked_paths);
        drop(stdin);

        write_result
    });

    let output = child.wait_with_output().map_err(|source| CliError::Io {
        context: "failed to wait for git check-attr -z --stdin filter".to_owned(),
        source,
    })?;

    let write_result = stdin_writer.join().map_err(|_| CliError::Io {
        context: "git check-attr input writer panicked".to_owned(),
        source: io::Error::other("git check-attr input writer panicked"),
    })?;

    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git check-attr -z --stdin filter".to_owned(),
            status: command_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    write_result.map_err(|source| CliError::Io {
        context: "failed to write git check-attr path input".to_owned(),
        source,
    })?;

    Ok(output)
}

fn git_lfs_objects_dir(repository: &GitRepository) -> CliResult<PathBuf> {
    let git_common_dir = repository.git_common_dir_path()?;
    let storage_dir = match configured_git_lfs_storage_dir(&repository.worktree_root)? {
        Some(storage_dir) if storage_dir.is_absolute() => storage_dir,
        Some(storage_dir) => git_common_dir.join(storage_dir),
        None => git_common_dir.join("lfs"),
    };

    Ok(storage_dir.join("objects"))
}

fn configured_git_lfs_storage_dir(worktree_root: &Path) -> CliResult<Option<PathBuf>> {
    let output = ProcessCommand::new("git")
        .args(["config", "--get", "lfs.storage"])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git config --get lfs.storage".to_owned(),
            source,
        })?;

    if output.status.success() {
        let storage =
            String::from_utf8(output.stdout).map_err(|_| CliError::ExternalCommandOutput {
                command: "git config --get lfs.storage".to_owned(),
                message: SanitizedMessage::new("git returned non-UTF-8 lfs.storage output"),
            })?;
        let storage = storage.trim_end();

        Ok((!storage.is_empty()).then(|| PathBuf::from(storage)))
    } else if output.status.code() == Some(1) {
        Ok(None)
    } else {
        Err(CliError::ExternalCommand {
            command: "git config --get lfs.storage".to_owned(),
            status: command_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        })
    }
}

fn cli_git_path_output_error(error: GitPathOutputError, command: &str) -> CliError {
    let message = match error {
        GitPathOutputError::MalformedAttributeOutput => "git returned malformed attribute output",
        #[cfg(not(unix))]
        GitPathOutputError::NonUtf8Path => "git returned non-UTF-8 path output",
        GitPathOutputError::PathOutsideWorktree => "git returned a path outside the worktree",
    };
    CliError::ExternalCommandOutput {
        command: command.to_owned(),
        message: SanitizedMessage::new(message),
    }
}

fn read_current_checkout_pointer_candidate(path: &Path) -> CliResult<Option<LfsPointer>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(CliError::Io {
                context: format!("failed to inspect checkout path {}", path.display()),
                source,
            });
        }
    };
    if !metadata.is_file() || metadata.len() >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(None);
    }

    let contents = fs::read(path).map_err(|source| CliError::Io {
        context: format!("failed to read checkout path {}", path.display()),
        source,
    })?;
    let Ok(contents) = std::str::from_utf8(&contents) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents).ok())
}

// `contained_path` must come from `contained_worktree_file_path`, which
// canonicalizes its parent and rejects symlinks or traversal outside the
// worktree before Git sees the repository-relative path.
fn indexed_lfs_object_for_dehydration(
    worktree_root: &Path,
    contained_path: &Path,
) -> CliResult<LfsObject> {
    let relative_path =
        dehydration_relative_path_from_contained_file(worktree_root, contained_path)?;
    require_lfs_filter(worktree_root, &relative_path, contained_path)?;
    let blob_oid = index_blob_oid(worktree_root, &relative_path, contained_path)?;
    let pointer = read_index_lfs_pointer(worktree_root, &blob_oid, contained_path)?;

    Ok(pointer.object)
}

fn dehydration_relative_path_from_contained_file(
    worktree_root: &Path,
    contained_path: &Path,
) -> CliResult<PathBuf> {
    let root = dunce::canonicalize(worktree_root).map_err(|source| CliError::Io {
        context: format!(
            "failed to resolve Git worktree root {}",
            worktree_root.display()
        ),
        source,
    })?;
    contained_path
        .strip_prefix(&root)
        .map(Path::to_path_buf)
        .map_err(|_| CliError::InvalidArguments {
            message: format!(
                "dehydration path must be contained in the current Git worktree: {}",
                contained_path.display()
            ),
        })
}

fn contained_worktree_file_path(
    worktree_root: &Path,
    path: &Path,
    operation: &'static str,
) -> CliResult<PathBuf> {
    let root = dunce::canonicalize(worktree_root).map_err(|source| CliError::Io {
        context: format!(
            "failed to resolve Git worktree root {}",
            worktree_root.display()
        ),
        source,
    })?;
    let parent = path.parent().ok_or_else(|| CliError::InvalidArguments {
        message: format!(
            "{operation} path must be contained in the current Git worktree: {}",
            path.display()
        ),
    })?;
    let parent = dunce::canonicalize(parent).map_err(|source| CliError::Io {
        context: format!("failed to resolve {operation} path {}", path.display()),
        source,
    })?;
    let relative_parent = parent
        .strip_prefix(&root)
        .map_err(|_| CliError::InvalidArguments {
            message: format!(
                "{operation} path must be contained in the current Git worktree: {}",
                path.display()
            ),
        })?;
    let file_name = path.file_name().ok_or_else(|| CliError::InvalidArguments {
        message: format!("{operation} path is not a file: {}", path.display()),
    })?;
    let path = root.join(relative_parent).join(file_name);
    let metadata = fs::symlink_metadata(&path).map_err(|source| CliError::Io {
        context: format!("failed to inspect {operation} path {}", path.display()),
        source,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(CliError::InvalidArguments {
            message: format!(
                "{operation} path must be a regular file and not a symbolic link: {}",
                path.display()
            ),
        });
    }

    Ok(path)
}

fn require_lfs_filter(
    worktree_root: &Path,
    relative_path: &Path,
    display_path: &Path,
) -> CliResult<()> {
    let output = ProcessCommand::new("git")
        .args(["check-attr", "-z", "filter", "--"])
        .arg(relative_path)
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git check-attr -z filter".to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git check-attr -z filter -- <path>".to_owned(),
            status: command_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    let mut fields = output.stdout.split(|byte| *byte == b'\0');
    let returned_path = fields.next();
    let attribute = fields.next();
    let value = fields.next();
    let terminator = fields.next();
    if returned_path.is_none()
        || attribute != Some(&b"filter"[..])
        || value != Some(&b"lfs"[..])
        || terminator != Some(&[][..])
        || fields.next().is_some()
    {
        return Err(CliError::InvalidArguments {
            message: format!(
                "dehydration path must be tracked with filter=lfs: {}",
                display_path.display()
            ),
        });
    }

    Ok(())
}

fn index_blob_oid(
    worktree_root: &Path,
    relative_path: &Path,
    display_path: &Path,
) -> CliResult<String> {
    let output = ProcessCommand::new("git")
        .args(["--literal-pathspecs", "ls-files", "--stage", "-z", "--"])
        .arg(relative_path)
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git ls-files --stage -z".to_owned(),
            source,
        })?;
    if !output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git --literal-pathspecs ls-files --stage -z -- <path>".to_owned(),
            status: command_status_text(output.status),
            stderr: sanitized_external_stderr(&output.stderr),
        });
    }

    let records = output
        .stdout
        .split(|byte| *byte == b'\0')
        .filter(|record| !record.is_empty())
        .collect::<Vec<_>>();
    let [record] = records.as_slice() else {
        return Err(CliError::InvalidArguments {
            message: format!(
                "dehydration path must have one tracked index entry: {}",
                display_path.display()
            ),
        });
    };
    let Some(separator) = record.iter().position(|byte| *byte == b'\t') else {
        return Err(index_entry_parse_error());
    };
    let metadata = &record[..separator];
    let fields = metadata
        .split(|byte| *byte == b' ')
        .filter(|field| !field.is_empty())
        .collect::<Vec<_>>();
    let [_mode, oid, stage] = fields.as_slice() else {
        return Err(index_entry_parse_error());
    };
    if *stage != b"0" {
        return Err(CliError::InvalidArguments {
            message: format!(
                "dehydration path has an unmerged index entry: {}",
                display_path.display()
            ),
        });
    }

    std::str::from_utf8(oid)
        .map(str::to_owned)
        .map_err(|_| index_entry_parse_error())
}

fn index_entry_parse_error() -> CliError {
    CliError::ExternalCommandOutput {
        command: "git --literal-pathspecs ls-files --stage -z -- <path>".to_owned(),
        message: SanitizedMessage::new("git returned malformed index metadata"),
    }
}

fn read_index_lfs_pointer(
    worktree_root: &Path,
    blob_oid: &str,
    display_path: &Path,
) -> CliResult<LfsPointer> {
    let size_output = ProcessCommand::new("git")
        .args(["cat-file", "-s", blob_oid])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git cat-file -s".to_owned(),
            source,
        })?;
    if !size_output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git cat-file -s <index-object>".to_owned(),
            status: command_status_text(size_output.status),
            stderr: sanitized_external_stderr(&size_output.stderr),
        });
    }
    let size = std::str::from_utf8(&size_output.stdout)
        .ok()
        .and_then(|size| size.trim().parse::<u64>().ok())
        .ok_or_else(|| CliError::ExternalCommandOutput {
            command: "git cat-file -s <index-object>".to_owned(),
            message: SanitizedMessage::new("git returned an invalid index object size"),
        })?;
    if size >= LFS_POINTER_SIZE_CUTOFF {
        return Err(invalid_index_pointer_error(display_path));
    }

    let pointer_output = ProcessCommand::new("git")
        .args(["cat-file", "blob", blob_oid])
        .current_dir(worktree_root)
        .output()
        .map_err(|source| CliError::Io {
            context: "failed to start git cat-file blob".to_owned(),
            source,
        })?;
    if !pointer_output.status.success() {
        return Err(CliError::ExternalCommand {
            command: "git cat-file blob <index-object>".to_owned(),
            status: command_status_text(pointer_output.status),
            stderr: sanitized_external_stderr(&pointer_output.stderr),
        });
    }
    let contents = std::str::from_utf8(&pointer_output.stdout)
        .map_err(|_| invalid_index_pointer_error(display_path))?;

    LfsPointer::parse(contents).map_err(|_| invalid_index_pointer_error(display_path))
}

fn invalid_index_pointer_error(path: &Path) -> CliError {
    CliError::InvalidArguments {
        message: format!(
            "dehydration path must have a valid Git LFS pointer in the index: {}",
            path.display()
        ),
    }
}

fn publish_dehydrated_object_to_git_lfs(
    layout: &LocalCacheLayout,
    git_lfs_objects_dir: &Path,
    dehydration: &LocalCacheDehydration,
) -> CliResult<()> {
    if dehydration.status == LocalCacheDehydrationStatus::AlreadyDehydrated
        && !dehydration.cache_path.is_file()
    {
        return Ok(());
    }

    let oid = dehydration.object.oid.as_hex();
    let destination = git_lfs_objects_dir
        .join(&oid[..2])
        .join(&oid[2..4])
        .join(oid);
    layout
        .materialize_object(&dehydration.object, destination)
        .map_err(local_cache_cli_error)?;

    Ok(())
}

fn write_hydrate_result<W>(
    output: &mut W,
    materialization: &LocalCacheMaterialization,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "hydrated {} sha256:{} ({} bytes) {}",
        materialization.destination_path.display(),
        materialization.object.oid,
        materialization.object.size,
        materialization_status_label(materialization.status)
    )
}

fn write_dehydrate_result<W>(output: &mut W, dehydration: &LocalCacheDehydration) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "dehydrated {} sha256:{} ({} bytes) {}",
        dehydration.pointer_path.display(),
        dehydration.object.oid,
        dehydration.object.size,
        dehydration_status_label(dehydration.status)
    )
}

fn write_pull_result<W>(
    output: &mut W,
    ingest: &LocalCacheIngest,
    materialization: &LocalCacheMaterialization,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "pulled {} sha256:{} ({} bytes) {} {}",
        materialization.destination_path.display(),
        materialization.object.oid,
        materialization.object.size,
        ingest_status_label(ingest.status),
        materialization_status_label(materialization.status)
    )
}

fn write_gc_result<W>(
    output: &mut W,
    cache_root: &Path,
    report: &LocalCacheGarbageCollection,
) -> io::Result<()>
where
    W: Write,
{
    let action = if report.dry_run {
        "would remove"
    } else {
        "removed"
    };

    writeln!(output, "lfscloud gc")?;
    writeln!(output, "  cache: {}", cache_root.display())?;
    writeln!(
        output,
        "  worktrees: {} active, {} unavailable, {} {}",
        report.active_worktree_count,
        report.unavailable_worktrees.len(),
        report.pruned_worktrees.len(),
        if report.dry_run {
            "would prune"
        } else {
            "pruned"
        }
    )?;
    writeln!(
        output,
        "  objects: {} retained, {} protected, {} {}, {} cache paths skipped, {} worktree pointers skipped",
        report.retained_objects.len(),
        report.protected_objects.len(),
        report.unreferenced_objects.len(),
        action,
        report.skipped_cache_paths.len(),
        report.skipped_worktree_pointer_paths.len()
    )?;

    for object in &report.unreferenced_objects {
        write_gc_object(output, action, object)?;
    }
    for object in &report.protected_objects {
        write_gc_object(output, "protected while worktree unavailable", object)?;
    }
    for registration in &report.unavailable_worktrees {
        writeln!(
            output,
            "unavailable worktree {} ({})",
            registration.worktree_root.display(),
            registration.repository_id
        )?;
    }
    for registration in &report.pruned_worktrees {
        let action = if report.dry_run {
            "would prune"
        } else {
            "pruned"
        };
        writeln!(
            output,
            "{action} worktree {} ({})",
            registration.worktree_root.display(),
            registration.repository_id
        )?;
    }
    for path in &report.skipped_cache_paths {
        writeln!(output, "skipped cache path {}", path.display())?;
    }
    for path in &report.skipped_worktree_pointer_paths {
        writeln!(
            output,
            "skipped non-regular worktree pointer {}",
            path.display()
        )?;
    }

    Ok(())
}

fn write_gc_object<W>(
    output: &mut W,
    action: &str,
    object: &LocalCacheGarbageCollectionObject,
) -> io::Result<()>
where
    W: Write,
{
    writeln!(
        output,
        "{action} {} sha256:{} ({} bytes)",
        object.path.display(),
        object.oid,
        object.size_bytes
    )
}

fn materialization_status_label(status: LocalCacheMaterializationStatus) -> &'static str {
    match status {
        LocalCacheMaterializationStatus::AlreadyMaterialized => "already-materialized",
        LocalCacheMaterializationStatus::CopyOnWriteCloned => "copy-on-write-cloned",
        LocalCacheMaterializationStatus::Copied => "copied",
    }
}

fn ingest_status_label(status: LocalCacheIngestStatus) -> &'static str {
    match status {
        LocalCacheIngestStatus::AlreadyCached => "already-cached",
        LocalCacheIngestStatus::Copied => "cached",
    }
}

fn dehydration_status_label(status: LocalCacheDehydrationStatus) -> &'static str {
    match status {
        LocalCacheDehydrationStatus::AlreadyDehydrated => "already-dehydrated",
        LocalCacheDehydrationStatus::ReplacedWithPointer => "replaced-with-pointer",
        LocalCacheDehydrationStatus::CachedAndReplacedWithPointer => {
            "cached-and-replaced-with-pointer"
        }
    }
}

fn local_cache_cli_error(error: crate::LocalCacheError) -> CliError {
    CliError::LocalCache { source: error }
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::ffi::OsString;
    #[cfg(unix)]
    use std::os::unix::ffi::OsStringExt;
    #[cfg(unix)]
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

    #[test]
    fn pull_fetches_ingests_and_hydrates_current_checkout_pointers() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"object already fetched by git lfs";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        write_file(&repo.join("README.md"), b"not a pointer");
        run_git(
            &repo,
            &["add", ".gitattributes", "asset/model.bin", "README.md"],
        );
        write_git_lfs_source_object(&repo, &object, bytes);
        let fetched_root = Arc::new(Mutex::new(None));
        let fetched_root_for_runner = Arc::clone(&fetched_root);
        let mut output = Vec::new();

        run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &repo,
            &mut output,
            move |worktree_root| {
                *fetched_root_for_runner
                    .lock()
                    .expect("capture mutex should lock") = Some(worktree_root.to_path_buf());
                Ok(())
            },
        )
        .expect("pull should hydrate fetched objects");

        assert_eq!(
            *fetched_root.lock().expect("capture mutex should lock"),
            Some(dunce::canonicalize(&repo).expect("repo path should canonicalize"))
        );
        assert_eq!(
            fs::read(&worktree_file).expect("hydrated file should be readable"),
            bytes
        );
        let layout = LocalCacheLayout::new(cache_root);
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfscloud pull"));
        assert!(rendered.contains("fetched Git LFS objects"));
        assert!(rendered.contains("tracked paths: 1"));
        assert!(rendered.contains("pointers: 1"));
        assert!(rendered.contains("pulled"));
        assert!(rendered.contains("cached"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn pull_ingests_from_configured_git_lfs_storage_dir() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "lfs.storage", "custom-lfs"]);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"object fetched into custom git lfs storage";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        write_git_lfs_source_object_in(
            &repo.join(".git").join("custom-lfs").join("objects"),
            &object,
            bytes,
        );
        let mut output = Vec::new();

        run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &repo,
            &mut output,
            |_| Ok(()),
        )
        .expect("pull should hydrate from custom git lfs storage");

        assert_eq!(
            fs::read(&worktree_file).expect("hydrated file should be readable"),
            bytes
        );
        let layout = LocalCacheLayout::new(cache_root);
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
    }

    #[test]
    fn pull_ingests_from_git_common_dir_for_linked_worktree() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let linked = temp.path().join("linked");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.email", "lfscloud@example.invalid"]);
        run_git(&repo, &["config", "user.name", "LFS Cloud Test"]);
        let bytes = b"object fetched into common git lfs storage";
        let object = object_for_bytes(bytes);
        write_file(&repo.join(".gitattributes"), b"*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        run_git(&repo, &["commit", "-m", "add lfs pointer"]);
        let output = ProcessCommand::new("git")
            .args([
                "worktree",
                "add",
                linked.to_str().expect("test path should be UTF-8"),
            ])
            .current_dir(&repo)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .output()
            .expect("git worktree add should start");
        assert!(
            output.status.success(),
            "git worktree add failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        write_git_lfs_source_object(&repo, &object, bytes);
        let mut output = Vec::new();

        run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &linked,
            &mut output,
            |_| Ok(()),
        )
        .expect("pull should hydrate from the linked worktree common git dir");

        assert_eq!(
            fs::read(linked.join("asset/model.bin")).expect("hydrated file should be readable"),
            bytes
        );
        let layout = LocalCacheLayout::new(cache_root);
        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cache object should be readable"),
            bytes
        );
    }

    #[test]
    fn pull_propagates_git_lfs_fetch_failure_before_cache_mutation() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"not fetched");
        write_file(
            &repo.join("asset/model.bin"),
            LfsPointer::new(object).to_pointer_file().as_bytes(),
        );
        run_git(&repo, &["add", "asset/model.bin"]);
        let mut output = Vec::new();

        let error = run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root.clone()),
            },
            &repo,
            &mut output,
            |_| {
                Err(CliError::ExternalCommand {
                    command: "git lfs fetch".to_owned(),
                    status: "exit status: 2".to_owned(),
                    stderr: SanitizedMessage::new("git lfs is unavailable"),
                })
            },
        )
        .expect_err("fetch failure should stop pull");

        assert!(matches!(error, CliError::ExternalCommand { .. }));
        assert!(output.is_empty());
        assert!(
            !cache_root.exists(),
            "pull should not create cache state after fetch failure"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pull_process_runner_rejects_unbounded_concurrent_output() {
        let mut command = ProcessCommand::new("/bin/sh");
        command.args([
            "-c",
            "(while :; do printf 'stdout-data'; done) & \
             (while :; do printf 'stderr-data' >&2; done) & wait",
        ]);
        let started = Instant::now();

        let error = run_bounded_child_command(
            &mut command,
            "test pull fetch",
            Duration::from_secs(5),
            1024,
        )
        .expect_err("unbounded command output should be rejected");

        assert!(
            matches!(error, CliError::ExternalCommandOutput { message, .. }
                if message.as_str().contains("exceeded the 1024-byte limit"))
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "output overflow should stop the process before the timeout"
        );
    }

    #[cfg(unix)]
    #[test]
    fn pull_process_runner_terminates_descendants_on_timeout() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let escaped_marker = temp.path().join("escaped");
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .args(["-c", "(sleep 1; printf escaped > \"$1\") & wait", "sh"])
            .arg(&escaped_marker);

        let error = run_bounded_child_command(
            &mut command,
            "test pull fetch",
            Duration::from_millis(50),
            1024,
        )
        .expect_err("stalled command should time out");

        assert!(matches!(error, CliError::ExternalCommand { status, .. }
                if status.contains("timed out")));
        std::thread::sleep(Duration::from_millis(1_100));
        assert!(
            !escaped_marker.exists(),
            "the timed-out command's descendant must not outlive the boundary"
        );
    }

    #[test]
    fn pull_reports_failures_after_attempting_remaining_pointers() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let missing_file = repo.join("asset/missing.bin");
        let available_file = repo.join("asset/available.bin");
        let missing_object = object_for_bytes(b"missing fetched object");
        let available_bytes = b"available fetched object";
        let available_object = object_for_bytes(available_bytes);
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        write_file(
            &missing_file,
            LfsPointer::new(missing_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &available_file,
            LfsPointer::new(available_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        run_git(
            &repo,
            &[
                "add",
                ".gitattributes",
                "asset/available.bin",
                "asset/missing.bin",
            ],
        );
        write_git_lfs_source_object(&repo, &available_object, available_bytes);
        let mut output = Vec::new();

        let error = run_pull_from_dir(
            PullCommand {
                cache_root: Some(cache_root),
            },
            &repo,
            &mut output,
            |_| Ok(()),
        )
        .expect_err("one missing fetched object should fail pull");

        assert!(matches!(
            error,
            CliError::PullFailed {
                failures: 1,
                path,
                ..
            } if path == dunce::canonicalize(&missing_file)
                .unwrap_or_else(|_| missing_file.clone())
        ));
        assert_eq!(
            fs::read(&available_file).expect("available file should be hydrated"),
            available_bytes
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("tracked paths: 2"));
        assert!(rendered.contains("pointers: 2"));
        assert!(rendered.contains("failed"));
        assert!(rendered.contains("missing.bin"));
        assert!(rendered.contains("pulled"));
        assert!(rendered.contains("available.bin"));
    }

    #[test]
    fn current_checkout_pointer_scan_uses_lfs_tracked_files_only() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let tracked_object = object_for_bytes(b"tracked pointer object");
        let untracked_object = object_for_bytes(b"untracked pointer object");
        let ordinary_object = object_for_bytes(b"ordinary tracked pointer-shaped object");
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/tracked.bin"),
            LfsPointer::new(tracked_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("asset/untracked.bin"),
            LfsPointer::new(untracked_object)
                .to_pointer_file()
                .as_bytes(),
        );
        write_file(
            &repo.join("docs/pointer-example.txt"),
            LfsPointer::new(ordinary_object)
                .to_pointer_file()
                .as_bytes(),
        );
        run_git(
            &repo,
            &[
                "add",
                ".gitattributes",
                "asset/tracked.bin",
                "docs/pointer-example.txt",
            ],
        );

        let pointers = current_checkout_lfs_pointer_files(&repo)
            .expect("pointer scan should inspect tracked files");

        assert_eq!(pointers.len(), 1);
        assert_eq!(pointers[0].object, tracked_object);
        assert_eq!(pointers[0].path, repo.join("asset/tracked.bin"));
    }

    #[test]
    fn current_checkout_pointer_scan_reports_tracked_and_pointer_counts() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"tracked pointer object");
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        write_file(
            &repo.join("asset/pointer.bin"),
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        write_file(&repo.join("asset/empty.bin"), b"");
        write_file(&repo.join("asset/hydrated.bin"), b"already hydrated bytes");
        run_git(
            &repo,
            &[
                "add",
                ".gitattributes",
                "asset/pointer.bin",
                "asset/empty.bin",
                "asset/hydrated.bin",
            ],
        );

        let scan = current_checkout_lfs_pointer_scan(&repo)
            .expect("pointer scan should inspect tracked files");

        assert_eq!(scan.tracked_path_count, 3);
        assert_eq!(scan.pointer_files.len(), 1);
        assert_eq!(scan.pointer_files[0].object, object);
    }

    #[cfg(unix)]
    #[test]
    fn current_checkout_pointer_scan_accepts_non_utf8_tracked_paths() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let object = object_for_bytes(b"non UTF-8 path object");
        let non_utf8_name = OsString::from_vec(b"nonutf8-\xff.bin".to_vec());
        let worktree_file = repo.join("asset").join(PathBuf::from(non_utf8_name));
        write_file(&repo.join(".gitattributes"), b"asset/*.bin filter=lfs\n");
        fs::create_dir_all(worktree_file.parent().expect("path should have parent"))
            .expect("non-UTF-8 path parent should be created");
        if fs::write(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        )
        .is_err()
        {
            return;
        }
        run_git(&repo, &["add", "-A"]);

        let pointers = current_checkout_lfs_pointer_files(&repo)
            .expect("pointer scan should accept non-UTF-8 paths");

        assert_eq!(pointers.len(), 1);
        assert_eq!(pointers[0].object, object);
        assert_eq!(pointers[0].path, worktree_file);
    }

    #[test]
    fn hydrate_replaces_pointer_file_with_verified_cache_object() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"cached model bytes";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(&layout.object_path(&object), bytes);
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        let mut output = Vec::new();

        run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("hydrate should replace pointer with cache bytes");

        assert_eq!(
            fs::read(&worktree_file).expect("hydrated file should be readable"),
            bytes
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("hydrated"));
        #[cfg(target_os = "macos")]
        {
            let file_system = rustix::fs::statfs(temp.path())
                .expect("test filesystem should be inspectable")
                .f_fstypename;
            let is_apfs = file_system
                .iter()
                .copied()
                .take_while(|byte| *byte != 0)
                .map(|byte| byte as u8)
                .eq(b"apfs".iter().copied());
            if is_apfs {
                assert!(rendered.contains("copy-on-write-cloned"));
            } else {
                assert!(rendered.contains("copied"));
            }
        }
        #[cfg(not(target_os = "macos"))]
        assert!(rendered.contains("copied"));
        assert!(
            rendered.contains(
                &dunce::canonicalize(&worktree_file)
                    .expect("worktree file should canonicalize")
                    .display()
                    .to_string()
            )
        );
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn dehydrate_caches_clean_file_and_writes_pointer() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"hydrated model bytes";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        stage_lfs_pointer(&repo, "asset/model.bin", &object);
        write_file(&worktree_file, bytes);
        let mut output = Vec::new();

        run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("dehydrate should cache bytes and write pointer");

        assert_eq!(
            fs::read(layout.object_path(&object)).expect("cached file should be readable"),
            bytes
        );
        assert_eq!(
            fs::read_to_string(&worktree_file).expect("pointer file should be readable"),
            LfsPointer::new(object.clone()).to_pointer_file()
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("dehydrated"));
        assert!(rendered.contains("cached-and-replaced-with-pointer"));
        assert!(
            rendered.contains(
                &dunce::canonicalize(&worktree_file)
                    .expect("worktree file should canonicalize")
                    .display()
                    .to_string()
            )
        );
        assert!(rendered.contains(object.oid.as_hex()));

        let mut gc_output = Vec::new();
        run_gc_from_dir(
            GcCommand {
                cache_root: Some(layout.root().to_path_buf()),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut gc_output,
        )
        .expect("gc should retain the dehydrated pointer's cached bytes");
        assert!(layout.object_path(&object).exists());
    }

    #[test]
    fn dehydrate_accepts_existing_pointer_as_idempotent() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let object = object_for_bytes(b"already dehydrated bytes");
        let pointer = LfsPointer::new(object.clone()).to_pointer_file();
        stage_lfs_pointer(&repo, "asset/model.bin", &object);
        let mut output = Vec::new();

        run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("existing pointer should be accepted");

        assert_eq!(
            fs::read_to_string(&worktree_file).expect("pointer file should be readable"),
            pointer
        );
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("already-dehydrated"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn dehydrate_rejects_dirty_lfs_content_without_caching_it() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let clean_object = object_for_bytes(b"clean hydrated model bytes");
        let dirty_bytes = b"dirty edit that must not be preserved as LFS content";
        let dirty_object = object_for_bytes(dirty_bytes);
        stage_lfs_pointer(&repo, "asset/model.bin", &clean_object);
        write_file(&worktree_file, dirty_bytes);
        let mut output = Vec::new();

        let error = run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root.clone()),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("dirty LFS content must not be dehydrated");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::IntegrityMismatch { .. }
            }
        ));
        assert_eq!(
            fs::read(&worktree_file).expect("dirty file should remain readable"),
            dirty_bytes
        );
        assert!(
            !LocalCacheLayout::new(cache_root)
                .object_path(&dirty_object)
                .exists()
        );
        assert!(output.is_empty());
    }

    #[test]
    fn dehydrate_rejects_untracked_and_non_lfs_paths() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        write_file(&repo.join("untracked.bin"), b"untracked bytes");
        write_file(&repo.join("tracked.txt"), b"ordinary tracked bytes");
        run_git(&repo, &["add", "tracked.txt"]);

        for path in ["untracked.bin", "tracked.txt"] {
            let mut output = Vec::new();
            let error = run_dehydrate_from_dir(
                DehydrateCommand {
                    cache_root: Some(cache_root.clone()),
                    paths: vec![PathBuf::from(path)],
                },
                &repo,
                &mut output,
            )
            .expect_err("only tracked filter=lfs paths may be dehydrated");

            assert!(matches!(error, CliError::InvalidArguments { .. }));
            assert!(output.is_empty());
        }

        assert!(!cache_root.join("objects").exists());
    }

    #[test]
    fn dehydrate_rejects_paths_outside_the_current_worktree() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside.bin");
        init_git_repo_with_origin(&repo);
        write_file(&outside, b"outside bytes");
        for path in [outside.clone(), PathBuf::from("../outside.bin")] {
            let mut output = Vec::new();
            let error = run_dehydrate_from_dir(
                DehydrateCommand {
                    cache_root: Some(cache_root.clone()),
                    paths: vec![path],
                },
                &repo,
                &mut output,
            )
            .expect_err("outside paths must not be dehydrated");

            assert!(matches!(error, CliError::InvalidArguments { .. }));
            assert!(output.is_empty());
        }
        assert_eq!(
            fs::read(&outside).expect("outside file should remain readable"),
            b"outside bytes"
        );
        assert!(!cache_root.join("objects").exists());
    }

    #[test]
    fn hydrate_rejects_paths_outside_the_current_worktree() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let outside = temp.path().join("outside.bin");
        init_git_repo_with_origin(&repo);
        let bytes = b"outside cached bytes";
        let object = object_for_bytes(bytes);
        let pointer = LfsPointer::new(object.clone()).to_pointer_file();
        write_file(&outside, pointer.as_bytes());
        write_file(
            &LocalCacheLayout::new(&cache_root).object_path(&object),
            bytes,
        );
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![outside.clone()],
            },
            &repo,
            &mut output,
        )
        .expect_err("outside paths must not be hydrated");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
        assert_eq!(
            fs::read_to_string(&outside).expect("outside pointer should remain readable"),
            pointer
        );
        assert!(output.is_empty());
    }

    #[test]
    fn dehydrate_republishes_cache_bytes_for_real_git_lfs_push() {
        require_git_lfs();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let remote = temp.path().join("remote.git");
        init_git_repo_with_origin(&repo);
        run_git(&repo, &["config", "user.name", "LFS Cloud Test"]);
        run_git(&repo, &["config", "user.email", "lfscloud@example.invalid"]);
        run_git(&repo, &["lfs", "install", "--local"]);
        run_git(temp.path(), &["init", "--bare", "remote.git"]);
        write_file(
            &repo.join(".gitattributes"),
            b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
        );
        let worktree_file = repo.join("asset/model.bin");
        let bytes = b"object restored to Git LFS media before push";
        let object = object_for_bytes(bytes);
        write_file(&worktree_file, bytes);
        run_git(&repo, &["add", ".gitattributes", "asset/model.bin"]);
        run_git(&repo, &["commit", "-m", "Add LFS object"]);
        let local_media = repo.join(".git").join("lfs").join("objects");
        fs::remove_dir_all(&local_media).expect("local Git LFS media should be removable");
        let mut output = Vec::new();

        run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect("dehydrate should restore Git LFS media");
        run_git(
            &repo,
            &[
                "remote",
                "set-url",
                "origin",
                remote
                    .to_str()
                    .expect("temporary remote path should be UTF-8"),
            ],
        );
        run_git(&repo, &["lfs", "push", "origin", "HEAD"]);

        let oid = object.oid.as_hex();
        let remote_object = remote
            .join("lfs")
            .join("objects")
            .join(&oid[..2])
            .join(&oid[2..4])
            .join(oid);
        assert_eq!(
            fs::read(remote_object).expect("pushed Git LFS object should be readable"),
            bytes
        );
    }

    #[test]
    fn hydrate_rejects_non_pointer_worktree_content_with_local_cache_error() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        write_file(&worktree_file, b"plain worktree bytes");
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("non-pointer content should not hydrate");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::PointerParse { path, .. }
            } if path == dunce::canonicalize(&worktree_file).unwrap_or(worktree_file)
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn hydrate_reports_missing_cache_object_as_local_cache_error() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let worktree_file = repo.join("asset/model.bin");
        let object = object_for_bytes(b"not cached yet");
        write_file(
            &worktree_file,
            LfsPointer::new(object.clone()).to_pointer_file().as_bytes(),
        );
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("missing cache object should fail hydration");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::MissingCacheObject { oid, .. }
            } if oid == object.oid
        ));
        assert!(output.is_empty());
    }

    #[test]
    fn dehydrate_rejects_non_file_path_before_cache_mutation() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        fs::create_dir_all(repo.join("asset/model.bin")).expect("test directory should be created");
        let mut output = Vec::new();

        let error = run_dehydrate_from_dir(
            DehydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![PathBuf::from("asset/model.bin")],
            },
            &repo,
            &mut output,
        )
        .expect_err("directory path should not dehydrate");

        assert!(matches!(error, CliError::InvalidArguments { .. }));
        assert!(output.is_empty());
    }

    #[test]
    fn hydrate_stops_when_one_of_multiple_paths_fails() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let missing_cache_file = repo.join("asset/missing.bin");
        let cached_file = repo.join("asset/cached.bin");
        let missing_object = object_for_bytes(b"missing cache object");
        let cached_object = object_for_bytes(b"cached object");
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(
            &missing_cache_file,
            LfsPointer::new(missing_object.clone())
                .to_pointer_file()
                .as_bytes(),
        );
        let cached_pointer = LfsPointer::new(cached_object.clone()).to_pointer_file();
        write_file(&cached_file, cached_pointer.as_bytes());
        write_file(&layout.object_path(&cached_object), b"cached object");
        let mut output = Vec::new();

        let error = run_hydrate_from_dir(
            HydrateCommand {
                cache_root: Some(cache_root),
                paths: vec![
                    PathBuf::from("asset/missing.bin"),
                    PathBuf::from("asset/cached.bin"),
                ],
            },
            &repo,
            &mut output,
        )
        .expect_err("first missing cache object should stop hydration");

        assert!(matches!(
            error,
            CliError::LocalCache {
                source: LocalCacheError::MissingCacheObject { oid, .. }
            } if oid == missing_object.oid
        ));
        assert!(output.is_empty());
        assert_eq!(
            fs::read_to_string(&cached_file).expect("second path should remain readable"),
            cached_pointer
        );
    }

    #[test]
    fn gc_removes_unreferenced_cache_objects_and_reports_summary() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let keep_bytes = b"gc referenced object";
        let remove_bytes = b"gc unreferenced object";
        let keep_object = object_for_bytes(keep_bytes);
        let remove_object = object_for_bytes(remove_bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(&layout.object_path(&keep_object), keep_bytes);
        write_file(&layout.object_path(&remove_object), remove_bytes);
        stage_lfs_pointer(&repo, "asset/model.bin", &keep_object);
        let mut output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut output,
        )
        .expect("gc should remove unreferenced cache objects");

        assert!(layout.object_path(&keep_object).exists());
        assert!(!layout.object_path(&remove_object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("lfscloud gc"));
        assert!(rendered.contains("worktrees: 1 active, 0 unavailable, 0 pruned"));
        assert!(rendered.contains(
            "objects: 1 retained, 0 protected, 1 removed, 0 cache paths skipped, 0 worktree pointers skipped"
        ));
        assert!(rendered.contains(remove_object.oid.as_hex()));
    }

    #[test]
    fn gc_dry_run_reports_without_removing_cache_objects() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        init_git_repo_with_origin(&repo);
        let bytes = b"gc dry-run unreferenced object";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        write_file(&layout.object_path(&object), bytes);
        let mut output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: true,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut output,
        )
        .expect("gc dry-run should report unreferenced cache objects");

        assert!(layout.object_path(&object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains(
            "objects: 0 retained, 0 protected, 1 would remove, 0 cache paths skipped, 0 worktree pointers skipped"
        ));
        assert!(rendered.contains("would remove"));
        assert!(rendered.contains(object.oid.as_hex()));
    }

    #[test]
    fn gc_requires_explicit_pruning_before_collecting_with_unavailable_worktrees() {
        require_git();

        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let repo = temp.path().join("repo");
        let missing_repo = temp.path().join("disconnected-repo");
        init_git_repo_with_origin(&repo);
        let bytes = b"possibly referenced by disconnected worktree";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        let missing_registration = LocalCacheWorktreeRegistration::new(
            "github-main:owner/disconnected",
            &missing_repo,
            missing_repo.join(".git"),
        )
        .expect("missing worktree registration should validate");
        layout
            .register_worktree(missing_registration)
            .expect("missing worktree should register");
        write_file(&layout.object_path(&object), bytes);
        let mut protected_output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root.clone()),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &repo,
            &mut protected_output,
        )
        .expect("ordinary gc should preserve objects for unavailable worktrees");

        assert!(layout.object_path(&object).exists());
        let rendered = String::from_utf8(protected_output).expect("output should be UTF-8");
        assert!(rendered.contains("1 unavailable, 0 pruned"));
        assert!(rendered.contains("1 protected, 0 removed"));
        assert!(rendered.contains("unavailable worktree"));
        assert!(rendered.contains("protected while worktree unavailable"));

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: false,
                prune_unavailable_worktrees: true,
            },
            &repo,
            &mut Vec::new(),
        )
        .expect("explicit pruning should permit collection");

        assert!(!layout.object_path(&object).exists());
        assert_eq!(
            layout
                .load_worktree_registry()
                .expect("registry should reload")
                .worktrees()
                .len(),
            1
        );
    }

    #[test]
    fn gc_runs_outside_git_worktree() {
        let temp = TempDir::new().expect("temporary directory should be created");
        let cache_root = temp.path().join("cache");
        let start_dir = temp.path().join("outside-repo");
        let bytes = b"gc outside repo object";
        let object = object_for_bytes(bytes);
        let layout = LocalCacheLayout::new(&cache_root);
        fs::create_dir_all(&start_dir).expect("start directory should be created");
        write_file(&layout.object_path(&object), bytes);
        let mut output = Vec::new();

        run_gc_from_dir(
            GcCommand {
                cache_root: Some(cache_root),
                dry_run: false,
                prune_unavailable_worktrees: false,
            },
            &start_dir,
            &mut output,
        )
        .expect("gc should run without a current Git worktree");

        assert!(!layout.object_path(&object).exists());
        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("worktrees: 0 active, 0 unavailable, 0 pruned"));
        assert!(rendered.contains(
            "objects: 0 retained, 0 protected, 1 removed, 0 cache paths skipped, 0 worktree pointers skipped"
        ));
    }

    #[test]
    fn gc_ignores_only_non_worktree_git_discovery_failures() {
        let outside_worktree = CliError::ExternalCommand {
            command: "git rev-parse --show-toplevel".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: SanitizedMessage::new(
                "fatal: not a git repository (or any of the parent directories): .git",
            ),
        };
        assert!(is_git_worktree_discovery_error(&outside_worktree));

        let bare_repository = CliError::ExternalCommand {
            command: "git rev-parse --show-toplevel".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: SanitizedMessage::new("fatal: this operation must be run in a work tree"),
        };
        assert!(is_git_worktree_discovery_error(&bare_repository));

        let unsafe_repository = CliError::ExternalCommand {
            command: "git rev-parse --show-toplevel".to_owned(),
            status: "exit status: 128".to_owned(),
            stderr: SanitizedMessage::new(
                "fatal: detected dubious ownership in repository at '/tmp/repo'",
            ),
        };
        assert!(!is_git_worktree_discovery_error(&unsafe_repository));

        let start_failure = CliError::Io {
            context: "failed to start git rev-parse --show-toplevel".to_owned(),
            source: io::Error::new(io::ErrorKind::NotFound, "git"),
        };
        assert!(!is_git_worktree_discovery_error(&start_failure));
    }
}
