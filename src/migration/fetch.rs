// This file is included by `mod.rs` so the migration API remains in one module.

/// Ref scope used when fetching source Git LFS objects for migration.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum MigrationFetchMode {
    /// Fetch objects required by the currently checked-out ref.
    CurrentCheckout,
    /// Fetch objects required by the supplied branch, tag, or ref names.
    SelectedRefs {
        /// Refs to pass to `git lfs fetch` after validation.
        refs: Vec<String>,
    },
    /// Fetch all objects reachable from fetched local refs.
    AllFetchedRefs,
    /// Fetch only the supplied object identities from an explicit LFS endpoint.
    ObjectIds,
}

impl MigrationFetchMode {
    /// Builds a selected-ref fetch mode from caller-supplied ref names.
    ///
    /// Validation happens when the fetch command is prepared so callers can
    /// keep raw CLI arguments in one place until migration execution begins.
    #[must_use]
    pub fn selected_refs<I, S>(refs: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self::SelectedRefs {
            refs: refs.into_iter().map(Into::into).collect(),
        }
    }

    /// Returns the selected ref names when this mode targets explicit refs.
    #[must_use]
    pub fn selected_ref_names(&self) -> Option<&[String]> {
        match self {
            Self::SelectedRefs { refs } => Some(refs),
            Self::CurrentCheckout | Self::AllFetchedRefs | Self::ObjectIds => None,
        }
    }
}

/// Result of fetching missing source Git LFS objects into local media storage.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct MigrationSourceFetch {
    /// Git worktree root where the source fetch ran.
    pub worktree_root: PathBuf,
    /// Explicit Git remote used for source-provider fetches.
    pub source_remote: String,
    /// Ref scope used for the source fetch.
    pub mode: MigrationFetchMode,
    /// Safe display form of the `git lfs fetch` command that ran.
    ///
    /// This is a human-readable diagnostic string, not a shell script. Arguments
    /// that need quoting are single-quoted, and non-UTF-8 argument bytes are
    /// rendered lossily because Git accepts platform-native paths and ref names.
    ///
    /// `None` means every requested object was already locally available, so no
    /// source-provider fetch was needed.
    pub command: Option<String>,
    /// Availability snapshot before fetching from the source provider.
    pub before: LocalMigrationObjectAvailability,
    /// Availability snapshot after fetching from the source provider.
    pub after: LocalMigrationObjectAvailability,
    /// Objects that were unavailable before the fetch and available afterward.
    pub fetched_objects: Vec<LfsObject>,
    /// Objects that still have no verified local bytes after the fetch.
    pub unavailable_objects: Vec<LfsObject>,
}
/// Fetches missing source Git LFS objects without updating worktree files.
///
/// The helper first checks repository-local Git LFS media storage and the
/// optional shared LFS Cloud cache. If any requested object lacks verified local
/// bytes, it runs `git lfs fetch` for the requested ref scope and then checks
/// availability again. Git LFS fetch populates local media storage and does not
/// smudge or checkout files, so callers can use this before upload planning
/// without mutating worktree contents.
///
/// # Errors
///
/// Returns [`MigrationError`] when the start directory is not a Git worktree,
/// selected refs use invalid revision syntax, `git lfs fetch` cannot be
/// started, or the source provider fetch exits unsuccessfully.
pub fn fetch_missing_migration_objects<I, O>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    mode: MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetch>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
{
    fetch_missing_migration_objects_from_remote(
        start_dir,
        objects,
        shared_cache,
        DEFAULT_REMOTE_NAME,
        mode,
    )
}

/// Fetches missing source Git LFS objects from an explicit Git remote.
///
/// This is the remote-selecting variant of [`fetch_missing_migration_objects`].
/// The selected remote is included in every fetch scope so later execution
/// cannot silently follow the current branch's configured remote.
///
/// # Errors
///
/// Returns [`MigrationError`] when the source remote is invalid, the start
/// directory is not a Git worktree, selected refs use invalid revision syntax,
/// `git lfs fetch` cannot be started, or the source fetch exits unsuccessfully.
pub fn fetch_missing_migration_objects_from_remote<I, O>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    source_remote: impl AsRef<str>,
    mode: MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetch>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
{
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;
    fetch_missing_migration_objects_with_runner(
        start_dir,
        objects,
        shared_cache,
        &source_remote,
        None,
        mode,
        run_git_lfs_fetch_command,
    )
}

