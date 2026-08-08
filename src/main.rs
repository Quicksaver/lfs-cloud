//! Command-line entry point for LFS Cloud.

use std::{
    env,
    ffi::OsStr,
    io::{self, Write},
    process::ExitCode,
};

use anyhow::Context as _;

/// Runs the CLI with process-wide error reporting.
fn main() -> ExitCode {
    let quiet = quiet_requested(env::args_os());
    if !quiet {
        enable_error_backtraces();
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to initialize the lfscloud async runtime");
    let result = match runtime {
        Ok(runtime) => runtime.block_on(lfscloud::run_from_env()),
        Err(error) => Err(error),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if let Err(report_error) = write_error_report(&mut io::stderr().lock(), &error, quiet) {
                eprintln!("Error: failed to write lfscloud error report: {report_error}");
            }
            ExitCode::FAILURE
        }
    }
}

fn quiet_requested<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    for argument in args {
        let argument = argument.as_ref();
        if argument == OsStr::new("--") {
            break;
        }
        if argument == OsStr::new("--quiet") {
            return true;
        }
    }
    false
}

fn enable_error_backtraces() {
    if env::var_os("RUST_BACKTRACE").is_some() || env::var_os("RUST_LIB_BACKTRACE").is_some() {
        return;
    }

    // SAFETY: this runs at the start of `main`, before the Tokio runtime or any
    // application-owned thread is created, so no other thread can read the
    // process environment concurrently.
    unsafe {
        env::set_var("RUST_LIB_BACKTRACE", "1");
    }
}

fn write_error_report<W>(output: &mut W, error: &anyhow::Error, quiet: bool) -> io::Result<()>
where
    W: Write,
{
    if quiet {
        return writeln!(output, "Error: {error:?}");
    }

    writeln!(output, "Error: {error}")?;
    writeln!(output)?;
    writeln!(output, "Detailed error trace:")?;
    for (index, cause) in error.chain().enumerate() {
        writeln!(output, "  {index}: {cause}")?;
        write_indented(output, "     debug: ", &format!("{cause:#?}"))?;
    }
    writeln!(output)?;
    writeln!(output, "Backtrace:")?;
    write_indented(output, "  ", &error.backtrace().to_string())
}

fn write_indented<W>(output: &mut W, prefix: &str, value: &str) -> io::Result<()>
where
    W: Write,
{
    for (index, line) in value.lines().enumerate() {
        if index == 0 {
            writeln!(output, "{prefix}{line}")?;
        } else {
            writeln!(
                output,
                "{empty:width$}{line}",
                empty = "",
                width = prefix.len()
            )?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io;

    use anyhow::Context as _;

    use super::*;

    #[test]
    fn detailed_error_report_includes_concrete_causes_and_backtrace_state() {
        let error = anyhow::Error::new(lfscloud::CliError::from(
            lfscloud::MigrationError::InvalidInput {
                message: lfscloud::SanitizedMessage::new("missing fixture"),
            },
        ))
        .context("failed to inspect fixture");
        let mut output = Vec::new();

        write_error_report(&mut output, &error, false)
            .expect("detailed error report should render");

        let output = String::from_utf8(output).expect("error report should be UTF-8");
        assert!(output.starts_with("Error: failed to inspect fixture"));
        assert!(output.contains("Detailed error trace:"));
        assert!(output.contains("failed to inspect fixture"));
        assert!(output.contains("missing fixture"));
        assert!(output.contains("Migration"));
        assert!(output.contains("InvalidInput"));
        assert!(output.contains("Backtrace:"));
    }

    #[test]
    fn quiet_error_report_preserves_the_current_concise_format() {
        let error = Err::<(), _>(io::Error::new(io::ErrorKind::NotFound, "missing fixture"))
            .context("failed to inspect fixture")
            .expect_err("fixture error should be retained");
        let mut output = Vec::new();

        write_error_report(&mut output, &error, true).expect("quiet error report should render");

        assert_eq!(
            String::from_utf8(output).expect("error report should be UTF-8"),
            format!("Error: {error:?}\n")
        );
    }

    #[test]
    fn quiet_argument_detection_accepts_global_flag_positions() {
        assert!(quiet_requested(["lfscloud", "--quiet", "status"]));
        assert!(quiet_requested(["lfscloud", "status", "--quiet"]));
        assert!(!quiet_requested(["lfscloud", "status"]));
        assert!(!quiet_requested([
            "lfscloud",
            "--log-level=quiet",
            "status"
        ]));
        assert!(!quiet_requested(["lfscloud", "hydrate", "--", "--quiet"]));
    }
}
