// This file is included by `mod.rs` so the migration API remains in one module.

/// Git LFS pointers discovered by scanning Git history for one or more refs.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsHistoryPointers {
    /// Git worktree root whose object database was inspected.
    pub worktree_root: PathBuf,
    /// Refs that were resolved and scanned.
    pub refs: Vec<GitLfsScannedRef>,
    /// Pointer occurrences found in commits reachable from the scanned refs.
    pub pointers: Vec<GitLfsHistoryPointer>,
}

/// A Git ref that was resolved for migration history scanning.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsScannedRef {
    /// Ref name requested or discovered for scanning.
    pub name: String,
    /// Commit object ID that the ref resolved to when scanning started.
    pub commit: String,
}

/// A Git LFS pointer found in a commit reachable from a scanned ref.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub struct GitLfsHistoryPointer {
    /// Ref whose reachable history contained this pointer.
    pub ref_name: String,
    /// Commit object ID containing this pointer at `relative_path`.
    pub commit: String,
    /// Repository-relative path to the pointer file in that commit.
    pub relative_path: PathBuf,
    /// Object identity referenced by the pointer file.
    pub object: LfsObject,
}

/// Enumerates Git LFS pointer files reachable from selected refs.
///
/// This function is intentionally read-only. It resolves each ref to a commit,
/// walks reachable commits, asks Git to evaluate `filter=lfs` attributes at
/// each historical tree, and parses only small LFS pointer blobs at matching
/// paths. It does not fetch objects, check out refs, or mutate repository
/// state.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// Git is older than 2.40, the repository is shallow, any selected ref cannot
/// be resolved to a commit, or Git returns malformed history, attribute, or
/// object data.
pub fn enumerate_selected_ref_lfs_pointers<I, S>(
    start_dir: impl AsRef<Path>,
    refs: I,
) -> MigrationResult<GitLfsHistoryPointers>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    enumerate_selected_ref_lfs_pointers_with_metrics(start_dir, refs).map(|(pointers, _)| pointers)
}

fn enumerate_selected_ref_lfs_pointers_with_metrics<I, S>(
    start_dir: impl AsRef<Path>,
    refs: I,
) -> MigrationResult<(GitLfsHistoryPointers, HistoryScanMetrics)>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    require_historical_scan_git_version(&worktree_root)?;
    require_complete_history(&worktree_root)?;
    let mut scanned_refs = Vec::new();

    for ref_name in refs {
        let ref_name = ref_name.as_ref();
        validate_history_ref_name(ref_name)?;
        let commit = resolve_ref_commit(&worktree_root, ref_name)?;
        scanned_refs.push(GitLfsScannedRef {
            name: ref_name.to_owned(),
            commit: commit.clone(),
        });
    }

    scan_resolved_history_refs(worktree_root, scanned_refs)
}

/// Enumerates Git LFS pointer files reachable from local branches, tags, and
/// fetched `origin` remote-tracking refs.
///
/// Symbolic refs are skipped so aliases such as `refs/remotes/origin/HEAD` do
/// not duplicate another ref's history. Use
/// [`enumerate_fetched_ref_lfs_pointers_for_remote`] to select another explicit
/// source remote.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree, Git
/// cannot list refs, or any discovered ref cannot be scanned.
pub fn enumerate_all_fetched_ref_lfs_pointers(
    start_dir: impl AsRef<Path>,
) -> MigrationResult<GitLfsHistoryPointers> {
    enumerate_fetched_ref_lfs_pointers_for_remote(start_dir, DEFAULT_REMOTE_NAME)
}

/// Enumerates Git LFS pointers reachable from local branches, tags, and one
/// explicit source remote's fetched remote-tracking refs.
///
/// Scoping remote-tracking refs prevents an all-ref migration from silently
/// mixing histories fetched from unrelated repository remotes. Local branches
/// and tags remain included because they are repository-owned refs rather than
/// remote-tracking namespaces.
///
/// # Errors
///
/// Returns [`MigrationError`] when `start_dir` is not inside a Git worktree,
/// Git is older than 2.40, the repository is shallow, `source_remote` is
/// invalid, Git cannot list refs, or a discovered ref cannot be scanned.
pub fn enumerate_fetched_ref_lfs_pointers_for_remote(
    start_dir: impl AsRef<Path>,
    source_remote: impl AsRef<str>,
) -> MigrationResult<GitLfsHistoryPointers> {
    let start_dir = start_dir.as_ref();
    let worktree_root = detect_worktree_root(start_dir)?;
    require_historical_scan_git_version(&worktree_root)?;
    require_complete_history(&worktree_root)?;
    let source_remote = validate_source_remote_name(source_remote.as_ref())?;
    let refs = all_fetched_ref_names(&worktree_root, &source_remote)?;
    let mut scanned_refs = Vec::new();

    for ref_name in refs {
        let commit = resolve_ref_commit(&worktree_root, &ref_name)?;
        scanned_refs.push(GitLfsScannedRef {
            name: ref_name.clone(),
            commit: commit.clone(),
        });
    }

    scan_resolved_history_refs(worktree_root, scanned_refs).map(|(pointers, _)| pointers)
}
fn require_historical_scan_git_version(worktree_root: &Path) -> MigrationResult<()> {
    const COMMAND: &str = "git --version";
    let output = run_git(worktree_root, ["--version"])?;
    let stdout = required_success_stdout(output, COMMAND)?;
    validate_historical_scan_git_version(&stdout)
}

fn validate_historical_scan_git_version(output: &str) -> MigrationResult<()> {
    let version_text = output
        .trim()
        .strip_prefix("git version ")
        .and_then(|version| version.split_ascii_whitespace().next())
        .ok_or_else(git_version_parse_error)?;
    let version = parse_git_version(version_text).ok_or_else(git_version_parse_error)?;

    if version < MINIMUM_HISTORICAL_SCAN_GIT_VERSION {
        return Err(MigrationError::UnsupportedGitVersion {
            installed: version_text.to_owned(),
            required: MINIMUM_HISTORICAL_SCAN_GIT_VERSION_TEXT,
            feature: "historical migration attribute discovery",
        });
    }

    Ok(())
}

