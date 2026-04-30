use crate::types::Config;
use clap::{Args, Parser};
use std::num::NonZeroUsize;
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "cargo-ff",
    bin_name = "cargo ff",
    about = "Fast Format is a fast drop-in replacement for cargo fmt",
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

    /// Extra arguments forwarded to rustfmt (after `--`).
    #[arg(last = true)]
    pub rustfmt_args: Vec<String>,

    #[command(flatten)]
    pub ff: FfArgs,
}

/// cargo-ff-specific flags. All long names are prefixed `--ff-*` so they
/// can never collide with a flag added upstream by `cargo fmt`.
#[derive(Debug, Args)]
pub struct FfArgs {
    /// Number of worker threads. Defaults to available_parallelism().
    #[arg(long = "ff-workers")]
    pub workers: Option<NonZeroUsize>,

    /// Bounded-channel capacity. Default 512. Hidden — benchmarking knob.
    #[arg(long = "ff-channel-capacity", hide = true)]
    pub channel_capacity: Option<usize>,

    /// Crates per rustfmt invocation. Higher amortizes spawn cost; lower
    /// gives finer scheduling granularity. Hidden — benchmarking knob.
    #[arg(long = "ff-batch-size", hide = true)]
    pub batch_size: Option<usize>,

    /// Experimental: skip rustfmt for crates whose `*.rs` mtimes match
    /// the prior successful run. May produce stale results if files
    /// outside `manifest_dir` are pulled in via `#[path]`.
    #[arg(long = "ff-experimental-cache", hide = true)]
    pub experimental_cache: bool,

    /// Emit advisory warnings to stderr (off by default).
    #[arg(long = "ff-warnings")]
    pub warnings: bool,
}

impl Cli {
    /// Parse argv, stripping the cargo-subcommand `argv[1] == "ff"` if present.
    pub fn parse_argv() -> Self {
        let mut args: Vec<std::ffi::OsString> = std::env::args_os().collect();
        if args.len() >= 2 && args[1] == "ff" {
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
            rustfmt_args: self.rustfmt_args,
            workers: self.ff.workers.map(NonZeroUsize::get),
            channel_capacity: self.ff.channel_capacity,
            batch_size: self.ff.batch_size,
            experimental_cache: self.ff.experimental_cache,
            warnings: self.ff.warnings,
        }
    }
}