/// Fetches missing migration objects from an explicit remote and legacy LFS endpoint.
///
/// This is used after a repository has committed an LFS Cloud `lfs.url`: the
/// command-scoped override sends only this migration fetch to the legacy
/// endpoint recorded as `remote.<name>.lfsurl` in `.lfsconfig`.
///
/// # Errors
///
/// Returns [`MigrationError`] when the endpoint is unsafe, the source remote or
/// ref scope is invalid, or Git LFS cannot fetch the requested objects.
pub fn fetch_missing_migration_objects_from_remote_at_endpoint<I, O>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    source_remote: impl AsRef<str>,
    source_endpoint: impl AsRef<str>,
    allow_insecure_http: bool,
    mode: MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetch>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
{
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;
    let source_endpoint = validated_migration_source_endpoint(
        source_endpoint.as_ref(),
        allow_insecure_http,
    )?;
    fetch_missing_migration_objects_with_runner(
        start_dir,
        objects,
        shared_cache,
        &source_remote,
        Some(&source_endpoint),
        mode,
        run_git_lfs_fetch_command,
    )
}

/// Refreshes branches, tags, and remote-tracking refs from the selected source remote.
///
/// Full-history migration inventory is only as complete as the Git refs stored
/// locally. Execution calls this before its final all-ref scan so a stale clone
/// cannot silently omit a branch or tag that still references LFS objects.
/// Interactive prompting is disabled, output is bounded, and the owned process
/// tree is terminated after the same six-hour deadline used for source LFS
/// transfers.
///
/// # Errors
///
/// Returns [`MigrationError`] when the remote name is invalid, the Git fetch
/// cannot start, times out, or exits unsuccessfully.
pub fn fetch_migration_git_refs(
    start_dir: impl AsRef<Path>,
    source_remote: impl AsRef<str>,
) -> MigrationResult<()> {
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;
    let remote_tracking_refspec = format!("+refs/heads/*:refs/remotes/{source_remote}/*");
    let args = vec![
        OsString::from("fetch"),
        OsString::from("--prune"),
        OsString::from("--tags"),
        OsString::from("--no-recurse-submodules"),
        OsString::from("--"),
        OsString::from(&source_remote),
        OsString::from(remote_tracking_refspec),
    ];
    let display = display_git_command(&args);
    let mut process = Command::new("git");
    process
        .args(&args)
        .current_dir(start_dir.as_ref())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GCM_INTERACTIVE", "Never")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_process_tree(&mut process);
    let mut child = process.spawn().map_err(|source| MigrationError::Io {
        context: format!("failed to start {display}"),
        source,
    })?;
    let (status, stderr) =
        wait_for_git_command(&mut child, &display, MIGRATION_SOURCE_FETCH_TIMEOUT)?;
    if status.success() {
        Ok(())
    } else {
        Err(command_error(&display, status, &stderr))
    }
}
fn fetch_missing_migration_objects_with_runner<I, O, F>(
    start_dir: impl AsRef<Path>,
    objects: I,
    shared_cache: Option<&LocalCacheLayout>,
    source_remote: &str,
    source_endpoint: Option<&str>,
    mode: MigrationFetchMode,
    mut runner: F,
) -> MigrationResult<MigrationSourceFetch>
where
    I: IntoIterator<Item = O>,
    O: Borrow<LfsObject>,
    F: FnMut(&Path, &MigrationSourceFetchCommand) -> MigrationResult<()>,
{
    let start_dir = start_dir.as_ref();
    let before = check_local_migration_objects(start_dir, objects, shared_cache)?;
    let worktree_root = before.worktree_root.clone();
    let mut command = None;

    if before.unavailable_objects().is_empty() {
        let after = before.clone();
        return Ok(MigrationSourceFetch {
            worktree_root,
            source_remote: source_remote.to_owned(),
            mode,
            command,
            fetched_objects: Vec::new(),
            unavailable_objects: Vec::new(),
            before,
            after,
        });
    }

    let fetch_commands = migration_source_fetch_commands(
        source_remote,
        source_endpoint,
        &mode,
        before.unavailable_objects(),
    )?;
    for fetch_command in &fetch_commands {
        runner(&worktree_root, fetch_command)?;
    }
    command = match fetch_commands.as_slice() {
        [] => None,
        [fetch_command] => Some(fetch_command.display.clone()),
        [first, ..] => Some(format!(
            "{} (repeated for {} target-missing objects)",
            first.display,
            fetch_commands.len()
        )),
    };

    let after = check_local_migration_objects(
        &worktree_root,
        before.objects.iter().map(|object| &object.object),
        shared_cache,
    )?;
    let fetched_objects = fetched_migration_objects(&before, &after);
    let unavailable_objects = unavailable_migration_objects(&after);

    Ok(MigrationSourceFetch {
        worktree_root,
        source_remote: source_remote.to_owned(),
        mode,
        command,
        before,
        after,
        fetched_objects,
        unavailable_objects,
    })
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct MigrationSourceFetchCommand {
    args: Vec<OsString>,
    display: String,
    stdin: Option<Vec<u8>>,
}

fn migration_source_fetch_commands(
    source_remote: &str,
    source_endpoint: Option<&str>,
    mode: &MigrationFetchMode,
    unavailable_objects: Vec<&LocalMigrationObject>,
) -> MigrationResult<Vec<MigrationSourceFetchCommand>> {
    if *mode != MigrationFetchMode::ObjectIds {
        return migration_source_fetch_command(source_remote, source_endpoint, mode)
            .map(|command| vec![command]);
    }

    validate_source_remote_name(source_remote)?;
    let source_endpoint = source_endpoint.ok_or_else(|| MigrationError::InvalidInput {
        message: SanitizedMessage::new(
            "object-ID migration fetch requires an explicit source LFS endpoint",
        ),
    })?;
    let mut args = vec![
        OsString::from("-c"),
        OsString::from(format!("lfs.url={source_endpoint}")),
        OsString::from("-c"),
        OsString::from("lfs.fetchinclude="),
        OsString::from("-c"),
        OsString::from("lfs.fetchexclude="),
    ];
    args.extend([OsString::from("lfs"), OsString::from("smudge")]);
    let display = display_git_command(&args);

    Ok(unavailable_objects
        .into_iter()
        .map(|local| MigrationSourceFetchCommand {
            args: args.clone(),
            display: display.clone(),
            stdin: Some(LfsPointer::new(local.object.clone()).to_pointer_file().into_bytes()),
        })
        .collect())
}

fn migration_source_fetch_command(
    source_remote: &str,
    source_endpoint: Option<&str>,
    mode: &MigrationFetchMode,
) -> MigrationResult<MigrationSourceFetchCommand> {
    let source_remote = validate_source_remote_name(source_remote)?;
    // Migration fetch scope must be determined only by `mode`. Repository or
    // user configuration can otherwise turn every fetch into `--recent`,
    // expanding downloads beyond the reviewed migration inventory and making
    // `--all` fail because Git LFS forbids combining it with recent mode.
    let mut args = vec![
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentalways=false"),
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentrefsdays=0"),
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentremoterefs=false"),
        OsString::from("-c"),
        OsString::from("lfs.fetchrecentcommitsdays=0"),
    ];
    if let Some(source_endpoint) = source_endpoint {
        args.push(OsString::from("-c"));
        args.push(OsString::from(format!("lfs.url={source_endpoint}")));
    }
    args.extend([OsString::from("lfs"), OsString::from("fetch")]);

    match mode {
        MigrationFetchMode::CurrentCheckout => {
            args.push(OsString::from("--include="));
            args.push(OsString::from("--exclude="));
            args.push(OsString::from(&source_remote));
        }
        MigrationFetchMode::SelectedRefs { refs } => {
            if refs.is_empty() {
                return Err(MigrationError::InvalidInput {
                    message: SanitizedMessage::new(
                        "selected-ref migration fetch requires at least one ref",
                    ),
                });
            }

            for ref_name in refs {
                validate_history_ref_name(ref_name)?;
            }

            args.push(OsString::from("--include="));
            args.push(OsString::from("--exclude="));
            args.push(OsString::from(&source_remote));
            args.extend(
                refs.iter()
                    .map(|ref_name| OsString::from(ref_name.as_str())),
            );
        }
        MigrationFetchMode::AllFetchedRefs => {
            args.push(OsString::from("--all"));
            args.push(OsString::from(&source_remote));
        }
        MigrationFetchMode::ObjectIds => {
            return Err(MigrationError::InvalidInput {
                message: SanitizedMessage::new(
                    "object-ID migration fetch requires explicit object metadata",
                ),
            });
        }
    }

    let display = display_git_command(&args);
    Ok(MigrationSourceFetchCommand {
        args,
        display,
        stdin: None,
    })
}

pub(crate) fn validated_migration_source_endpoint(
    source_endpoint: &str,
    allow_insecure_http: bool,
) -> MigrationResult<String> {
    if source_endpoint.is_empty()
        || source_endpoint
            .chars()
            .any(|character| character.is_whitespace() || character.is_control() || character == '\\')
    {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new("legacy LFS endpoint contains unsafe characters"),
        });
    }
    let parsed = Url::parse(source_endpoint).map_err(|_| MigrationError::InvalidInput {
        message: SanitizedMessage::new("legacy LFS endpoint must be an absolute HTTP(S) URL"),
    })?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new("legacy LFS endpoint must be an absolute HTTP(S) URL"),
        });
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new("legacy LFS endpoint must not contain credentials"),
        });
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(
                "legacy LFS endpoint must not contain a query string or fragment",
            ),
        });
    }
    if !allow_insecure_http && !crate::http_transport::uses_protected_http_transport(&parsed) {
        return Err(MigrationError::InvalidInput {
            message: SanitizedMessage::new(
                "legacy LFS endpoint must use HTTPS unless it targets an exact loopback IP",
            ),
        });
    }
    Ok(parsed.to_string().trim_end_matches('/').to_owned())
}

