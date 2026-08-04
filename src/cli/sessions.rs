//! Session encryption-key maintenance commands.

use super::*;
use crate::MetadataDatabase;

const ROTATION_WARNING: &str = "WARNING: generating a new session encryption key will invalidate all current LFS Cloud sessions. Users will need to log in again.\nType 'yes' to continue: ";

pub(super) fn run_sessions_to_stdio(
    command: SessionsCommand,
    config_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let stdin = io::stdin();
    let mut input = stdin.lock();
    let mut output = io::stdout().lock();
    run_sessions_with_io(command, config_path, &mut input, &mut output).map_err(anyhow::Error::from)
}

fn run_sessions_with_io<R, W>(
    command: SessionsCommand,
    config_path: Option<PathBuf>,
    input: &mut R,
    output: &mut W,
) -> CliResult<()>
where
    R: BufRead,
    W: Write,
{
    match command.action {
        SessionsAction::GenerateKey => {
            output
                .write_all(ROTATION_WARNING.as_bytes())
                .map_err(|source| CliError::Io {
                    context: "failed to write session-key rotation warning".to_owned(),
                    source,
                })?;
            output.flush().map_err(|source| CliError::Io {
                context: "failed to flush session-key rotation warning".to_owned(),
                source,
            })?;

            let mut confirmation = String::new();
            input
                .take(32)
                .read_line(&mut confirmation)
                .map_err(|source| CliError::Io {
                    context: "failed to read session-key rotation confirmation".to_owned(),
                    source,
                })?;
            if confirmation.trim() != "yes" {
                writeln!(output, "Session encryption key was not changed.").map_err(|source| {
                    CliError::Io {
                        context: "failed to report cancelled session-key rotation".to_owned(),
                        source,
                    }
                })?;
                return Ok(());
            }

            rotate_configured_managed_key(config_path, output)
        }
    }
}

fn rotate_configured_managed_key<W: Write>(
    config_path: Option<PathBuf>,
    output: &mut W,
) -> CliResult<()> {
    let config_path = match config_path {
        Some(config_path) => config_path,
        None => ServerConfig::default_path()?,
    };
    let config = ServerConfig::load_from_path(config_path)?;
    if config.server.session_encryption_secret.is_some()
        || config.repository_providers.values().any(|provider| {
            let crate::RepositoryProviderConfig::GitHub(provider) = provider;
            provider.authentication.personal_access_token().is_some()
        })
    {
        return Err(CliError::InvalidArguments {
            message: "sessions generate-key manages only the native credential-store key; remove the explicit server.session_encryption_secret or deprecated provider personal_access_token before using it".to_owned(),
        });
    }

    let _process_lock = crate::metadata::ServerProcessLock::acquire(&config.server.metadata_path)?;
    let database = MetadataDatabase::open(&config.server.metadata_path)?;
    let invalidated = crate::session_keys::rotate_managed_session_key(
        &database,
        &crate::session_keys::NativeSessionEncryptionKeyStore,
    )?;
    writeln!(
        output,
        "Generated a new managed session encryption key and invalidated {invalidated} session(s)."
    )
    .map_err(|source| CliError::Io {
        context: "failed to report session-key rotation".to_owned(),
        source,
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_key_requires_explicit_confirmation_before_loading_config() {
        let command = SessionsCommand {
            action: SessionsAction::GenerateKey,
        };
        let mut input = "no\n".as_bytes();
        let mut output = Vec::new();

        run_sessions_with_io(
            command,
            Some(PathBuf::from("does-not-exist.yml")),
            &mut input,
            &mut output,
        )
        .expect("declining rotation should not touch configuration");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("invalidate all current"));
        assert!(rendered.contains("was not changed"));
    }
}