fn parse_git_version(version: &str) -> Option<GitVersion> {
    let mut components = version.split('.');
    let major = components.next()?.parse().ok()?;
    let minor = components.next()?.parse().ok()?;
    let patch_text = components.next()?;
    let patch_digits = patch_text
        .bytes()
        .take_while(u8::is_ascii_digit)
        .collect::<Vec<_>>();
    let patch = std::str::from_utf8(&patch_digits).ok()?.parse().ok()?;

    Some(GitVersion::new(major, minor, patch))
}

fn git_version_parse_error() -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: "git --version".to_owned(),
        message: SanitizedMessage::new(
            "could not determine whether Git 2.40.0 or newer is installed; upgrade Git before scanning selected refs or all refs",
        ),
    }
}

fn require_complete_history(worktree_root: &Path) -> MigrationResult<()> {
    const COMMAND: &str = "git rev-parse --is-shallow-repository";
    let output = run_git(worktree_root, ["rev-parse", "--is-shallow-repository"])?;
    let stdout = required_success_stdout(output, COMMAND)?;

    match stdout.trim() {
        "false" => Ok(()),
        "true" => Err(MigrationError::ShallowRepository {
            path: worktree_root.to_path_buf(),
        }),
        _ => Err(MigrationError::ExternalCommandOutput {
            command: COMMAND.to_owned(),
            message: SanitizedMessage::new("git returned an invalid shallow-repository state"),
        }),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitTreeBlob {
    object_id: String,
    relative_path: PathBuf,
    relative_path_bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitLfsHistoryPointerOccurrence {
    commit: String,
    relative_path: PathBuf,
    object: LfsObject,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HistoryScanMetrics {
    cat_file_processes: usize,
    attribute_processes: usize,
    tree_entries_inspected: usize,
    blobs_inspected: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitHistoryCommit {
    object_id: String,
    tree_id: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct HistoryTreeSummary {
    pointer_blobs: Vec<GitTreeBlob>,
    attribute_blobs: Vec<(PathBuf, String)>,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct HistoryAttributeQueryKey {
    attribute_blobs: Vec<(PathBuf, String)>,
    pointer_paths: Vec<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RawGitTreeEntry {
    mode: Vec<u8>,
    object_id: String,
    name: Vec<u8>,
}

struct HistoryScanner<'a> {
    worktree_root: &'a Path,
    object_reader: Option<GitBatchObjectReader>,
    tree_cache: BTreeMap<String, HistoryTreeSummary>,
    blob_pointer_cache: BTreeMap<String, Option<LfsPointer>>,
    attribute_cache: BTreeMap<HistoryAttributeQueryKey, BTreeSet<PathBuf>>,
    commit_cache: BTreeMap<String, Vec<GitLfsHistoryPointerOccurrence>>,
    pointers: Vec<GitLfsHistoryPointer>,
    seen: BTreeSet<(String, PathBuf, LfsObject)>,
    metrics: HistoryScanMetrics,
}

impl<'a> HistoryScanner<'a> {
    fn new(worktree_root: &'a Path) -> MigrationResult<Self> {
        Ok(Self {
            worktree_root,
            object_reader: Some(GitBatchObjectReader::start(worktree_root)?),
            tree_cache: BTreeMap::new(),
            blob_pointer_cache: BTreeMap::new(),
            attribute_cache: BTreeMap::new(),
            commit_cache: BTreeMap::new(),
            pointers: Vec::new(),
            seen: BTreeSet::new(),
            metrics: HistoryScanMetrics {
                cat_file_processes: 1,
                ..HistoryScanMetrics::default()
            },
        })
    }

    fn scan_ref(&mut self, scanned_ref: &GitLfsScannedRef) -> MigrationResult<()> {
        for commit in rev_list_commits(self.worktree_root, &scanned_ref.commit)? {
            if !self.commit_cache.contains_key(&commit.object_id) {
                let occurrences = self.scan_commit(&commit)?;
                self.commit_cache
                    .insert(commit.object_id.clone(), occurrences);
            }

            for occurrence in self
                .commit_cache
                .get(&commit.object_id)
                .expect("history commit cache should contain scanned commit")
            {
                let key = (
                    occurrence.commit.clone(),
                    occurrence.relative_path.clone(),
                    occurrence.object.clone(),
                );
                if self.seen.insert(key) {
                    self.pointers.push(GitLfsHistoryPointer {
                        ref_name: scanned_ref.name.clone(),
                        commit: occurrence.commit.clone(),
                        relative_path: occurrence.relative_path.clone(),
                        object: occurrence.object.clone(),
                    });
                }
            }
        }

        Ok(())
    }

    fn scan_commit(
        &mut self,
        commit: &GitHistoryCommit,
    ) -> MigrationResult<Vec<GitLfsHistoryPointerOccurrence>> {
        let summary = self.tree_summary(&commit.tree_id)?;
        if summary.pointer_blobs.is_empty() {
            return Ok(Vec::new());
        }

        let query_key = HistoryAttributeQueryKey {
            attribute_blobs: summary.attribute_blobs.clone(),
            pointer_paths: summary
                .pointer_blobs
                .iter()
                .map(|blob| blob.relative_path.clone())
                .collect(),
        };
        let lfs_paths = if let Some(paths) = self.attribute_cache.get(&query_key) {
            paths.clone()
        } else {
            let (paths, process_count) = git_check_attr_lfs_paths_for_tree_blobs(
                self.worktree_root,
                &summary.pointer_blobs,
                &commit.object_id,
            )?;
            self.metrics.attribute_processes += process_count;
            self.attribute_cache.insert(query_key, paths.clone());
            paths
        };

        let mut occurrences = Vec::new();
        for blob in summary
            .pointer_blobs
            .into_iter()
            .filter(|blob| lfs_paths.contains(&blob.relative_path))
        {
            let pointer = self
                .blob_pointer_cache
                .get(&blob.object_id)
                .and_then(Clone::clone)
                .expect("tree summaries contain only parsed pointer blobs");
            occurrences.push(GitLfsHistoryPointerOccurrence {
                commit: commit.object_id.clone(),
                relative_path: blob.relative_path,
                object: pointer.object,
            });
        }

        Ok(occurrences)
    }

    fn tree_summary(&mut self, tree_id: &str) -> MigrationResult<HistoryTreeSummary> {
        if let Some(summary) = self.tree_cache.get(tree_id) {
            return Ok(summary.clone());
        }

        let command_name = format!("git cat-file --batch-command tree {tree_id}");
        let contents = self
            .object_reader
            .as_mut()
            .expect("history scanner object reader should be available")
            .contents(
                tree_id,
                "tree",
                MAX_HISTORY_TREE_OUTPUT_BYTES,
                &command_name,
            )?;
        let entries = parse_raw_git_tree(&contents, tree_id, &command_name)?;
        self.metrics.tree_entries_inspected += entries.len();
        let mut summary = HistoryTreeSummary::default();

        for entry in entries {
            let relative_path = safe_git_relative_path(&entry.name, &command_name)?;
            if entry.mode == b"40000" {
                let child = self.tree_summary(&entry.object_id)?;
                append_prefixed_tree_summary(&mut summary, &relative_path, &entry.name, child);
                continue;
            }
            if entry.mode == b"160000" {
                continue;
            }
            if !matches!(entry.mode.as_slice(), b"100644" | b"100755" | b"120000") {
                return Err(MigrationError::ExternalCommandOutput {
                    command: command_name,
                    message: SanitizedMessage::new("git returned an unsupported tree mode"),
                });
            }

            if entry.name == b".gitattributes" {
                summary
                    .attribute_blobs
                    .push((relative_path.clone(), entry.object_id.clone()));
            }

            if !self.blob_pointer_cache.contains_key(&entry.object_id) {
                self.metrics.blobs_inspected += 1;
                let pointer = self.read_pointer_blob_candidate(&entry.object_id)?;
                self.blob_pointer_cache
                    .insert(entry.object_id.clone(), pointer);
            }
            if self
                .blob_pointer_cache
                .get(&entry.object_id)
                .is_some_and(Option::is_some)
            {
                summary.pointer_blobs.push(GitTreeBlob {
                    object_id: entry.object_id,
                    relative_path,
                    relative_path_bytes: entry.name,
                });
            }
        }

        summary
            .pointer_blobs
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        summary.attribute_blobs.sort();
        self.tree_cache.insert(tree_id.to_owned(), summary.clone());
        Ok(summary)
    }

    fn read_pointer_blob_candidate(
        &mut self,
        object_id: &str,
    ) -> MigrationResult<Option<LfsPointer>> {
        let command_name = format!("git cat-file --batch-command info {object_id}");
        let info = self
            .object_reader
            .as_mut()
            .expect("history scanner object reader should be available")
            .info(object_id, &command_name)?;
        if info.object_type != "blob" {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name,
                message: SanitizedMessage::new("git tree entry did not resolve to a blob"),
            });
        }
        if info.size >= LFS_POINTER_SIZE_CUTOFF {
            return Ok(None);
        }

        let blob_command = format!("git cat-file --batch-command contents {object_id}");
        let contents = self
            .object_reader
            .as_mut()
            .expect("history scanner object reader should be available")
            .contents(
                object_id,
                "blob",
                LFS_POINTER_SIZE_CUTOFF as usize,
                &blob_command,
            )?;
        let Ok(contents) = std::str::from_utf8(&contents) else {
            return Ok(None);
        };

        Ok(LfsPointer::parse(contents)
            .ok()
            .filter(|pointer| !pointer.is_empty()))
    }

    fn finish(mut self) -> MigrationResult<(Vec<GitLfsHistoryPointer>, HistoryScanMetrics)> {
        self.object_reader
            .take()
            .expect("history scanner object reader should be available")
            .finish()?;
        Ok((std::mem::take(&mut self.pointers), self.metrics.clone()))
    }
}

fn scan_resolved_history_refs(
    worktree_root: PathBuf,
    scanned_refs: Vec<GitLfsScannedRef>,
) -> MigrationResult<(GitLfsHistoryPointers, HistoryScanMetrics)> {
    if scanned_refs.is_empty() {
        return Ok((
            GitLfsHistoryPointers {
                worktree_root,
                refs: scanned_refs,
                pointers: Vec::new(),
            },
            HistoryScanMetrics::default(),
        ));
    }

    let mut scanner = HistoryScanner::new(&worktree_root)?;
    for scanned_ref in &scanned_refs {
        scanner.scan_ref(scanned_ref)?;
    }
    let (pointers, metrics) = scanner.finish()?;
    Ok((
        GitLfsHistoryPointers {
            worktree_root,
            refs: scanned_refs,
            pointers,
        },
        metrics,
    ))
}

fn append_prefixed_tree_summary(
    target: &mut HistoryTreeSummary,
    prefix: &Path,
    prefix_bytes: &[u8],
    child: HistoryTreeSummary,
) {
    target
        .pointer_blobs
        .extend(child.pointer_blobs.into_iter().map(|blob| {
            let mut relative_path_bytes =
                Vec::with_capacity(prefix_bytes.len() + 1 + blob.relative_path_bytes.len());
            relative_path_bytes.extend_from_slice(prefix_bytes);
            relative_path_bytes.push(b'/');
            relative_path_bytes.extend_from_slice(&blob.relative_path_bytes);
            GitTreeBlob {
                object_id: blob.object_id,
                relative_path: prefix.join(blob.relative_path),
                relative_path_bytes,
            }
        }));
    target.attribute_blobs.extend(
        child
            .attribute_blobs
            .into_iter()
            .map(|(path, object_id)| (prefix.join(path), object_id)),
    );
}

fn validate_history_ref_name(ref_name: &str) -> MigrationResult<()> {
    let has_invalid_byte = ref_name.bytes().any(|byte| {
        byte.is_ascii_control()
            || matches!(byte, b' ' | b'~' | b'^' | b':' | b'?' | b'*' | b'[' | b'\\')
    });
    let has_invalid_sequence =
        ref_name.contains("..") || ref_name.contains("@{") || ref_name.contains("//");
    let has_invalid_boundary = ref_name.starts_with('/')
        || ref_name.ends_with('/')
        || ref_name.ends_with('.')
        || ref_name.ends_with(".lock");

    if !ref_name.is_empty()
        && !has_invalid_byte
        && !has_invalid_sequence
        && !has_invalid_boundary
        && ref_name != "@"
        && ref_name != "HEAD"
    {
        return Ok(());
    }

    Err(MigrationError::InvalidInput {
        message: SanitizedMessage::new("selected ref name is empty or contains invalid ref syntax"),
    })
}

fn resolve_ref_commit(worktree_root: &Path, ref_name: &str) -> MigrationResult<String> {
    let revision = format!("{ref_name}^{{commit}}");
    let command_name = format!("git rev-parse --verify --end-of-options {revision}");
    let output = run_git_os(
        worktree_root,
        vec![
            OsString::from("rev-parse"),
            OsString::from("--verify"),
            OsString::from("--end-of-options"),
            OsString::from(revision),
        ],
        &command_name,
    )?;
    let stdout = required_success_stdout(output, &command_name)?;
    let commit = stdout.trim_end_matches(['\n', '\r']);

    if is_git_object_id(commit) {
        Ok(commit.to_owned())
    } else {
        Err(MigrationError::ExternalCommandOutput {
            command: command_name,
            message: SanitizedMessage::new("git returned an invalid commit object ID"),
        })
    }
}

fn all_fetched_ref_names(
    worktree_root: &Path,
    source_remote: &str,
) -> MigrationResult<Vec<String>> {
    let command_name = "git for-each-ref --format=%(refname)%00%(symref) refs/heads refs/remotes/<source> refs/tags";
    let remote_refs = format!("refs/remotes/{source_remote}");
    let output = run_git_os_with_limit(
        worktree_root,
        vec![
            OsString::from("for-each-ref"),
            OsString::from("--format=%(refname)%00%(symref)"),
            OsString::from("refs/heads"),
            OsString::from(remote_refs),
            OsString::from("refs/tags"),
        ],
        command_name,
        MAX_HISTORY_REF_LIST_BYTES,
    )?;
    let stdout =
        required_success_stdout_with_limit(output, command_name, MAX_HISTORY_REF_LIST_BYTES)?;
    let mut refs = Vec::new();

    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let Some((ref_name, symref)) = line.split_once('\0') else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed ref output"),
            });
        };

        if symref.is_empty() {
            refs.push(ref_name.to_owned());
        }
    }

    refs.sort();
    refs.dedup();
    Ok(refs)
}

fn rev_list_commits(
    worktree_root: &Path,
    root_commit: &str,
) -> MigrationResult<Vec<GitHistoryCommit>> {
    let command_name =
        format!("git rev-list --topo-order --format=%H%x20%T --no-commit-header {root_commit}");
    let output = run_git_os_with_limit(
        worktree_root,
        vec![
            OsString::from("rev-list"),
            OsString::from("--topo-order"),
            OsString::from("--format=%H %T"),
            OsString::from("--no-commit-header"),
            OsString::from(root_commit),
        ],
        &command_name,
        MAX_HISTORY_COMMIT_LIST_BYTES,
    )?;
    let stdout =
        required_success_stdout_with_limit(output, &command_name, MAX_HISTORY_COMMIT_LIST_BYTES)?;
    let mut commits = Vec::new();

    for line in stdout.lines().filter(|line| !line.is_empty()) {
        let Some((object_id, tree_id)) = line.split_once(' ') else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.clone(),
                message: SanitizedMessage::new("git returned malformed commit and tree output"),
            });
        };
        if !is_git_object_id(object_id) || !is_git_object_id(tree_id) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.clone(),
                message: SanitizedMessage::new("git returned an invalid commit or tree object ID"),
            });
        }
        commits.push(GitHistoryCommit {
            object_id: object_id.to_owned(),
            tree_id: tree_id.to_owned(),
        });
    }

    Ok(commits)
}