fn run_git_lfs_fetch_command(
    worktree_root: &Path,
    command: &MigrationSourceFetchCommand,
) -> MigrationResult<()> {
    let mut process = Command::new("git");
    process
        .args(&command.args)
        .current_dir(worktree_root)
        .env_remove("GIT_LFS_SKIP_SMUDGE")
        .stdin(if command.stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    configure_process_tree(&mut process);
    let mut child = process.spawn().map_err(|source| MigrationError::Io {
        context: "failed to start git lfs fetch".to_owned(),
        source,
    })?;
    if let Some(input) = &command.stdin {
        let write_result = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "Git LFS stdin was not piped"))
            .and_then(|mut stdin| stdin.write_all(input));
        if let Err(source) = write_result {
            let _ = terminate_process_tree(&mut child, &command.display);
            return Err(MigrationError::Io {
                context: "failed to write Git LFS pointer to smudge stdin".to_owned(),
                source,
            });
        }
    }
    let (status, stderr) =
        wait_for_git_command(&mut child, &command.display, MIGRATION_SOURCE_FETCH_TIMEOUT)?;

    if status.success() {
        Ok(())
    } else {
        Err(command_error(&command.display, status, &stderr))
    }
}

fn wait_for_git_command(
    child: &mut Child,
    command: &str,
    timeout: Duration,
) -> MigrationResult<(ExitStatus, Vec<u8>)> {
    if child.stderr.is_none() {
        terminate_process_tree(child, command)
            .map_err(|error| child_process_migration_error(error, command))?;
        return Err(MigrationError::Io {
            context: format!("failed to capture stderr from {command}"),
            source: io::Error::new(
                io::ErrorKind::BrokenPipe,
                format!("{command} stderr was not piped"),
            ),
        });
    }
    let output = wait_for_child(
        child,
        command,
        ChildProcessOptions {
            timeout: Some(timeout),
            stdout: PipeCapture::Truncate { limit: 0 },
            stderr: PipeCapture::Truncate {
                limit: MAX_MIGRATION_GIT_OUTPUT_BYTES + 1,
            },
            inherited_pipe_is_error: false,
        },
    )
    .map_err(|error| child_process_migration_error(error, command))?;

    Ok((output.status, output.stderr))
}

