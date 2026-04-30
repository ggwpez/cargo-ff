use anyhow::Context;
use cargo_ff::cli::Cli;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse_argv();
    let cfg = cli.into_config();
    let report = cargo_ff::run(&cfg).context("cargo ff failed")?;
    std::process::exit(report.exit_code);
}