fn parse_raw_git_tree(
    contents: &[u8],
    tree_id: &str,
    command_name: &str,
) -> MigrationResult<Vec<RawGitTreeEntry>> {
    let object_id_bytes = tree_id.len() / 2;
    if !matches!(object_id_bytes, 20 | 32) {
        return Err(MigrationError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new("git returned an invalid tree object ID"),
        });
    }

    let mut entries = Vec::new();
    let mut cursor = 0;
    while cursor < contents.len() {
        let Some(mode_end_offset) = contents[cursor..].iter().position(|byte| *byte == b' ') else {
            return Err(raw_git_tree_parse_error(command_name));
        };
        let mode_end = cursor + mode_end_offset;
        let mode = contents[cursor..mode_end].to_vec();
        cursor = mode_end + 1;

        let Some(name_end_offset) = contents[cursor..].iter().position(|byte| *byte == b'\0')
        else {
            return Err(raw_git_tree_parse_error(command_name));
        };
        let name_end = cursor + name_end_offset;
        let name = contents[cursor..name_end].to_vec();
        cursor = name_end + 1;

        let object_end = cursor
            .checked_add(object_id_bytes)
            .ok_or_else(|| raw_git_tree_parse_error(command_name))?;
        let object_bytes = contents
            .get(cursor..object_end)
            .ok_or_else(|| raw_git_tree_parse_error(command_name))?;
        cursor = object_end;

        if mode.is_empty() || name.is_empty() {
            return Err(raw_git_tree_parse_error(command_name));
        }
        let object_id = object_bytes
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        entries.push(RawGitTreeEntry {
            mode,
            object_id,
            name,
        });
    }

    Ok(entries)
}

