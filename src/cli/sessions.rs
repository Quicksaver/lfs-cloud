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
    let mut warning_output = io::stderr().lock();
    run_sessions_with_io(
        command,
        config_path,
        &mut input,
        &mut output,
        &mut warning_output,
    )
    .map_err(anyhow::Error::from)
}

fn run_sessions_with_io<R, W, E>(
    command: SessionsCommand,
    config_path: Option<PathBuf>,
    input: &mut R,
    output: &mut W,
    warning_output: &mut E,
) -> CliResult<()>
where
    R: BufRead,
    W: Write,
    E: Write,
{
    match command.action {
        SessionsAction::GenerateKey => {
            warning_output
                .write_all(ROTATION_WARNING.as_bytes())
                .map_err(|source| CliError::Io {
                    context: "failed to write session-key rotation warning".to_owned(),
                    source,
                })?;
            warning_output.flush().map_err(|source| CliError::Io {
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
    use std::fs;

    use super::*;

    #[test]
    fn generate_key_requires_explicit_confirmation_before_loading_config() {
        let command = SessionsCommand {
            action: SessionsAction::GenerateKey,
        };
        let mut input = "no\n".as_bytes();
        let mut output = Vec::new();
        let mut warning_output = Vec::new();

        run_sessions_with_io(
            command,
            Some(PathBuf::from("does-not-exist.yml")),
            &mut input,
            &mut output,
            &mut warning_output,
        )
        .expect("declining rotation should not touch configuration");

        let rendered = String::from_utf8(output).expect("output should be UTF-8");
        assert!(rendered.contains("was not changed"));
        let warning = String::from_utf8(warning_output).expect("warning should be UTF-8");
        assert!(warning.contains("invalidate all current"));
    }

    #[test]
    fn generate_key_rejects_explicit_secret_before_opening_metadata() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let config_path = directory.path().join("lfscloud.yml");
        let metadata_path = directory.path().join("metadata.sqlite3");
        fs::write(
            &config_path,
            format!(
                "server:\n  metadata_path: {:?}\n  session_encryption_secret: explicit-session-secret-at-least-32-characters\n",
                metadata_path.to_string_lossy()
            ),
        )
        .expect("configuration fixture should be written");

        let error = rotate_configured_managed_key(Some(config_path), &mut Vec::new())
            .expect_err("an explicit secret should reject managed-key rotation");

        assert!(error.to_string().contains("native credential-store key"));
        assert!(!metadata_path.exists());
    }

    #[test]
    fn generate_key_rejects_rotation_while_server_lock_is_held() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let config_path = directory.path().join("lfscloud.yml");
        let metadata_path = directory.path().join("metadata.sqlite3");
        fs::write(
            &config_path,
            format!(
                "server:\n  metadata_path: {:?}\n",
                metadata_path.to_string_lossy()
            ),
        )
        .expect("configuration fixture should be written");
        let _server_lock = crate::metadata::ServerProcessLock::acquire(&metadata_path)
            .expect("server fixture should acquire the lifecycle lock");

        let error = rotate_configured_managed_key(Some(config_path), &mut Vec::new())
            .expect_err("an active server lock should reject rotation");

        assert!(error.to_string().contains("already running"));
    }
}
