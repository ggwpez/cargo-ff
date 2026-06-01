use anyhow::Context;
use cargo_ff::cli::Cli;
use std::process::ExitCode;

fn main() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse_argv();
    let cfg = cli.into_config();
    let report = cargo_ff::run(&cfg).context("cargo ff failed")?;
    // Mirror rustfmt's exit code (0 = clean, 1 = diffs/failures). Returning it
    // from `main` instead of calling `process::exit` lets stdout/stderr flush.
    Ok(ExitCode::from(u8::try_from(report.exit_code).unwrap_or(1)))
}