fn raw_git_tree_parse_error(command_name: &str) -> MigrationError {
    MigrationError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git returned malformed tree object data"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct GitBatchObjectInfo {
    object_type: String,
    size: u64,
}

struct GitBatchObjectReader {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    stderr_reader: Option<thread::JoinHandle<io::Result<PipeReadResult>>>,
    finished: bool,
}

impl GitBatchObjectReader {
    fn start(worktree_root: &Path) -> MigrationResult<Self> {
        const COMMAND: &str = "git cat-file --batch-command";
        let mut child = read_only_git_command()
            .args(["cat-file", "--batch-command"])
            .current_dir(worktree_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|source| MigrationError::Io {
                context: format!("failed to start {COMMAND}"),
                source,
            })?;
        let stdin = child.stdin.take().ok_or_else(|| MigrationError::Io {
            context: format!("{COMMAND} stdin was not piped"),
            source: io::Error::other("git cat-file stdin was not piped"),
        })?;
        let stdout = child.stdout.take().ok_or_else(|| MigrationError::Io {
            context: format!("{COMMAND} stdout was not piped"),
            source: io::Error::other("git cat-file stdout was not piped"),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| MigrationError::Io {
            context: format!("{COMMAND} stderr was not piped"),
            source: io::Error::other("git cat-file stderr was not piped"),
        })?;
        let stderr_reader =
            thread::spawn(move || read_pipe_with_limit(stderr, MAX_MIGRATION_GIT_OUTPUT_BYTES + 1));

        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            stderr_reader: Some(stderr_reader),
            finished: false,
        })
    }

    fn info(&mut self, object_id: &str, command_name: &str) -> MigrationResult<GitBatchObjectInfo> {
        self.write_request("info", object_id, command_name)?;
        self.read_header(object_id, command_name)
    }

    fn contents(
        &mut self,
        object_id: &str,
        expected_type: &str,
        max_size: usize,
        command_name: &str,
    ) -> MigrationResult<Vec<u8>> {
        self.write_request("contents", object_id, command_name)?;
        let info = self.read_header(object_id, command_name)?;
        if info.object_type != expected_type {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned an unexpected object type"),
            });
        }
        let size =
            usize::try_from(info.size).map_err(|_| MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned an oversized object"),
            })?;
        if size > max_size {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned too much object data"),
            });
        }

        let mut contents = vec![0; size];
        self.stdout
            .read_exact(&mut contents)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} object data"),
                source,
            })?;
        let mut delimiter = [0];
        self.stdout
            .read_exact(&mut delimiter)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} object delimiter"),
                source,
            })?;
        if delimiter != [b'\n'] {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed batch object data"),
            });
        }

        Ok(contents)
    }

    fn write_request(
        &mut self,
        operation: &str,
        object_id: &str,
        command_name: &str,
    ) -> MigrationResult<()> {
        if !is_git_object_id(object_id) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git object request contained an invalid ID"),
            });
        }
        let stdin = self
            .stdin
            .as_mut()
            .expect("unfinished git cat-file reader should retain stdin");
        writeln!(stdin, "{operation} {object_id}")
            .and_then(|()| stdin.flush())
            .map_err(|source| MigrationError::Io {
                context: format!("failed to write {command_name} request"),
                source,
            })
    }

    fn read_header(
        &mut self,
        requested_object_id: &str,
        command_name: &str,
    ) -> MigrationResult<GitBatchObjectInfo> {
        let mut header = Vec::new();
        let bytes_read = self
            .stdout
            .read_until(b'\n', &mut header)
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} response"),
                source,
            })?;
        if bytes_read == 0 || !header.ends_with(b"\n") {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned a truncated batch object header"),
            });
        }
        header.pop();
        if header.ends_with(b" missing") {
            return Err(MigrationError::GitObjectUnavailable {
                object_id: requested_object_id.to_owned(),
            });
        }
        let header =
            std::str::from_utf8(&header).map_err(|_| MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned non-UTF-8 batch object metadata"),
            })?;
        let fields = header.split(' ').collect::<Vec<_>>();
        let [object_id, object_type, size] = fields.as_slice() else {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned malformed batch object metadata"),
            });
        };
        if *object_id != requested_object_id || !is_git_object_id(object_id) {
            return Err(MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned a mismatched batch object ID"),
            });
        }
        let size = size
            .parse::<u64>()
            .map_err(|_| MigrationError::ExternalCommandOutput {
                command: command_name.to_owned(),
                message: SanitizedMessage::new("git returned an invalid batch object size"),
            })?;
        Ok(GitBatchObjectInfo {
            object_type: (*object_type).to_owned(),
            size,
        })
    }

    fn finish(mut self) -> MigrationResult<()> {
        const COMMAND: &str = "git cat-file --batch-command";
        self.stdin.take();
        let status = self
            .child
            .as_mut()
            .expect("unfinished git cat-file reader should retain its child")
            .wait()
            .map_err(|source| MigrationError::Io {
                context: format!("failed to wait for {COMMAND}"),
                source,
            })?;
        let stderr = self.join_stderr_reader(COMMAND)?;
        self.finished = true;
        if !status.success() {
            return Err(command_error(COMMAND, status, &stderr.bytes));
        }
        if stderr.exceeded_limit {
            return Err(MigrationError::ExternalCommandOutput {
                command: COMMAND.to_owned(),
                message: SanitizedMessage::new("git returned too much batch diagnostic output"),
            });
        }
        Ok(())
    }

    fn join_stderr_reader(&mut self, command_name: &str) -> MigrationResult<PipeReadResult> {
        self.stderr_reader
            .take()
            .expect("unfinished git cat-file reader should retain stderr reader")
            .join()
            .map_err(|_| MigrationError::Io {
                context: format!("{command_name} stderr reader panicked"),
                source: io::Error::other("git cat-file stderr reader panicked"),
            })?
            .map_err(|source| MigrationError::Io {
                context: format!("failed to read {command_name} stderr"),
                source,
            })
    }
}

