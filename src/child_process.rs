//! Shared child-process output and process-tree handling.
//!
//! Git and Git LFS may start helpers that inherit their output pipes. This
//! module gives every caller the same bounded-drain and recursive-termination
//! behavior while leaving domain-specific error rendering to the caller.

use std::{
    fmt,
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

const OUTPUT_DRAIN_GRACE: Duration = Duration::from_millis(500);
const OUTPUT_DRAIN_AFTER_STOP: Duration = Duration::from_secs(1);
const PROCESS_POLL_INTERVAL: Duration = Duration::from_millis(10);

/// Policy applied to one captured child pipe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PipeCapture {
    /// Drain and retain the complete pipe.
    Unlimited,
    /// Retain at most `limit` bytes while continuing to drain the pipe.
    Truncate { limit: usize },
    /// Stop the process tree as soon as the pipe exceeds `limit` bytes.
    HardLimit { limit: usize },
}

/// Options for waiting on an already-spawned child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ChildProcessOptions {
    /// Optional maximum lifetime for the direct child.
    pub(crate) timeout: Option<Duration>,
    /// Standard-output capture policy.
    pub(crate) stdout: PipeCapture,
    /// Standard-error capture policy.
    pub(crate) stderr: PipeCapture,
    /// Whether successfully stopping descendants that retain output pipes
    /// after the direct child exits must still fail the command.
    pub(crate) inherited_pipe_is_error: bool,
}

/// Captured output from a completed child.
pub(crate) struct ChildProcessOutput {
    /// Direct child exit status.
    pub(crate) status: ExitStatus,
    /// Captured standard output.
    pub(crate) stdout: Vec<u8>,
    /// Captured standard error.
    pub(crate) stderr: Vec<u8>,
}

impl fmt::Debug for ChildProcessOutput {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChildProcessOutput")
            .field("status", &self.status)
            .field("stdout_len", &self.stdout.len())
            .field("stderr_len", &self.stderr.len())
            .finish()
    }
}

/// Failure while waiting for or draining a child process tree.
pub(crate) enum ChildProcessError {
    /// A process or pipe operation failed.
    Io {
        /// Operation being attempted.
        context: String,
        /// Underlying I/O error.
        source: io::Error,
    },
    /// The child exceeded its configured lifetime.
    TimedOut {
        /// Configured timeout.
        timeout: Duration,
        /// Output retained before termination.
        stdout: Vec<u8>,
        /// Error output retained before termination.
        stderr: Vec<u8>,
    },
    /// One captured stream exceeded its hard limit.
    OutputLimit {
        /// Human-readable stream name.
        stream: &'static str,
        /// Configured byte limit.
        limit: usize,
    },
    /// A descendant retained an output pipe after the direct child exited.
    InheritedPipe,
}

impl fmt::Debug for ChildProcessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { context, source } => formatter
                .debug_struct("Io")
                .field("context", context)
                .field("source", source)
                .finish(),
            Self::TimedOut {
                timeout,
                stdout,
                stderr,
            } => formatter
                .debug_struct("TimedOut")
                .field("timeout", timeout)
                .field("stdout_len", &stdout.len())
                .field("stderr_len", &stderr.len())
                .finish(),
            Self::OutputLimit { stream, limit } => formatter
                .debug_struct("OutputLimit")
                .field("stream", stream)
                .field("limit", limit)
                .finish(),
            Self::InheritedPipe => formatter.write_str("InheritedPipe"),
        }
    }
}

impl ChildProcessError {
    fn io(context: impl Into<String>, source: io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }
}

#[derive(Debug)]
struct PipeReadResult {
    bytes: Vec<u8>,
    exceeded_limit: Option<usize>,
}

#[derive(Debug)]
enum PipeEvent {
    Stdout(io::Result<PipeReadResult>),
    Stderr(io::Result<PipeReadResult>),
}