fn fetched_migration_objects(
    before: &LocalMigrationObjectAvailability,
    after: &LocalMigrationObjectAvailability,
) -> Vec<LfsObject> {
    before
        .objects
        .iter()
        .zip(after.objects.iter())
        .filter(|(before, after)| {
            before.object == after.object && !before.is_available() && after.is_available()
        })
        .map(|(before, _)| before.object.clone())
        .collect()
}

fn unavailable_migration_objects(
    availability: &LocalMigrationObjectAvailability,
) -> Vec<LfsObject> {
    availability
        .objects
        .iter()
        .filter(|object| !object.is_available())
        .map(|object| object.object.clone())
        .collect()
}

fn display_git_command(args: &[OsString]) -> String {
    std::iter::once(OsStr::new("git"))
        .chain(args.iter().map(OsString::as_os_str))
        .map(display_git_command_arg)
        .collect::<Vec<_>>()
        .join(" ")
}

fn display_git_command_arg(arg: &OsStr) -> String {
    let arg = arg.to_string_lossy();
    if arg.is_empty() {
        return "''".to_owned();
    }

    if arg
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./:=@,".contains(character))
    {
        return arg.into_owned();
    }

    format!("'{}'", arg.replace('\'', "'\\''"))
}


#[cfg(test)]
mod fetch_tests {
    use super::test_support::*;
    use super::validated_migration_source_endpoint;

    const PROCESS_TREE_HELPER_TEST: &str = "migration::fetch_tests::migration_process_tree_helper";
    const PROCESS_TREE_DESCENDANT_TEST: &str =
        "migration::fetch_tests::migration_process_tree_descendant";
    const PROCESS_TREE_MARKER_ENV: &str = "LFS_CLOUD_MIGRATION_TEST_MARKER";