impl Drop for GitBatchObjectReader {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.stdin.take();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if let Some(stderr_reader) = self.stderr_reader.take() {
            let _ = stderr_reader.join();
        }
    }
}

fn git_check_attr_lfs_paths_for_tree_blobs(
    worktree_root: &Path,
    blobs: &[GitTreeBlob],
    commit: &str,
) -> MigrationResult<(BTreeSet<PathBuf>, usize)> {
    let mut lfs_paths = BTreeSet::new();
    let mut path_input = Vec::new();
    let mut process_count = 0;

    for blob in blobs {
        let path_entry_len = blob.relative_path_bytes.len() + 1;
        if path_entry_len > MAX_HISTORY_CHECK_ATTR_INPUT_BYTES {
            return Err(MigrationError::ExternalCommandOutput {
                command: git_check_attr_filter_command_name(Some(commit)),
                message: SanitizedMessage::new(
                    "historical pointer path is too large for attribute lookup",
                ),
            });
        }

        if !path_input.is_empty()
            && path_input.len() + path_entry_len > MAX_HISTORY_CHECK_ATTR_INPUT_BYTES
        {
            append_git_check_attr_lfs_paths(worktree_root, commit, path_input, &mut lfs_paths)?;
            process_count += 1;
            path_input = Vec::new();
        }

        path_input.extend_from_slice(&blob.relative_path_bytes);
        path_input.push(b'\0');
    }

    if !path_input.is_empty() {
        append_git_check_attr_lfs_paths(worktree_root, commit, path_input, &mut lfs_paths)?;
        process_count += 1;
    }

    Ok((lfs_paths, process_count))
}

