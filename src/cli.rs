use crate::types::Config;
use clap::Parser;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "cargo-ffmt",
    bin_name = "cargo ffmt",
    about = "Fast parallel cargo fmt — streaming, crate-level parallel rustfmt driver",
    version
)]
pub struct Cli {
    /// Run rustfmt in --check mode (exit non-zero if any file would change).
    #[arg(long)]
    pub check: bool,

    /// Format every workspace member, regardless of which package
    /// `--manifest-path` (or the current directory) implicitly selects.
    /// Mirrors `cargo fmt --all`.
    #[arg(long)]
    pub all: bool,

    /// Format only the given workspace member(s). Can be repeated.
    #[arg(short = 'p', long = "package")]
    pub packages: Vec<String>,

    /// Path to the workspace's Cargo.toml (forwarded to `cargo metadata`).
    #[arg(long)]
    pub manifest_path: Option<PathBuf>,

    /// Number of worker threads. Defaults to available_parallelism().
    #[arg(long)]
    pub workers: Option<usize>,

    /// Bounded-channel capacity. Default 512. Hidden — benchmarking knob.
    #[arg(long, hide = true)]
    pub channel_capacity: Option<usize>,

    /// Crates per rustfmt invocation. Higher amortizes spawn cost; lower
    /// gives finer scheduling granularity. Hidden — benchmarking knob.
    #[arg(long, hide = true)]
    pub batch_size: Option<usize>,

    /// Extra arguments forwarded to rustfmt (after `--`).
    #[arg(last = true)]
    pub rustfmt_args: Vec<String>,
}

impl Cli {
    /// Parse argv, stripping the cargo-subcommand `argv[1] == "ffmt"` if present.
    pub fn parse_argv() -> Self {
        let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if args.len() >= 2 && args[1] == "ffmt" {
            args.remove(1);
        }
        Cli::parse_from(args)
    }

    pub fn into_config(self) -> Config {
        Config {
            manifest_path: self.manifest_path,
            packages: self.packages,
            all: self.all,
            check: self.check,
            workers: self.workers,
            channel_capacity: self.channel_capacity,
            rustfmt_args: self.rustfmt_args,
            batch_size: self.batch_size,
        }
    }
}
