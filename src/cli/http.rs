//! Shared bounded networking and external-process diagnostics for CLI commands.

use super::*;

pub(super) fn resolve_socket_addresses_with_timeout(
    host: String,
    port: u16,
) -> CliResult<Vec<SocketAddr>> {
    let (sender, receiver) = mpsc::sync_channel(1);
    let thread_host = host.clone();
    std::thread::Builder::new()
        .name("lfscloud-status-resolver".to_owned())
        .spawn(move || {
            let result = (thread_host.as_str(), port)
                .to_socket_addrs()
                .map(|addresses| addresses.collect::<Vec<_>>())
                .map_err(|source| CliError::Io {
                    context: format!("failed to resolve {thread_host}:{port}"),
                    source,
                });
            let _ = sender.send(result);
        })
        .map_err(|source| CliError::Io {
            context: format!("failed to start resolver for {host}:{port}"),
            source,
        })?;

    receiver
        .recv_timeout(STATUS_SERVER_CONNECT_TIMEOUT)
        .map_err(|_| CliError::Io {
            context: format!("timed out resolving {host}:{port}"),
            source: io::Error::new(io::ErrorKind::TimedOut, "DNS resolution timed out"),
        })?
}

pub(super) fn auth_url_for_server(server_url: &str, route_path: &str) -> CliResult<String> {
    // Login and logout callers obtain this base from `LfsInitRoute`, which has
    // already enforced the CLI's insecure-HTTP opt-in. Revalidation accepts
    // HTTP here so loopback and explicitly opted-in LAN routes remain usable.
    let mut auth_url = crate::init::validate_server_url(server_url, true)?;
    append_url_path_segments(&mut auth_url, route_path)?;

    Ok(auth_url.to_string())
}

pub(super) fn append_url_path_segments(url: &mut Url, route_path: &str) -> CliResult<()> {
    let mut segments = url
        .path_segments_mut()
        .map_err(|()| CliError::InvalidArguments {
            message: "URL cannot be used as a route base".to_owned(),
        })?;
    segments.extend(route_path.split('/').filter(|segment| !segment.is_empty()));

    Ok(())
}

pub(super) fn redirect_free_http_client(context: &'static str) -> CliResult<Client> {
    // Token-bearing requests must never forward credentials to a redirect
    // target, even when that target shares the original host.
    Client::builder()
        .redirect(Policy::none())
        .build()
        .map_err(|source| CliError::Io {
            context: context.to_owned(),
            source: io::Error::other(source),
        })
}

pub(super) fn block_on_reqwest<T>(
    future: impl Future<Output = Result<T, reqwest::Error>>,
    context: &'static str,
) -> CliResult<T> {
    // The synchronous CLI handlers run inside the process Tokio runtime; move
    // their reqwest futures through its handle without nesting another runtime.
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(future)).map_err(
        |source| CliError::Io {
            context: context.to_owned(),
            source: io::Error::other(source),
        },
    )
}

pub(super) fn sanitized_external_stderr(stderr: &[u8]) -> SanitizedMessage {
    const MAX_EXTERNAL_STDERR_LEN: usize = 1024;

    let mut message = String::from_utf8_lossy(stderr).into_owned();
    message = message.replace(['\r', '\n'], " ");
    truncate_with_ellipsis(&mut message, MAX_EXTERNAL_STDERR_LEN);
    let message = message.trim();

    if message.is_empty() {
        SanitizedMessage::new("<no stderr>")
    } else {
        SanitizedMessage::new(message.to_owned())
    }
}

pub(super) fn sanitized_external_failure_output(stderr: &[u8], stdout: &[u8]) -> SanitizedMessage {
    if stdout.is_empty() {
        return sanitized_external_stderr(stderr);
    }

    let mut combined = Vec::with_capacity(stderr.len() + stdout.len() + 9);
    combined.extend_from_slice(stderr);
    if !stderr.is_empty() {
        combined.push(b'\n');
    }
    combined.extend_from_slice(b"stdout: ");
    combined.extend_from_slice(stdout);

    sanitized_external_stderr(&combined)
}

pub(super) fn output_error(source: io::Error) -> CliError {
    CliError::Io {
        context: "failed to write command output".to_owned(),
        source,
    }
}