fn append_git_check_attr_lfs_paths(
    worktree_root: &Path,
    commit: &str,
    path_input: Vec<u8>,
    lfs_paths: &mut BTreeSet<PathBuf>,
) -> MigrationResult<()> {
    let attributes = git_check_attr_filter_with_source(worktree_root, path_input, Some(commit))?;
    let command_name = git_check_attr_filter_command_name(Some(commit));
    lfs_paths.extend(parse_git_check_attr_filter_stdout(
        &attributes.stdout,
        &command_name,
    )?);

    Ok(())
}

fn read_history_pointer_blob_candidate(
    worktree_root: &Path,
    object_id: &str,
) -> MigrationResult<Option<LfsPointer>> {
    let size_command = format!("git cat-file -s {object_id}");
    let size_output = run_git_os(
        worktree_root,
        vec![
            OsString::from("cat-file"),
            OsString::from("-s"),
            OsString::from(object_id),
        ],
        &size_command,
    )?;
    if !size_output.status.success() {
        return Err(MigrationError::GitObjectUnavailable {
            object_id: object_id.to_owned(),
        });
    }
    let size_stdout = output_stdout(size_output, &size_command)?;
    let size = size_stdout
        .trim_end_matches(['\n', '\r'])
        .parse::<u64>()
        .map_err(|_| MigrationError::ExternalCommandOutput {
            command: size_command.clone(),
            message: SanitizedMessage::new("git returned an invalid blob size"),
        })?;
    if size >= LFS_POINTER_SIZE_CUTOFF {
        return Ok(None);
    }

    let blob_command = format!("git cat-file blob {object_id}");
    let blob_output = run_git_os(
        worktree_root,
        vec![
            OsString::from("cat-file"),
            OsString::from("blob"),
            OsString::from(object_id),
        ],
        &blob_command,
    )?;
    if !blob_output.status.success() {
        return Err(command_error(
            &blob_command,
            blob_output.status,
            &blob_output.stderr,
        ));
    }
    if blob_output.stdout.len() >= LFS_POINTER_SIZE_CUTOFF as usize {
        return Ok(None);
    }
    let Ok(contents) = std::str::from_utf8(&blob_output.stdout) else {
        return Ok(None);
    };

    Ok(LfsPointer::parse(contents)
        .ok()
        .filter(|pointer| !pointer.is_empty()))
}

fn is_git_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}


#[cfg(test)]
mod history_tests {
    use super::test_support::*;

    #[test]
    fn historical_scan_rejects_git_older_than_2_40() {
        let error = validate_historical_scan_git_version("git version 2.39.5")
            .expect_err("Git 2.39 should not support historical attribute sources");

        assert!(matches!(
            &error,
            MigrationError::UnsupportedGitVersion {
                installed,
                required: "2.40.0",
                ..
            } if installed == "2.39.5"
        ));
        assert!(error.to_string().contains("upgrade Git"));
        assert!(error.to_string().contains("current-checkout"));
    }

    #[test]
    fn historical_scan_accepts_supported_git_version_variants() {
        for output in [
            "git version 2.40.0\n",
            "git version 2.52.0 (Apple Git-154)\n",
            "git version 2.40.0.windows.1\n",
            "git version 3.0.0\n",
        ] {
            validate_historical_scan_git_version(output)
                .unwrap_or_else(|error| panic!("{output:?} should be supported: {error}"));
        }
    }

    #[test]
    fn historical_scan_rejects_unrecognized_git_version_output() {
        let error = validate_historical_scan_git_version("vendor git build")
            .expect_err("unrecognized output should not bypass the compatibility preflight");

        assert!(matches!(
            &error,
            MigrationError::ExternalCommandOutput { command, .. }
                if command == "git --version"
        ));
        assert!(error.to_string().contains("Git 2.40.0 or newer"));
    }