    #[test]
    fn migration_ref_fetch_refreshes_source_branches_and_tags() {
        let remote_root = tempfile::tempdir().expect("temporary remote root should be created");
        let remote_path = remote_root.path().join("source.git");
        let remote = remote_path.to_string_lossy().into_owned();
        let bare_init = Command::new("git")
            .args([
                "init",
                "--bare",
                "--initial-branch",
                "main",
                remote.as_str(),
            ])
            .output()
            .expect("bare remote initialization should start");
        assert!(
            bare_init.status.success(),
            "bare remote initialization failed: {}",
            String::from_utf8_lossy(&bare_init.stderr)
        );

        let source = TempRepo::new();
        source.write_file("README.md", "initial branch\n");
        source.commit_all("add main branch");
        source.git(["remote", "add", "origin", remote.as_str()]);
        source.git(["push", "-u", "origin", "main"]);

        let clone_path = remote_root.path().join("clone");
        let clone = clone_path.to_string_lossy().into_owned();
        let clone_output = Command::new("git")
            .args(["clone", "--no-local", remote.as_str(), clone.as_str()])
            .output()
            .expect("source clone should start");
        assert!(
            clone_output.status.success(),
            "source clone failed: {}",
            String::from_utf8_lossy(&clone_output.stderr)
        );
        let narrow_fetch = Command::new("git")
            .args([
                "config",
                "remote.origin.fetch",
                "+refs/heads/main:refs/remotes/origin/main",
            ])
            .current_dir(&clone_path)
            .output()
            .expect("narrow fetch configuration should start");
        assert!(narrow_fetch.status.success());

        source.git(["checkout", "-b", "feature/history"]);
        source.write_file("historical.txt", "branch-only history\n");
        source.commit_all("add historical branch");
        source.git(["tag", "v-history"]);
        source.git(["push", "origin", "feature/history"]);
        source.git(["push", "origin", "v-history"]);

        fetch_migration_git_refs(&clone_path, "origin")
            .expect("migration ref refresh should fetch branches and tags");

        for git_ref in ["refs/remotes/origin/feature/history", "refs/tags/v-history"] {
            let output = Command::new("git")
                .args(["rev-parse", "--verify", git_ref])
                .current_dir(&clone_path)
                .output()
                .expect("ref verification should start");
            assert!(output.status.success(), "missing fetched ref {git_ref}");
        }
    }

