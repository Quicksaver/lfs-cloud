//! Shared tracing and logging initialization.
//!
//! CLI and server entry points should use this module instead of configuring
//! `tracing-subscriber` directly, so log filtering and formatting behavior stay
//! consistent as the root package grows new modules.

use std::{env, error::Error, io::IsTerminal};

use tracing_subscriber::{EnvFilter, filter::ParseError};

/// Default environment variable used to override the tracing filter.
pub const DEFAULT_LOG_ENV_VAR: &str = "RUST_LOG";

/// Default tracing filter for local CLI and server runs.
pub const DEFAULT_LOG_FILTER: &str = "warn,lfscloud=info";

/// Configuration for initializing process-wide tracing output.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TracingConfig {
    /// Fallback filter used when the configured environment variable is absent.
    pub default_filter: String,
    /// Optional environment variable name that can override [`Self::default_filter`].
    pub env_filter_var: Option<String>,
    /// Whether ANSI color codes are emitted by the default formatter.
    pub ansi: bool,
}

impl Default for TracingConfig {
    fn default() -> Self {
        Self {
            default_filter: DEFAULT_LOG_FILTER.to_owned(),
            env_filter_var: Some(DEFAULT_LOG_ENV_VAR.to_owned()),
            ansi: std::io::stderr().is_terminal(),
        }
    }
}

impl TracingConfig {
    /// Builds a tracing configuration with a custom fallback filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::TracingConfig;
    ///
    /// let config = TracingConfig::new("lfscloud=debug").without_env_filter();
    /// assert_eq!(config.default_filter, "lfscloud=debug");
    /// ```
    #[must_use]
    pub fn new(default_filter: impl Into<String>) -> Self {
        Self {
            default_filter: default_filter.into(),
            ..Self::default()
        }
    }

    /// Disables environment-variable overrides for deterministic callers.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::TracingConfig;
    ///
    /// let config = TracingConfig::default().without_env_filter();
    /// assert!(config.env_filter_var.is_none());
    /// ```
    #[must_use]
    pub fn without_env_filter(mut self) -> Self {
        self.env_filter_var = None;
        self
    }

    /// Sets the environment variable used to override the fallback filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::TracingConfig;
    ///
    /// let config = TracingConfig::default().with_env_filter_var("LFS_CLOUD_LOG");
    /// assert_eq!(config.env_filter_var.as_deref(), Some("LFS_CLOUD_LOG"));
    /// ```
    #[must_use]
    pub fn with_env_filter_var(mut self, var_name: impl Into<String>) -> Self {
        self.env_filter_var = Some(var_name.into());
        self
    }

    /// Enables or disables ANSI color output for the default formatter.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfscloud::TracingConfig;
    ///
    /// let config = TracingConfig::default().with_ansi(false);
    /// assert!(!config.ansi);
    /// ```
    #[must_use]
    pub fn with_ansi(mut self, ansi: bool) -> Self {
        self.ansi = ansi;
        self
    }
}

/// Error returned when tracing setup cannot be completed.
#[derive(Debug, thiserror::Error)]
pub enum TracingInitError {
    /// The selected tracing filter could not be parsed.
    #[error("invalid tracing filter {value:?}: {source}")]
    InvalidFilter {
        /// Filter string that failed to parse.
        value: String,
        /// Parser error returned by `tracing-subscriber`.
        #[source]
        source: ParseError,
    },

    /// The selected environment variable contains a non-Unicode filter value.
    #[error("tracing filter environment variable {var_name:?} is not valid Unicode: {source}")]
    InvalidEnvironmentFilter {
        /// Environment variable that contained the invalid filter value.
        var_name: String,
        /// Environment variable decoding error.
        #[source]
        source: env::VarError,
    },

    /// A process-wide tracing subscriber was already installed or unavailable.
    #[error("failed to install tracing subscriber: {source}")]
    Install {
        /// Underlying subscriber installation failure.
        #[source]
        source: Box<dyn Error + Send + Sync + 'static>,
    },
}

/// Builds the effective [`EnvFilter`] for a tracing configuration.
///
/// This is separated from [`init_tracing`] so server code can validate or reuse
/// the same filter before it installs its own subscriber layers.
///
/// # Examples
///
/// ```
/// use lfscloud::{TracingConfig, tracing_filter};
///
/// let config = TracingConfig::new("warn,lfscloud=debug").without_env_filter();
/// tracing_filter(&config)?;
/// # Ok::<(), lfscloud::TracingInitError>(())
/// ```
pub fn tracing_filter(config: &TracingConfig) -> Result<EnvFilter, TracingInitError> {
    let value = configured_filter_value(config)?;

    EnvFilter::try_new(value.as_str())
        .map_err(|source| TracingInitError::InvalidFilter { value, source })
}

/// Installs the default process-wide tracing subscriber.
///
/// This installs a process-global subscriber. Calling it after another global
/// subscriber has already been installed returns [`TracingInitError::Install`].
///
/// # Examples
///
/// ```no_run
/// use lfscloud::{TracingConfig, init_tracing};
///
/// init_tracing(&TracingConfig::default())?;
/// # Ok::<(), lfscloud::TracingInitError>(())
/// ```
pub fn init_tracing(config: &TracingConfig) -> Result<(), TracingInitError> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_filter(config)?)
        .with_ansi(config.ansi)
        .with_writer(std::io::stderr)
        .try_init()
        .map_err(|source| TracingInitError::Install { source })
}