    #[test]
    fn rejects_revision_syntax_as_history_ref_names() {
        for ref_name in ["", "main..feature", "HEAD^", "refs/heads/main\n"] {
            assert!(
                matches!(
                    validate_history_ref_name(ref_name),
                    Err(MigrationError::InvalidInput { .. })
                ),
                "{ref_name:?} should be rejected as unsafe revision syntax"
            );
        }

        validate_history_ref_name("refs/heads/feature/assets")
            .expect("normal full ref names should be accepted");
        validate_history_ref_name("feature/assets")
            .expect("normal branch names should be accepted");
    }

    #[test]
    fn selected_ref_pointer_scan_walks_history_and_respects_historical_attributes() {
        let repo = TempRepo::new();
        let old_object = test_lfs_object('a', 123);
        let new_object = test_lfs_object('b', 456);
        let non_lfs_object = test_lfs_object('c', 789);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n*.txt text\n");
        repo.write_file(
            "asset/old.bin",
            &LfsPointer::new(old_object.clone()).to_pointer_file(),
        );
        repo.write_file("asset/empty.bin", "");
        repo.write_file(
            "docs/pointer-example.txt",
            &LfsPointer::new(non_lfs_object).to_pointer_file(),
        );
        repo.commit_all("add historical pointer");
        repo.git(["rm", "asset/old.bin"]);
        repo.write_file(
            "asset/new.bin",
            &LfsPointer::new(new_object.clone()).to_pointer_file(),
        );
        repo.commit_all("replace pointer");

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["main"])
            .expect("selected ref scan should succeed");
        let objects = history_scan_objects(&scan.pointers);