    #[test]
    fn source_fetch_skips_git_lfs_when_all_objects_are_available() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"already available source bytes");
        write_git_lfs_source_object(&repo, &object, b"already available source bytes");
        let mut fetch_attempted = false;

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            "origin",
            None,
            MigrationFetchMode::CurrentCheckout,
            |_, _| {
                fetch_attempted = true;
                Ok(())
            },
        )
        .expect("available migration objects should not require fetch");

        assert!(!fetch_attempted);
        assert!(report.command.is_none());
        assert!(report.fetched_objects.is_empty());
        assert!(report.unavailable_objects.is_empty());
        assert_eq!(report.before.available_objects().len(), 1);
        assert_eq!(report.after.available_objects().len(), 1);
        assert_eq!(report.before, report.after);
    }

    #[test]
    fn source_fetch_downloads_missing_objects_into_git_lfs_media_storage() {
        let repo = TempRepo::new();
        repo.git([
            "remote",
            "add",
            "origin",
            "https://github.com/owner/repo.git",
        ]);
        let object = test_lfs_object_from_bytes(b"downloaded source bytes");
        let object_for_runner = object.clone();
        let mut observed_command = None;

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            "origin",
            None,
            MigrationFetchMode::selected_refs(["main"]),
            |worktree_root, command| {
                observed_command = Some(command.clone());
                write_git_lfs_source_object_in(
                    &worktree_root.join(".git/lfs/objects"),
                    &object_for_runner,
                    b"downloaded source bytes",
                );
                Ok(())
            },
        )
        .expect("source fetch should re-check downloaded objects");

        let command = observed_command.expect("missing object should run git lfs fetch");
        assert_eq!(
            command.args,
            vec![
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentalways=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentrefsdays=0"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentremoterefs=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentcommitsdays=0"),
                OsString::from("lfs"),
                OsString::from("fetch"),
                OsString::from("--include="),
                OsString::from("--exclude="),
                OsString::from("origin"),
                OsString::from("main"),
            ]
        );
        assert_eq!(
            report.command.as_deref(),
            Some(
                "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= origin main"
            )
        );
        assert_eq!(report.fetched_objects, vec![object]);
        assert!(report.unavailable_objects.is_empty());
    }

    #[test]
    fn source_fetch_downloads_all_fetched_ref_objects_into_git_lfs_media_storage() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"downloaded all-ref source bytes");
        let object_for_runner = object.clone();
        let mut observed_command = None;

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            "origin",
            None,
            MigrationFetchMode::AllFetchedRefs,
            |worktree_root, command| {
                observed_command = Some(command.clone());
                write_git_lfs_source_object_in(
                    &worktree_root.join(".git/lfs/objects"),
                    &object_for_runner,
                    b"downloaded all-ref source bytes",
                );
                Ok(())
            },
        )
        .expect("all-ref source fetch should re-check downloaded objects");

        let command = observed_command.expect("missing object should run git lfs fetch");
        assert_eq!(
            command.args,
            vec![
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentalways=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentrefsdays=0"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentremoterefs=false"),
                OsString::from("-c"),
                OsString::from("lfs.fetchrecentcommitsdays=0"),
                OsString::from("lfs"),
                OsString::from("fetch"),
                OsString::from("--all"),
                OsString::from("origin"),
            ]
        );
        assert_eq!(
            report.command.as_deref(),
            Some(
                "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --all origin"
            )
        );
        assert_eq!(report.fetched_objects, vec![object]);
        assert!(report.unavailable_objects.is_empty());
    }

    #[test]
    fn source_fetch_by_object_id_requests_only_the_missing_object() {
        let repo = TempRepo::new();
        let available = test_lfs_object_from_bytes(b"already available source bytes");
        let missing = test_lfs_object_from_bytes(b"target-missing source bytes");
        write_git_lfs_source_object(&repo, &available, b"already available source bytes");
        let missing_for_runner = missing.clone();
        let mut observed_commands = Vec::new();

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&available, &missing],
            None,
            "origin",
            Some("https://legacy.example/owner/repo.git/info/lfs"),
            MigrationFetchMode::ObjectIds,
            |worktree_root, command| {
                observed_commands.push(command.clone());
                write_git_lfs_source_object_in(
                    &worktree_root.join(".git/lfs/objects"),
                    &missing_for_runner,
                    b"target-missing source bytes",
                );
                Ok(())
            },
        )
        .expect("object-ID source fetch should re-check requested objects");

        assert_eq!(observed_commands.len(), 1);
        assert_eq!(
            observed_commands[0].args,
            vec![
                OsString::from("-c"),
                OsString::from("lfs.url=https://legacy.example/owner/repo.git/info/lfs"),
                OsString::from("-c"),
                OsString::from("lfs.fetchinclude="),
                OsString::from("-c"),
                OsString::from("lfs.fetchexclude="),
                OsString::from("lfs"),
                OsString::from("smudge"),
            ]
        );
        assert_eq!(
            observed_commands[0].stdin.as_deref(),
            Some(LfsPointer::new(missing.clone()).to_pointer_file().as_bytes())
        );
        assert_eq!(report.fetched_objects, vec![missing]);
        assert!(report.unavailable_objects.is_empty());
    }

    #[test]
    fn source_fetch_reports_objects_still_unavailable_after_fetch() {
        let repo = TempRepo::new();
        let object = test_lfs_object_from_bytes(b"still missing source bytes");

        let report = fetch_missing_migration_objects_with_runner(
            repo.path(),
            [&object],
            None,
            "origin",
            None,
            MigrationFetchMode::CurrentCheckout,
            |_, _| Ok(()),
        )
        .expect("source fetch report should include objects still missing afterward");

        assert_eq!(
            report.command.as_deref(),
            Some(
                "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= origin"
            )
        );
        assert!(report.fetched_objects.is_empty());
        assert_eq!(report.unavailable_objects, vec![object]);
    }

    #[test]
    fn source_fetch_commands_match_migration_scope() {
        let current =
            migration_source_fetch_command("upstream", None, &MigrationFetchMode::CurrentCheckout)
                .expect("current checkout fetch command should be built");
        assert_eq!(
            current.display,
            "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= upstream"
        );

        let selected = migration_source_fetch_command(
            "upstream",
            None,
            &MigrationFetchMode::selected_refs(["main", "refs/tags/v1"]),
        )
        .expect("selected-ref fetch command should be built");
        assert_eq!(
            selected.display,
            "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --include= --exclude= upstream main refs/tags/v1"
        );

        let all_refs =
            migration_source_fetch_command("upstream", None, &MigrationFetchMode::AllFetchedRefs)
                .expect("all-ref fetch command should be built");
        assert_eq!(
            all_refs.display,
            "git -c lfs.fetchrecentalways=false -c lfs.fetchrecentrefsdays=0 -c lfs.fetchrecentremoterefs=false -c lfs.fetchrecentcommitsdays=0 lfs fetch --all upstream"
        );
    }

    #[test]
    fn source_fetch_command_can_override_committed_lfscloud_target_with_legacy_url() {
        let legacy = "https://legacy.example/owner/repo.git/info/lfs";
        let command = migration_source_fetch_command(
            "origin",
            Some(legacy),
            &MigrationFetchMode::AllFetchedRefs,
        )
        .expect("legacy source fetch command should be built");

        assert!(
            command
                .args
                .windows(2)
                .any(|window| window == [OsString::from("-c"), OsString::from(format!("lfs.url={legacy}"))])
        );
        assert!(command.display.contains("lfs.url=https://legacy.example"));
    }

    #[test]
    fn legacy_source_endpoint_rejects_embedded_credentials() {
        let error = validated_migration_source_endpoint(
            "https://user:secret@legacy.example/owner/repo.git/info/lfs",
            false,
        )
        .expect_err("credentialed source URL must not be committed or invoked");

        assert!(matches!(error, MigrationError::InvalidInput { .. }));
        assert!(!error.to_string().contains("secret"));
    }

    #[test]
    fn source_fetch_commands_disable_recent_fetch_configuration() {
        for mode in [
            MigrationFetchMode::CurrentCheckout,
            MigrationFetchMode::selected_refs(["main"]),
            MigrationFetchMode::AllFetchedRefs,
        ] {
            let command = migration_source_fetch_command("origin", None, &mode)
                .expect("migration source fetch command should be built");

            assert_eq!(
                &command.args[..8],
                [
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentalways=false"),
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentrefsdays=0"),
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentremoterefs=false"),
                    OsString::from("-c"),
                    OsString::from("lfs.fetchrecentcommitsdays=0"),
                ]
            );
        }
    }

    #[test]
    fn source_fetch_command_display_quotes_ambiguous_arguments() {
        let display = display_git_command(&[
            OsString::from("lfs"),
            OsString::from("fetch"),
            OsString::from("feature branch"),
            OsString::from("release'candidate"),
        ]);

        assert_eq!(
            display,
            "git lfs fetch 'feature branch' 'release'\\''candidate'"
        );
    }

    #[test]
    fn source_fetch_rejects_empty_or_unsafe_selected_refs() {
        let mode = MigrationFetchMode::selected_refs(["main", "refs/tags/v1"]);
        assert_eq!(
            mode.selected_ref_names(),
            Some(&["main".to_owned(), "refs/tags/v1".to_owned()][..])
        );

        assert!(matches!(
            migration_source_fetch_command(
                "origin",
                None,
                &MigrationFetchMode::SelectedRefs { refs: Vec::new() }
            ),
            Err(MigrationError::InvalidInput { .. })
        ));
        assert!(matches!(
            migration_source_fetch_command(
                "origin",
                None,
                &MigrationFetchMode::selected_refs(["main..feature"])
            ),
            Err(MigrationError::InvalidInput { .. })
        ));
    }

    #[test]
    fn source_fetch_timeout_stops_stderr_holding_descendants() {
        let temp = tempfile::tempdir().expect("tempdir should be created");
        let marker_path = temp.path().join("descendant-survived");
        let test_executable = std::env::current_exe().expect("test executable should resolve");
        let mut command = Command::new(test_executable);
        command
            .args(["--ignored", "--exact", PROCESS_TREE_HELPER_TEST])
            .env(PROCESS_TREE_MARKER_ENV, &marker_path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn().expect("test shell should start");

        let started_at = Instant::now();
        let error =
            wait_for_git_command(&mut child, "git lfs fetch test", Duration::from_millis(50))
                .expect_err("timed-out command should fail");

        assert!(
            started_at.elapsed() < Duration::from_secs(5),
            "timeout cleanup should not block on descendant stderr handles"
        );
        assert!(matches!(
            error,
            MigrationError::ExternalCommand { status, .. }
                if status == "timed out after 0 seconds"
        ));
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            !marker_path.exists(),
            "timeout cleanup left a descendant process alive"
        );
    }

    #[test]
    fn source_fetch_missing_stderr_stops_the_configured_process_tree() {
        use std::io::Read as _;

        let temp = tempfile::tempdir().expect("tempdir should be created");
        let marker_path = temp.path().join("descendant-survived");
        let test_executable = std::env::current_exe().expect("test executable should resolve");
        let mut command = Command::new(test_executable);
        command
            .args(["--ignored", "--exact", PROCESS_TREE_HELPER_TEST])
            .env(PROCESS_TREE_MARKER_ENV, &marker_path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        configure_process_tree(&mut command);
        let mut child = command.spawn().expect("test helper should start");
        let mut readiness_output = Vec::new();
        while !readiness_output.ends_with(b"ready\n") {
            let mut byte = [0_u8; 1];
            child
                .stdout
                .as_mut()
                .expect("helper stdout should be captured")
                .read_exact(&mut byte)
                .expect("helper should report that its descendant started");
            readiness_output.push(byte[0]);
            assert!(
                readiness_output.len() <= 1024,
                "helper readiness output exceeded its test boundary"
            );
        }

        let error = wait_for_git_command(
            &mut child,
            "git lfs fetch missing-stderr test",
            Duration::from_secs(5),
        )
        .expect_err("missing stderr capture should fail");

        assert!(matches!(
            error,
            MigrationError::Io { context, .. }
                if context
                    == "failed to capture stderr from git lfs fetch missing-stderr test"
        ));
        std::thread::sleep(Duration::from_millis(700));
        assert!(
            !marker_path.exists(),
            "missing-stderr cleanup left a descendant process alive"
        );
    }

    #[test]
    #[ignore = "invoked as a platform-native process-tree test helper"]
    fn migration_process_tree_helper() {
        use std::io::Write as _;

        let Some(marker_path) = std::env::var_os(PROCESS_TREE_MARKER_ENV) else {
            return;
        };
        let test_executable = std::env::current_exe().expect("test executable should resolve");
        let mut descendant = Command::new(test_executable)
            .args(["--ignored", "--exact", PROCESS_TREE_DESCENDANT_TEST])
            .env(PROCESS_TREE_MARKER_ENV, marker_path)
            .spawn()
            .expect("descendant helper should start");
        std::io::stdout()
            .write_all(b"ready\n")
            .expect("helper readiness should be writable");
        std::io::stdout()
            .flush()
            .expect("helper readiness should be flushed");

        descendant
            .wait()
            .expect("descendant helper should remain waitable");
    }

    #[test]
    #[ignore = "invoked as a platform-native process-tree test descendant"]
    fn migration_process_tree_descendant() {
        let Some(marker_path) = std::env::var_os(PROCESS_TREE_MARKER_ENV) else {
            return;
        };

        std::thread::sleep(Duration::from_millis(500));
        fs::write(marker_path, b"descendant survived timeout cleanup")
            .expect("descendant marker should be writable");
    }

    #[ignore = "manual verification requires git-lfs and a local source repository"]
    #[test]
    fn source_fetch_downloads_missing_objects_without_changing_worktree_files() {
        require_git_lfs();

        let source = TempRepo::new();
        source.git(["lfs", "install", "--local"]);
        source.git(["lfs", "track", "*.bin"]);
        source.write_bytes("asset/model.bin", b"real source lfs bytes");
        source.commit_all("add source lfs object");
        source.git(["switch", "-c", "recent-extra"]);
        source.write_bytes("asset/recent-extra.bin", b"out-of-scope recent lfs bytes");
        source.commit_all("add recent out-of-scope lfs object");
        source.git(["switch", "main"]);
        let out_of_scope_object = test_lfs_object_from_bytes(b"out-of-scope recent lfs bytes");

        let temp = tempfile::tempdir().expect("temporary clone parent should be created");
        let clone_path = temp.path().join("clone");
        let output = Command::new("git")
            .arg("clone")
            .arg(source.path())
            .arg(&clone_path)
            .env("GIT_LFS_SKIP_SMUDGE", "1")
            .output()
            .expect("git clone should start");
        assert!(
            output.status.success(),
            "git clone failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );

        match fs::remove_dir_all(clone_path.join(".git/lfs/objects")) {
            Ok(()) => {}
            Err(source) if source.kind() == io::ErrorKind::NotFound => {}
            Err(source) => panic!("clone LFS media storage should be removable: {source}"),
        }
        for (key, value) in [
            ("lfs.fetchrecentalways", "true"),
            ("lfs.fetchrecentrefsdays", "36500"),
            ("lfs.fetchrecentremoterefs", "true"),
            ("lfs.fetchrecentcommitsdays", "36500"),
        ] {
            let output = Command::new("git")
                .args(["config", "--local", key, value])
                .current_dir(&clone_path)
                .output()
                .expect("hostile recent-fetch configuration should start");
            assert!(
                output.status.success(),
                "hostile recent-fetch configuration failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let pointer_path = clone_path.join("asset/model.bin");
        let pointer_before =
            fs::read(&pointer_path).expect("clone pointer file should be readable");
        let pointers = enumerate_current_checkout_lfs_pointers(&clone_path)
            .expect("clone pointer should be discoverable");
        assert_eq!(pointers.pointers.len(), 1);

        let report = fetch_missing_migration_objects(
            &clone_path,
            pointers.pointers.iter().map(|pointer| &pointer.object),
            None,
            MigrationFetchMode::CurrentCheckout,
        )
        .expect("real git lfs fetch should download missing object bytes");

        assert_eq!(
            fs::read(&pointer_path).expect("clone pointer file should remain readable"),
            pointer_before,
            "git lfs fetch must not smudge or replace checkout files"
        );
        assert_eq!(report.fetched_objects.len(), 1);
        assert!(report.unavailable_objects.is_empty());
        assert!(
            !git_lfs_object_path(
                &clone_path.join(".git/lfs/objects"),
                &out_of_scope_object.oid
            )
            .expect("out-of-scope Git LFS object path should be valid")
            .exists(),
            "hostile recent-fetch configuration must not expand migration scope"
        );
        assert_git_status_clean(&clone_path);
    }

}