fn configured_filter_value(config: &TracingConfig) -> Result<String, TracingInitError> {
    let Some(var_name) = config.env_filter_var.as_deref() else {
        return Ok(config.default_filter.clone());
    };

    match env::var(var_name) {
        Ok(value) if !value.trim().is_empty() => Ok(value),
        Ok(_) | Err(env::VarError::NotPresent) => Ok(config.default_filter.clone()),
        Err(source @ env::VarError::NotUnicode(_)) => {
            Err(TracingInitError::InvalidEnvironmentFilter {
                var_name: var_name.to_owned(),
                source,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{env, ffi::OsString, io::IsTerminal, process::Command};

    use super::{
        DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, TracingConfig, TracingInitError,
        configured_filter_value, tracing_filter,
    };

    const ENV_FILTER_CASE_ENV: &str = "LFS_CLOUD_LOGGING_TEST_CASE";
    const ENV_FILTER_HELPER_TEST: &str = "logging::tests::configured_filter_environment_subprocess";
    const TEST_LOG_ENV_VAR: &str = "LFS_CLOUD_TEST_LOG";

    fn assert_env_filter_subprocess(case: &str, value: OsString) {
        let output = Command::new(env::current_exe().expect("test executable should resolve"))
            .args([
                "--ignored",
                "--exact",
                ENV_FILTER_HELPER_TEST,
                "--nocapture",
            ])
            .env(ENV_FILTER_CASE_ENV, case)
            .env(TEST_LOG_ENV_VAR, value)
            .output()
            .expect("environment-sensitive logging test subprocess should start");

        assert!(
            output.status.success(),
            "environment-sensitive logging test subprocess failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn default_config_uses_project_filter_and_rust_log_override() {
        let config = TracingConfig::default();

        assert_eq!(config.default_filter, DEFAULT_LOG_FILTER);
        assert_eq!(config.env_filter_var.as_deref(), Some(DEFAULT_LOG_ENV_VAR));
        assert_eq!(config.ansi, std::io::stderr().is_terminal());
    }

    #[test]
    fn configured_filter_uses_explicit_default_when_env_override_is_disabled() {
        let config = TracingConfig::new("warn,lfscloud=debug").without_env_filter();

        assert_eq!(
            configured_filter_value(&config).expect("filter value should resolve"),
            "warn,lfscloud=debug"
        );
        tracing_filter(&config).expect("filter should parse");
    }

    #[test]
    fn configured_filter_env_override_wins_over_default() {
        assert_env_filter_subprocess("override", "lfscloud=trace".into());
    }

    #[test]
    fn configured_filter_ignores_empty_env_override() {
        assert_env_filter_subprocess("empty", " \t\n".into());
    }

    #[cfg(unix)]
    #[test]
    fn configured_filter_reports_non_unicode_env_override() {
        use std::os::unix::ffi::OsStringExt;

        assert_env_filter_subprocess(
            "non-unicode",
            OsString::from_vec(vec![0xff, b'w', b'a', b'r', b'n']),
        );
    }

    #[test]
    #[ignore = "invoked as an isolated environment-sensitive test helper"]
    fn configured_filter_environment_subprocess() {
        let Some(case) = env::var_os(ENV_FILTER_CASE_ENV) else {
            return;
        };
        let config = match case.to_str() {
            Some("override") => TracingConfig::new("warn").with_env_filter_var(TEST_LOG_ENV_VAR),
            Some("empty") => {
                TracingConfig::new("warn,lfscloud=info").with_env_filter_var(TEST_LOG_ENV_VAR)
            }
            Some("non-unicode") => {
                let config = TracingConfig::new("warn").with_env_filter_var(TEST_LOG_ENV_VAR);
                let error = configured_filter_value(&config)
                    .expect_err("non-Unicode environment value should fail");

                match error {
                    TracingInitError::InvalidEnvironmentFilter { var_name, .. } => {
                        assert_eq!(var_name, TEST_LOG_ENV_VAR);
                    }
                    TracingInitError::InvalidFilter { .. } | TracingInitError::Install { .. } => {
                        panic!("non-Unicode env value should be reported before parsing")
                    }
                }
                return;
            }
            _ => panic!("environment-sensitive logging test case should be recognized"),
        };

        let expected = match case.to_str() {
            Some("override") => "lfscloud=trace",
            Some("empty") => "warn,lfscloud=info",
            _ => unreachable!("recognized string cases were handled above"),
        };
        assert_eq!(
            configured_filter_value(&config).expect("filter value should resolve"),
            expected
        );
    }

    #[test]
    fn tracing_filter_reports_invalid_filter_value() {
        let config = TracingConfig::new("lfscloud=not-a-level").without_env_filter();
        let error = tracing_filter(&config).expect_err("filter should fail");

        match error {
            TracingInitError::InvalidFilter { value, .. } => {
                assert_eq!(value, "lfscloud=not-a-level");
            }
            TracingInitError::InvalidEnvironmentFilter { .. }
            | TracingInitError::Install { .. } => {
                panic!("invalid filter should fail before subscriber installation")
            }
        }
    }
}