/// Configures a command so recursive cleanup owns helpers that it starts.
pub(crate) fn configure_process_tree(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;

        command.process_group(0);
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// Waits for a configured process-tree child while draining its output pipes.
///
/// The child must have been spawned from a [`Command`] passed to
/// [`configure_process_tree`]. Process-tree termination uses the direct
/// child's process identifier as the owned process-group identifier on Unix,
/// so calling this function for any other child is not safe.
pub(crate) fn wait_for_child(
    child: &mut Child,
    command_name: &str,
    options: ChildProcessOptions,
) -> Result<ChildProcessOutput, ChildProcessError> {
    let (sender, receiver) = mpsc::channel();
    let mut stdout_reader = child.stdout.take().map(|stdout| {
        let sender = sender.clone();
        thread::spawn(move || {
            let _ = sender.send(PipeEvent::Stdout(read_pipe(stdout, options.stdout)));
        })
    });
    let mut stderr_reader = child.stderr.take().map(|stderr| {
        let sender = sender.clone();
        thread::spawn(move || {
            let _ = sender.send(PipeEvent::Stderr(read_pipe(stderr, options.stderr)));
        })
    });
    drop(sender);

    let deadline = options.timeout.map(|timeout| Instant::now() + timeout);
    let mut status = None;
    let mut drain_deadline = None;
    let mut stdout = stdout_reader.is_none().then(Vec::new);
    let mut stderr = stderr_reader.is_none().then(Vec::new);

    loop {
        while let Ok(event) = receiver.try_recv() {
            if let Err(error) = accept_pipe_event(event, command_name, &mut stdout, &mut stderr) {
                terminate_process_tree(child, command_name)?;
                let _ = collect_pipe_events(
                    &receiver,
                    &mut stdout,
                    &mut stderr,
                    OUTPUT_DRAIN_AFTER_STOP,
                    command_name,
                );
                join_reader(&mut stdout_reader, "stdout", command_name)?;
                join_reader(&mut stderr_reader, "stderr", command_name)?;
                return Err(error);
            }
        }

        if status.is_none() {
            status = child.try_wait().map_err(|source| {
                ChildProcessError::io(format!("failed to wait for {command_name}"), source)
            })?;
            if status.is_some() {
                drain_deadline = Some(Instant::now() + OUTPUT_DRAIN_GRACE);
            }
        }

        if let Some(status) = status.filter(|_| stdout.is_some() && stderr.is_some()) {
            join_reader(&mut stdout_reader, "stdout", command_name)?;
            join_reader(&mut stderr_reader, "stderr", command_name)?;
            return Ok(ChildProcessOutput {
                status,
                stdout: stdout.take().expect("stdout was checked above"),
                stderr: stderr.take().expect("stderr was checked above"),
            });
        }

        if status.is_none() && deadline.is_some_and(|deadline| Instant::now() >= deadline) {
            terminate_process_tree(child, command_name)?;
            let _ = collect_pipe_events(
                &receiver,
                &mut stdout,
                &mut stderr,
                OUTPUT_DRAIN_AFTER_STOP,
                command_name,
            );
            join_reader(&mut stdout_reader, "stdout", command_name)?;
            join_reader(&mut stderr_reader, "stderr", command_name)?;
            return Err(ChildProcessError::TimedOut {
                timeout: options
                    .timeout
                    .expect("a reached deadline always has a configured timeout"),
                stdout: stdout.unwrap_or_default(),
                stderr: stderr.unwrap_or_default(),
            });
        }

        if status.is_some_and(|_| drain_deadline.is_some_and(|end| Instant::now() >= end)) {
            // The direct child exited, so any open output pipe belongs to a
            // descendant in the command's process group.
            stop_process_tree(child);
            let drain_error = collect_pipe_events(
                &receiver,
                &mut stdout,
                &mut stderr,
                OUTPUT_DRAIN_AFTER_STOP,
                command_name,
            );
            join_completed_reader(&mut stdout_reader, stdout.as_ref(), "stdout", command_name)?;
            join_completed_reader(&mut stderr_reader, stderr.as_ref(), "stderr", command_name)?;

            if let Some(error) = drain_error {
                return Err(error);
            }
            if stdout.is_none() || stderr.is_none() || options.inherited_pipe_is_error {
                return Err(ChildProcessError::InheritedPipe);
            }
        }

        thread::sleep(PROCESS_POLL_INTERVAL);
    }
}

fn accept_pipe_event(
    event: PipeEvent,
    command_name: &str,
    stdout: &mut Option<Vec<u8>>,
    stderr: &mut Option<Vec<u8>>,
) -> Result<(), ChildProcessError> {
    let (stream, result, destination) = match event {
        PipeEvent::Stdout(result) => ("stdout", result, stdout),
        PipeEvent::Stderr(result) => ("stderr", result, stderr),
    };
    let output = match result {
        Ok(output) => output,
        Err(source) => {
            return Err(ChildProcessError::io(
                format!("failed to read {stream} from {command_name}"),
                source,
            ));
        }
    };
    if let Some(limit) = output.exceeded_limit {
        return Err(ChildProcessError::OutputLimit { stream, limit });
    }
    *destination = Some(output.bytes);
    Ok(())
}

fn read_pipe(mut pipe: impl Read, policy: PipeCapture) -> io::Result<PipeReadResult> {
    let capacity = match policy {
        PipeCapture::Unlimited => 8192,
        PipeCapture::Truncate { limit } | PipeCapture::HardLimit { limit } => limit.min(8192),
    };
    let mut bytes = Vec::with_capacity(capacity);
    let mut buffer = [0_u8; 8192];

    loop {
        let count = pipe.read(&mut buffer)?;
        if count == 0 {
            return Ok(PipeReadResult {
                bytes,
                exceeded_limit: None,
            });
        }

        match policy {
            PipeCapture::Unlimited => bytes.extend_from_slice(&buffer[..count]),
            PipeCapture::Truncate { limit } => {
                let remaining = limit.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buffer[..count.min(remaining)]);
            }
            PipeCapture::HardLimit { limit } => {
                let remaining = limit.saturating_sub(bytes.len());
                bytes.extend_from_slice(&buffer[..count.min(remaining)]);
                if count > remaining {
                    return Ok(PipeReadResult {
                        bytes,
                        exceeded_limit: Some(limit),
                    });
                }
            }
        }
    }
}

