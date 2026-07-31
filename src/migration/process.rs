// This file is included by `mod.rs` so the migration API remains in one module.

fn detect_worktree_root(start_dir: &Path) -> MigrationResult<PathBuf> {
    let output = run_git(start_dir, ["rev-parse", "--show-toplevel"])?;
    if !output.status.success() {
        return Err(MigrationError::NotGitRepository {
            path: start_dir.to_path_buf(),
        });
    }

    let stdout = output_stdout(output, "git rev-parse --show-toplevel")?;
    Ok(PathBuf::from(stdout.trim_end_matches(['\n', '\r'])))
}

fn git_config_get<const N: usize>(
    worktree_root: &Path,
    args: [&str; N],
) -> MigrationResult<Option<String>> {
    let command_name = args.join(" ");
    let output = run_git(worktree_root, args)?;
    optional_stdout(output, &format!("git {command_name}"))
}

fn git_config_get_os<const N: usize>(
    worktree_root: &Path,
    args: [&OsStr; N],
    command_name: &str,
) -> MigrationResult<Option<String>> {
    let output = run_git_os(worktree_root, args, command_name)?;
    optional_stdout(output, command_name)
}

fn run_git<const N: usize>(current_dir: &Path, args: [&str; N]) -> MigrationResult<Output> {
    let command_name = format!("git {}", args.join(" "));
    let mut command = read_only_git_command();
    command.args(args).current_dir(current_dir);
    run_bounded_command_output(&mut command, &command_name, MAX_MIGRATION_GIT_OUTPUT_BYTES)
}

fn run_git_os<I, S>(current_dir: &Path, args: I, command_name: &str) -> MigrationResult<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    run_git_os_with_limit(
        current_dir,
        args,
        command_name,
        MAX_MIGRATION_GIT_OUTPUT_BYTES,
    )
}

fn run_git_os_with_limit<I, S>(
    current_dir: &Path,
    args: I,
    command_name: &str,
    stdout_limit: usize,
) -> MigrationResult<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = read_only_git_command();
    command.args(args).current_dir(current_dir);
    run_bounded_command_output(&mut command, command_name, stdout_limit)
}

fn read_only_git_command() -> Command {
    let mut command = Command::new("git");
    // Promisor repositories may fetch missing objects from their remote during
    // otherwise read-only commands. Migration discovery must never transfer
    // data, especially when it is building a dry-run report.
    command.env(GIT_NO_LAZY_FETCH_ENV, "1");
    command
}

/// Runs a migration Git command without allowing either captured pipe to grow
/// beyond its declared boundary.
///
/// stdout and stderr are drained concurrently so a noisy diagnostic stream
/// cannot deadlock a command whose primary output is still being consumed. A
/// reader reports overflow as soon as it sees the first excess byte; the parent
/// then stops the whole owned process tree before returning the bounded prefix.
fn run_bounded_command_output(
    command: &mut Command,
    command_name: &str,
    stdout_limit: usize,
) -> MigrationResult<Output> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_process_tree(command);

    let mut child = command.spawn().map_err(|source| MigrationError::Io {
        context: format!("failed to start {command_name}"),
        source,
    })?;
    let output = wait_for_child(
        &mut child,
        command_name,
        ChildProcessOptions {
            timeout: None,
            stdout: PipeCapture::HardLimit {
                limit: stdout_limit,
            },
            stderr: PipeCapture::HardLimit {
                limit: MAX_MIGRATION_GIT_OUTPUT_BYTES,
            },
            inherited_pipe_is_error: true,
        },
    )
    .map_err(|error| child_process_migration_error(error, command_name))?;

    Ok(Output {
        status: output.status,
        stdout: output.stdout,
        stderr: output.stderr,
    })
}

fn child_process_migration_error(error: ChildProcessError, command_name: &str) -> MigrationError {
    match error {
        ChildProcessError::Io { context, source } => MigrationError::Io { context, source },
        ChildProcessError::TimedOut {
            timeout, stderr, ..
        } => MigrationError::ExternalCommand {
            command: command_name.to_owned(),
            status: format!("timed out after {} seconds", timeout.as_secs()),
            stderr: SanitizedMessage::new(truncated_lossy_message(
                &stderr,
                MAX_MIGRATION_GIT_OUTPUT_BYTES,
            )),
        },
        ChildProcessError::OutputLimit { stream, limit } => MigrationError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new(format!("git {stream} exceeded the {limit}-byte limit")),
        },
        ChildProcessError::InheritedPipe => MigrationError::Io {
            context: format!("timed out draining output from {command_name}"),
            source: io::Error::new(
                io::ErrorKind::TimedOut,
                "git output pipes remained open after process exit",
            ),
        },
    }
}