        assert_eq!(scan.refs.len(), 1);
        assert_eq!(scan.refs[0].name, "main");
        assert!(objects.contains(&old_object));
        assert!(objects.contains(&new_object));
        assert!(!objects.contains(&test_lfs_object('c', 789)));
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/old.bin") && pointer.object == old_object
        }));
        assert!(
            scan.pointers
                .iter()
                .all(|pointer| pointer.relative_path != Path::new("asset/empty.bin"))
        );
        assert!(
            scan.pointers
                .iter()
                .all(|pointer| pointer.ref_name == "main")
        );
    }

    #[test]
    fn selected_ref_pointer_scan_reuses_unchanged_history_work() {
        let repo = TempRepo::new();
        let object = test_lfs_object('4', 444);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        for index in 0..128 {
            repo.write_file(
                format!("stable/file-{index:03}.txt"),
                &format!("stable fixture {index}\n"),
            );
        }
        repo.write_file("changing/revision.txt", "revision 0\n");
        repo.commit_all("add representative history fixture");

        for revision in 1..=16 {
            repo.write_file("changing/revision.txt", &format!("revision {revision}\n"));
            repo.commit_all(&format!("update revision {revision}"));
        }

        let (scan, metrics) =
            enumerate_selected_ref_lfs_pointers_with_metrics(repo.path(), ["main"])
                .expect("representative selected-ref scan should succeed");

        assert_eq!(scan.pointers.len(), 17);
        assert!(scan.pointers.iter().all(|pointer| pointer.object == object));
        assert_eq!(metrics.cat_file_processes, 1);
        assert_eq!(metrics.attribute_processes, 1);
        assert!(
            metrics.tree_entries_inspected < 256,
            "unchanged subtrees should be decoded once, got {metrics:?}"
        );
        assert!(
            metrics.blobs_inspected < 160,
            "unchanged blobs should be inspected once, got {metrics:?}"
        );
    }

    #[test]
    fn selected_ref_pointer_scan_rechecks_changed_historical_attributes() {
        let repo = TempRepo::new();
        let object = test_lfs_object('5', 555);

        repo.write_file(".gitattributes", "asset/*.bin -filter\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add pointer-shaped non-lfs blob");
        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.commit_all("track asset with lfs");
        let lfs_commit = repo.git_stdout(["rev-parse", "HEAD"]);
        repo.write_file(".gitattributes", "asset/*.bin -filter\n");
        repo.commit_all("stop tracking asset with lfs");

        let (scan, metrics) =
            enumerate_selected_ref_lfs_pointers_with_metrics(repo.path(), ["main"])
                .expect("historical attribute changes should be evaluated independently");

        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].commit, lfs_commit);
        assert_eq!(scan.pointers[0].object, object);
        assert_eq!(metrics.attribute_processes, 2);
    }

    #[test]
    fn selected_ref_pointer_scan_rejects_shallow_repository_history() {
        let repo = TempRepo::new();
        let object = test_lfs_object('a', 123);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object).to_pointer_file(),
        );
        repo.commit_all("add pointer at shallow boundary");
        repo.mark_head_as_shallow_boundary();

        let error = enumerate_selected_ref_lfs_pointers(repo.path(), ["main"])
            .expect_err("selected-ref history must reject a shallow repository");

        assert!(matches!(error, MigrationError::ShallowRepository { .. }));
        assert!(error.to_string().contains("git fetch --unshallow"));
    }

    #[test]
    fn selected_ref_pointer_scan_finds_branch_only_history() {
        let repo = TempRepo::new();
        let main_object = test_lfs_object('d', 111);
        let branch_object = test_lfs_object('e', 222);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/main.bin",
            &LfsPointer::new(main_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add main pointer");
        repo.git(["checkout", "-b", "feature/assets"]);
        repo.write_file(
            "asset/branch.bin",
            &LfsPointer::new(branch_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add branch pointer");
        repo.git(["checkout", "main"]);

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["feature/assets"])
            .expect("selected branch scan should succeed");
        let objects = history_scan_objects(&scan.pointers);

        assert!(objects.contains(&main_object));
        assert!(objects.contains(&branch_object));
        assert!(scan.pointers.iter().any(|pointer| {
            pointer.relative_path == Path::new("asset/branch.bin")
                && pointer.object == branch_object
        }));
    }

    #[test]
    fn selected_ref_pointer_scan_deduplicates_shared_history() {
        let repo = TempRepo::new();
        let object = test_lfs_object('8', 888);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/shared.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add shared pointer");
        repo.git(["tag", "v-shared"]);

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["main", "v-shared"])
            .expect("selected refs with shared history should scan once");

        assert_eq!(scan.refs.len(), 2);
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(
            scan.pointers[0].relative_path,
            Path::new("asset/shared.bin")
        );
        assert_eq!(scan.pointers[0].object, object);
    }

    #[test]
    fn all_fetched_ref_pointer_scan_includes_local_branches_and_tags() {
        let repo = TempRepo::new();
        let main_object = test_lfs_object('f', 333);
        let branch_object = test_lfs_object('1', 444);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/main.bin",
            &LfsPointer::new(main_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add main pointer");
        repo.git(["tag", "v-main"]);
        repo.git(["checkout", "-b", "feature/assets"]);
        repo.write_file(
            "asset/branch.bin",
            &LfsPointer::new(branch_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add branch pointer");
        repo.git(["checkout", "main"]);

        let scan = enumerate_all_fetched_ref_lfs_pointers(repo.path())
            .expect("all fetched refs scan should succeed");
        let ref_names = scan
            .refs
            .iter()
            .map(|scanned_ref| scanned_ref.name.as_str())
            .collect::<BTreeSet<_>>();
        let objects = history_scan_objects(&scan.pointers);

        assert!(ref_names.contains("refs/heads/main"));
        assert!(ref_names.contains("refs/heads/feature/assets"));
        assert!(ref_names.contains("refs/tags/v-main"));
        assert!(objects.contains(&main_object));
        assert!(objects.contains(&branch_object));
    }

    #[test]
    fn all_fetched_ref_pointer_scan_rejects_shallow_repository_history() {
        let repo = TempRepo::new();
        let object = test_lfs_object('f', 333);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object).to_pointer_file(),
        );
        repo.commit_all("add pointer at shallow boundary");
        repo.mark_head_as_shallow_boundary();

        let error = enumerate_all_fetched_ref_lfs_pointers(repo.path())
            .expect_err("all-ref history must reject a shallow repository");

        assert!(matches!(error, MigrationError::ShallowRepository { .. }));
        assert!(error.to_string().contains("git fetch --unshallow"));
    }

    #[test]
    fn all_fetched_ref_pointer_scan_skips_symbolic_remote_head() {
        let repo = TempRepo::new();
        let object = test_lfs_object('7', 777);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.commit_all("add model pointer");
        repo.git(["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git([
            "symbolic-ref",
            "refs/remotes/origin/HEAD",
            "refs/remotes/origin/main",
        ]);

        let scan = enumerate_all_fetched_ref_lfs_pointers(repo.path())
            .expect("all fetched refs scan should skip symbolic remote HEAD");
        let ref_names = scan
            .refs
            .iter()
            .map(|scanned_ref| scanned_ref.name.as_str())
            .collect::<BTreeSet<_>>();

        assert!(ref_names.contains("refs/remotes/origin/main"));
        assert!(!ref_names.contains("refs/remotes/origin/HEAD"));
        assert_eq!(scan.pointers.len(), 1);
        assert_eq!(scan.pointers[0].object, object);
    }

    #[test]
    fn all_fetched_ref_pointer_scan_excludes_other_remote_tracking_refs() {
        let repo = TempRepo::new();
        let origin_object = test_lfs_object('5', 555);
        let upstream_object = test_lfs_object('6', 666);

        repo.write_file(".gitattributes", "asset/*.bin filter=lfs\n");
        repo.write_file(
            "asset/origin.bin",
            &LfsPointer::new(origin_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add origin pointer");
        repo.git(["update-ref", "refs/remotes/origin/main", "HEAD"]);
        repo.git(["checkout", "-b", "upstream-history"]);
        repo.write_file(
            "asset/upstream.bin",
            &LfsPointer::new(upstream_object.clone()).to_pointer_file(),
        );
        repo.commit_all("add upstream pointer");
        repo.git(["update-ref", "refs/remotes/upstream/main", "HEAD"]);
        repo.git(["checkout", "main"]);
        repo.git(["branch", "-D", "upstream-history"]);

        let scan = enumerate_fetched_ref_lfs_pointers_for_remote(repo.path(), "origin")
            .expect("source-scoped all-ref scan should succeed");
        let ref_names = scan
            .refs
            .iter()
            .map(|scanned_ref| scanned_ref.name.as_str())
            .collect::<BTreeSet<_>>();
        let objects = history_scan_objects(&scan.pointers);

        assert!(ref_names.contains("refs/remotes/origin/main"));
        assert!(!ref_names.contains("refs/remotes/upstream/main"));
        assert!(objects.contains(&origin_object));
        assert!(!objects.contains(&upstream_object));
    }

    #[test]
    fn selected_ref_pointer_scan_skips_lfs_matching_gitlinks() {
        let repo = TempRepo::new();
        let object = test_lfs_object('9', 555);

        repo.write_file(
            ".gitattributes",
            "asset/* filter=lfs\nvendor/* filter=lfs\n",
        );
        repo.write_file(
            "asset/model.bin",
            &LfsPointer::new(object.clone()).to_pointer_file(),
        );
        repo.git(["add", ".gitattributes", "asset/model.bin"]);
        repo.git([
            "update-index",
            "--add",
            "--cacheinfo",
            "160000",
            "1111111111111111111111111111111111111111",
            "vendor/tooling",
        ]);
        repo.git(["commit", "-m", "add lfs pointer and gitlink"]);

        let scan = enumerate_selected_ref_lfs_pointers(repo.path(), ["main"])
            .expect("gitlinks matching LFS attributes should be ignored");
        let objects = history_scan_objects(&scan.pointers);

        assert!(objects.contains(&object));
        assert!(
            scan.pointers
                .iter()
                .all(|pointer| pointer.relative_path != Path::new("vendor/tooling"))
        );
    }

}