fn collect_pipe_events(
    receiver: &mpsc::Receiver<PipeEvent>,
    stdout: &mut Option<Vec<u8>>,
    stderr: &mut Option<Vec<u8>>,
    timeout: Duration,
    command_name: &str,
) -> Option<ChildProcessError> {
    let deadline = Instant::now() + timeout;
    let mut first_error = None;
    while (stdout.is_none() || stderr.is_none()) && Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = receiver.recv_timeout(remaining) else {
            break;
        };
        match event {
            PipeEvent::Stdout(Ok(output)) => {
                if let Some(limit) = output.exceeded_limit {
                    first_error.get_or_insert(ChildProcessError::OutputLimit {
                        stream: "stdout",
                        limit,
                    });
                }
                *stdout = Some(output.bytes);
            }
            PipeEvent::Stderr(Ok(output)) => {
                if let Some(limit) = output.exceeded_limit {
                    first_error.get_or_insert(ChildProcessError::OutputLimit {
                        stream: "stderr",
                        limit,
                    });
                }
                *stderr = Some(output.bytes);
            }
            PipeEvent::Stdout(Err(source)) => {
                *stdout = Some(Vec::new());
                first_error.get_or_insert_with(|| {
                    ChildProcessError::io(
                        format!("failed to read stdout from {command_name}"),
                        source,
                    )
                });
            }
            PipeEvent::Stderr(Err(source)) => {
                *stderr = Some(Vec::new());
                first_error.get_or_insert_with(|| {
                    ChildProcessError::io(
                        format!("failed to read stderr from {command_name}"),
                        source,
                    )
                });
            }
        }
    }
    first_error
}

fn join_reader(
    reader: &mut Option<thread::JoinHandle<()>>,
    stream: &str,
    command_name: &str,
) -> Result<(), ChildProcessError> {
    let Some(reader) = reader.take() else {
        return Ok(());
    };
    reader.join().map_err(|_| {
        ChildProcessError::io(
            format!("{stream} reader thread panicked for {command_name}"),
            io::Error::other("pipe reader thread panicked"),
        )
    })
}

fn join_completed_reader(
    reader: &mut Option<thread::JoinHandle<()>>,
    output: Option<&Vec<u8>>,
    stream: &str,
    command_name: &str,
) -> Result<(), ChildProcessError> {
    if output.is_some() {
        join_reader(reader, stream, command_name)?;
    }
    Ok(())
}

/// Stops and reaps a child spawned from a configured process-tree command.
pub(crate) fn terminate_process_tree(
    child: &mut Child,
    command_name: &str,
) -> Result<(), ChildProcessError> {
    stop_process_tree(child);
    if child
        .try_wait()
        .map_err(|source| {
            ChildProcessError::io(format!("failed to wait for stopped {command_name}"), source)
        })?
        .is_none()
    {
        child.kill().map_err(|source| {
            ChildProcessError::io(format!("failed to stop {command_name}"), source)
        })?;
        child.wait().map_err(|source| {
            ChildProcessError::io(format!("failed to reap stopped {command_name}"), source)
        })?;
    }
    Ok(())
}

