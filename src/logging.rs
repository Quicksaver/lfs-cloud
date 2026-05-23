//! Shared tracing and logging initialization.
//!
//! CLI and server entry points should use this module instead of configuring
//! `tracing-subscriber` directly, so log filtering and formatting behavior stay
//! consistent as the root package grows new modules.

use std::{env, error::Error};

use tracing_subscriber::{EnvFilter, filter::ParseError};

/// Default environment variable used to override the tracing filter.
pub const DEFAULT_LOG_ENV_VAR: &str = "RUST_LOG";

/// Default tracing filter for local CLI and server runs.
pub const DEFAULT_LOG_FILTER: &str = "warn,lfs_cloud=info";

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
            ansi: true,
        }
    }
}

impl TracingConfig {
    /// Builds a tracing configuration with a custom fallback filter.
    ///
    /// # Examples
    ///
    /// ```
    /// use lfs_cloud::TracingConfig;
    ///
    /// let config = TracingConfig::new("lfs_cloud=debug").without_env_filter();
    /// assert_eq!(config.default_filter, "lfs_cloud=debug");
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
    /// use lfs_cloud::TracingConfig;
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
    /// use lfs_cloud::TracingConfig;
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
    /// use lfs_cloud::TracingConfig;
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
/// use lfs_cloud::{TracingConfig, tracing_filter};
///
/// let config = TracingConfig::new("warn,lfs_cloud=debug").without_env_filter();
/// let filter = tracing_filter(&config)?;
///
/// let filter_text = filter.to_string();
/// assert!(filter_text.contains("warn"));
/// assert!(filter_text.contains("lfs_cloud=debug"));
/// # Ok::<(), lfs_cloud::TracingInitError>(())
/// ```
pub fn tracing_filter(config: &TracingConfig) -> Result<EnvFilter, TracingInitError> {
    let value = configured_filter_value(config);

    EnvFilter::try_new(value.as_str())
        .map_err(|source| TracingInitError::InvalidFilter { value, source })
}

/// Installs the default process-wide tracing subscriber.
///
/// # Examples
///
/// ```no_run
/// use lfs_cloud::{TracingConfig, init_tracing};
///
/// init_tracing(&TracingConfig::default())?;
/// # Ok::<(), lfs_cloud::TracingInitError>(())
/// ```
pub fn init_tracing(config: &TracingConfig) -> Result<(), TracingInitError> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_filter(config)?)
        .with_ansi(config.ansi)
        .try_init()
        .map_err(|source| TracingInitError::Install { source })
}

fn configured_filter_value(config: &TracingConfig) -> String {
    config
        .env_filter_var
        .as_deref()
        .and_then(|var_name| env::var(var_name).ok())
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| config.default_filter.clone())
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_LOG_ENV_VAR, DEFAULT_LOG_FILTER, TracingConfig, TracingInitError, tracing_filter,
    };

    #[test]
    fn default_config_uses_project_filter_and_rust_log_override() {
        let config = TracingConfig::default();

        assert_eq!(config.default_filter, DEFAULT_LOG_FILTER);
        assert_eq!(config.env_filter_var.as_deref(), Some(DEFAULT_LOG_ENV_VAR));
        assert!(config.ansi);
    }

    #[test]
    fn tracing_filter_uses_explicit_default_when_env_override_is_disabled() {
        let config = TracingConfig::new("warn,lfs_cloud=debug").without_env_filter();
        let filter = tracing_filter(&config).expect("filter should parse");
        let filter_text = filter.to_string();

        assert!(filter_text.contains("warn"));
        assert!(filter_text.contains("lfs_cloud=debug"));
    }

    #[test]
    fn tracing_filter_reports_invalid_filter_value() {
        let config = TracingConfig::new("lfs_cloud=not-a-level").without_env_filter();
        let error = tracing_filter(&config).expect_err("filter should fail");

        match error {
            TracingInitError::InvalidFilter { value, .. } => {
                assert_eq!(value, "lfs_cloud=not-a-level");
            }
            TracingInitError::Install { .. } => {
                panic!("invalid filter should fail before subscriber installation")
            }
        }
    }
}