fn required_success_stdout(output: Output, command_name: &str) -> MigrationResult<String> {
    required_success_stdout_with_limit(output, command_name, MAX_MIGRATION_GIT_OUTPUT_BYTES)
}

fn required_success_stdout_with_limit(
    output: Output,
    command_name: &str,
    limit: usize,
) -> MigrationResult<String> {
    if !output.status.success() {
        return Err(command_error(command_name, output.status, &output.stderr));
    }

    output_stdout_with_limit(output, command_name, limit)
}

fn optional_stdout(output: Output, command_name: &str) -> MigrationResult<Option<String>> {
    if !output.status.success() {
        if output.status.code() == Some(1) && output.stderr.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }

        return Err(command_error(command_name, output.status, &output.stderr));
    }

    output_stdout(output, command_name).map(|stdout| {
        let trimmed = stdout.trim_end_matches(['\n', '\r']);
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

fn output_stdout(output: Output, command_name: &str) -> MigrationResult<String> {
    output_stdout_with_limit(output, command_name, MAX_MIGRATION_GIT_OUTPUT_BYTES)
}

fn output_stdout_with_limit(
    output: Output,
    command_name: &str,
    limit: usize,
) -> MigrationResult<String> {
    if output.stdout.len() > limit {
        return Err(MigrationError::ExternalCommandOutput {
            command: command_name.to_owned(),
            message: SanitizedMessage::new("git returned too much output"),
        });
    }

    String::from_utf8(output.stdout).map_err(|_| MigrationError::ExternalCommandOutput {
        command: command_name.to_owned(),
        message: SanitizedMessage::new("git returned non-UTF-8 output"),
    })
}

fn command_error(command: &str, status: ExitStatus, stderr: &[u8]) -> MigrationError {
    MigrationError::ExternalCommand {
        command: command.to_owned(),
        status: command_status_text(status),
        stderr: SanitizedMessage::new(truncated_lossy_message(
            stderr,
            MAX_MIGRATION_GIT_OUTPUT_BYTES,
        )),
    }
}

fn git_lfs_probe_diagnostic(output: &Output) -> String {
    let stderr = truncated_lossy_message(&output.stderr, MAX_MIGRATION_GIT_OUTPUT_BYTES);
    if stderr.trim().is_empty() {
        format!(
            "git lfs version exited with status {}",
            command_status_text(output.status)
        )
    } else {
        stderr.trim().to_owned()
    }
}

fn first_non_empty_line(value: &str) -> Option<&str> {
    value.lines().find(|line| !line.trim().is_empty())
}

fn is_regular_file_without_following_symlinks(path: &Path) -> MigrationResult<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.file_type().is_file()),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(MigrationError::Io {
            context: format!("failed to inspect {}", path.display()),
            source,
        }),
    }
}

fn repo_relative_path_from_git_output(path: &str) -> MigrationResult<PathBuf> {
    let path = PathBuf::from(path);
    let is_safe_relative_path = !path.is_absolute()
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_) | Component::CurDir));

    if is_safe_relative_path {
        Ok(path)
    } else {
        Err(MigrationError::ExternalCommandOutput {
            command: "git ls-files".to_owned(),
            message: SanitizedMessage::new("git returned a path outside the worktree"),
        })
    }
}

#[cfg(all(test, unix))]
mod process_tests {
    use super::test_support::*;

    #[test]
    fn bounded_git_output_stops_a_runaway_process_tree_on_overflow() {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg("(while :; do printf '0123456789abcdef'; done) & child=$!; wait \"$child\"");
        let started_at = Instant::now();

        let error =
            super::run_bounded_command_output(&mut command, "git runaway-output-test", 4 * 1024)
                .expect_err("runaway output should cross the hard limit");

        assert!(
            started_at.elapsed() < Duration::from_secs(5),
            "overflow cleanup should stop the command process tree promptly"
        );
        assert!(matches!(
            error,
            MigrationError::ExternalCommandOutput { command, message }
                if command == "git runaway-output-test"
                    && message.as_str().contains("stdout")
                    && message.as_str().contains("4096-byte limit")
        ));
    }

    #[test]
    fn bounded_git_output_drains_stdout_and_stderr_concurrently() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "i=0; while [ \"$i\" -lt 8192 ]; do printf '0123456789abcdef' >&2; i=$((i + 1)); done; i=0; while [ \"$i\" -lt 8192 ]; do printf 'fedcba9876543210'; i=$((i + 1)); done",
        );

        let output = super::run_bounded_command_output(
            &mut command,
            "git concurrent-output-test",
            MAX_MIGRATION_GIT_OUTPUT_BYTES,
        )
        .expect("bounded runner should drain both pipes without deadlocking");

        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 128 * 1024);
        assert_eq!(output.stderr.len(), 128 * 1024);
    }

}
