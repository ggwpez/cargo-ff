use anyhow::Context;
use cargo_ffmt::cli::Cli;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse_argv();
    let cfg = cli.into_config();
    let report = cargo_ffmt::run(&cfg).context("cargo ffmt failed")?;
    std::process::exit(report.exit_code);
}