#[cfg(unix)]
fn stop_process_tree(child: &Child) {
    signal_process_group("TERM", child.id());
    thread::sleep(Duration::from_millis(50));
    signal_process_group("KILL", child.id());
}

#[cfg(unix)]
fn signal_process_group(signal: &str, process_group_id: u32) {
    let _ = Command::new("kill")
        .arg(format!("-{signal}"))
        .arg(format!("-{process_group_id}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(windows)]
fn stop_process_tree(child: &Child) {
    let _ = Command::new("taskkill")
        .args(["/T", "/F", "/PID", &child.id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(any(unix, windows)))]
fn stop_process_tree(_child: &Child) {}

#[cfg(test)]
mod tests {
    use std::{
        io,
        process::{Command, Stdio},
        sync::mpsc,
        time::{Duration, Instant},
    };

    use super::{
        ChildProcessError, ChildProcessOptions, PipeCapture, PipeEvent, collect_pipe_events,
        configure_process_tree, read_pipe, wait_for_child,
    };

    const PROCESS_TREE_HELPER_TEST: &str = "child_process::tests::process_tree_pipe_holding_helper";
    const PROCESS_TREE_DESCENDANT_TEST: &str =
        "child_process::tests::process_tree_pipe_holding_descendant";
    const PROCESS_TREE_MODE_ENV: &str = "LFSCLOUD_CHILD_PROCESS_TEST_MODE";

    #[test]
    fn truncated_pipe_keeps_draining_but_retains_only_the_limit() {
        let output = read_pipe(&b"abcdef"[..], PipeCapture::Truncate { limit: 3 })
            .expect("pipe should be readable");

        assert_eq!(output.bytes, b"abc");
        assert_eq!(output.exceeded_limit, None);
    }

    #[test]
    fn hard_limited_pipe_reports_the_first_excess_byte() {
        let output = read_pipe(&b"abcdef"[..], PipeCapture::HardLimit { limit: 3 })
            .expect("pipe should be readable");

        assert_eq!(output.bytes, b"abc");
        assert_eq!(output.exceeded_limit, Some(3));
    }

    #[test]
    fn drain_reports_completed_reader_errors_without_calling_them_open_pipes() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PipeEvent::Stdout(Err(io::Error::other(
                "injected read failure",
            ))))
            .expect("read failure should be sent");
        drop(sender);
        let mut stdout = None;
        let mut stderr = Some(Vec::new());

        let error = collect_pipe_events(
            &receiver,
            &mut stdout,
            &mut stderr,
            Duration::from_secs(1),
            "test helper",
        )
        .expect("reader failure should be retained");

        assert!(stdout.is_some());
        assert!(matches!(
            error,
            ChildProcessError::Io { context, .. }
                if context == "failed to read stdout from test helper"
        ));
    }

    #[test]
    fn drain_reports_descendant_output_that_exceeds_a_hard_limit() {
        let (sender, receiver) = mpsc::channel();
        sender
            .send(PipeEvent::Stdout(Ok(super::PipeReadResult {
                bytes: b"abc".to_vec(),
                exceeded_limit: Some(3),
            })))
            .expect("completed descendant output should be sent");
        drop(sender);
        let mut stdout = None;
        let mut stderr = Some(Vec::new());

        let error = collect_pipe_events(
            &receiver,
            &mut stdout,
            &mut stderr,
            Duration::from_secs(1),
            "exited test helper",
        )
        .expect("post-exit drain should retain the hard-limit failure");

        assert_eq!(stdout, Some(b"abc".to_vec()));
        assert!(matches!(
            error,
            ChildProcessError::OutputLimit {
                stream: "stdout",
                limit: 3
            }
        ));
    }

    #[test]
    fn output_debug_reports_lengths_without_exposing_captured_bytes() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--ignored", "--exact", PROCESS_TREE_HELPER_TEST])
            .env(PROCESS_TREE_MODE_ENV, "complete")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn().expect("helper should start");

        let output = wait_for_child(
            &mut child,
            "test helper",
            ChildProcessOptions {
                timeout: Some(Duration::from_secs(5)),
                stdout: PipeCapture::Unlimited,
                stderr: PipeCapture::Unlimited,
                inherited_pipe_is_error: true,
            },
        )
        .expect("helper should complete");
        let debug = format!("{output:?}");

        assert!(debug.contains("stdout_len"));
        assert!(!debug.contains("stdout-secret"));
        assert!(!debug.contains("stderr-secret"));
    }

    #[test]
    fn timeout_retains_output_without_exposing_it_through_debug() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--ignored", "--exact", PROCESS_TREE_HELPER_TEST])
            .env(PROCESS_TREE_MODE_ENV, "timeout")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn().expect("helper should start");

        let error = wait_for_child(
            &mut child,
            "test helper",
            ChildProcessOptions {
                timeout: Some(Duration::from_millis(100)),
                stdout: PipeCapture::Unlimited,
                stderr: PipeCapture::Unlimited,
                inherited_pipe_is_error: true,
            },
        )
        .expect_err("helper should time out");
        let debug = format!("{error:?}");

        assert!(matches!(
            error,
            ChildProcessError::TimedOut {
                ref stdout,
                ref stderr,
                ..
            } if stdout.windows(b"stdout-secret\n".len()).any(|bytes| bytes == b"stdout-secret\n")
                && stderr
                    .windows(b"stderr-secret\n".len())
                    .any(|bytes| bytes == b"stderr-secret\n")
        ));
        assert!(!debug.contains("stdout-secret"));
        assert!(!debug.contains("stderr-secret"));
    }

    #[test]
    fn inherited_pipe_policy_distinguishes_recovered_output() {
        for inherited_pipe_is_error in [false, true] {
            let mut command = Command::new(std::env::current_exe().expect("test executable"));
            command
                .args(["--ignored", "--exact", PROCESS_TREE_HELPER_TEST])
                .env(PROCESS_TREE_MODE_ENV, "descendant")
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            configure_process_tree(&mut command);
            let mut child = command.spawn().expect("helper should start");
            let started = Instant::now();

            let result = wait_for_child(
                &mut child,
                "test helper",
                ChildProcessOptions {
                    timeout: Some(Duration::from_secs(5)),
                    stdout: PipeCapture::Unlimited,
                    stderr: PipeCapture::Unlimited,
                    inherited_pipe_is_error,
                },
            );

            assert!(started.elapsed() < Duration::from_secs(5));
            if inherited_pipe_is_error {
                assert!(matches!(result, Err(ChildProcessError::InheritedPipe)));
            } else {
                assert!(
                    result
                        .expect("cleanup should recover output")
                        .status
                        .success()
                );
            }
        }
    }

    #[test]
    fn hard_limit_reports_configured_limit_after_stopping_the_tree() {
        let mut command = Command::new(std::env::current_exe().expect("test executable"));
        command
            .args(["--ignored", "--exact", PROCESS_TREE_HELPER_TEST])
            .env(PROCESS_TREE_MODE_ENV, "timeout")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_tree(&mut command);
        let mut child = command.spawn().expect("helper should start");

        let error = wait_for_child(
            &mut child,
            "test helper",
            ChildProcessOptions {
                timeout: Some(Duration::from_secs(5)),
                stdout: PipeCapture::HardLimit { limit: 3 },
                stderr: PipeCapture::Unlimited,
                inherited_pipe_is_error: true,
            },
        )
        .expect_err("stdout should exceed the hard limit");

        assert!(matches!(
            error,
            ChildProcessError::OutputLimit {
                stream: "stdout",
                limit: 3
            }
        ));
    }

    #[test]
    #[ignore = "invoked as a platform-native child-process helper"]
    #[allow(
        clippy::zombie_processes,
        reason = "the helper must exit while its descendant retains the inherited pipes"
    )]
    fn process_tree_pipe_holding_helper() {
        use std::io::Write as _;

        let mode = std::env::var(PROCESS_TREE_MODE_ENV).unwrap_or_default();
        std::io::stdout()
            .write_all(b"stdout-secret\n")
            .expect("stdout should be writable");
        std::io::stderr()
            .write_all(b"stderr-secret\n")
            .expect("stderr should be writable");
        if mode == "timeout" {
            std::thread::sleep(Duration::from_secs(30));
        } else if mode == "descendant" {
            Command::new(std::env::current_exe().expect("test executable"))
                .args(["--ignored", "--exact", PROCESS_TREE_DESCENDANT_TEST])
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("descendant should start");
        }
    }

    #[test]
    #[ignore = "invoked as a platform-native child-process descendant"]
    fn process_tree_pipe_holding_descendant() {
        std::thread::sleep(Duration::from_secs(30));
    }
}
