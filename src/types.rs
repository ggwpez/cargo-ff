use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Edition {
    E2015,
    E2018,
    E2021,
    E2024,
}

impl Edition {
    pub fn as_str(self) -> &'static str {
        match self {
            Edition::E2015 => "2015",
            Edition::E2018 => "2018",
            Edition::E2021 => "2021",
            Edition::E2024 => "2024",
        }
    }
}

impl std::str::FromStr for Edition {
    type Err = UnknownEdition;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        Ok(match s {
            "2015" => Edition::E2015,
            "2018" => Edition::E2018,
            "2021" => Edition::E2021,
            "2024" => Edition::E2024,
            other => return Err(UnknownEdition(other.to_owned())),
        })
    }
}

impl TryFrom<cargo_metadata::Edition> for Edition {
    type Error = UnknownEdition;
    fn try_from(e: cargo_metadata::Edition) -> std::result::Result<Self, Self::Error> {
        e.as_str().parse()
    }
}

/// Edition string we don't recognize. Future variants (2027, 2030, …) land
/// here. Caller wraps with package context before surfacing.
#[derive(Debug, Clone, thiserror::Error)]
#[error("unknown edition: {0}")]
pub struct UnknownEdition(pub String);

#[derive(Debug, Clone)]
pub struct CrateUnit {
    pub edition: Edition,
    pub manifest_dir: PathBuf,
    pub files: Vec<PathBuf>,
    /// Work-size proxy: sum of `*.rs` bytes under `manifest_dir`,
    /// clamped at `size::HUGE_CUTOFF_BYTES`. Drives LPT bin-packing in
    /// the coalescer and dispatch ordering in the priority queue. Only
    /// the *ratios* between crates matter; a value exactly equal to the
    /// cutoff means the crate is at-or-above the solo-dispatch threshold.
    pub size_bytes: u64,
}

/// One rustfmt invocation's worth of work: a homogeneous-edition group of
/// crates whose entry-point files are passed together to a single rustfmt
/// process. With batch_size=1 this is equivalent to per-crate dispatch.
#[derive(Debug, Clone)]
pub struct Batch {
    pub edition: Edition,
    pub units: Vec<CrateUnit>,
}

impl Batch {
    pub fn size_bytes(&self) -> u64 {
        self.units.iter().map(|u| u.size_bytes).sum()
    }

    pub fn file_count(&self) -> usize {
        self.units.iter().map(|u| u.files.len()).sum()
    }

    /// Sort key for deterministic output ordering: lex-min of manifest dirs.
    pub fn sort_key(&self) -> PathBuf {
        self.units
            .iter()
            .map(|u| u.manifest_dir.clone())
            .min()
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct BatchResult {
    pub sort_key: PathBuf,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    /// All files in the batch, with the manifest_dir they came from.
    /// Used by the aggregator to attribute `--check` failures back to
    /// individual crates.
    pub files: Vec<(PathBuf, PathBuf)>,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub manifest_path: Option<PathBuf>,
    pub packages: Vec<String>,
    /// Format every workspace member, regardless of `manifest_path`'s implicit
    /// package selection. Mirrors `cargo fmt --all`.
    pub all: bool,
    pub check: bool,
    pub workers: Option<usize>,
    pub channel_capacity: Option<usize>,
    pub rustfmt_args: Vec<String>,
    /// Crates per rustfmt invocation. Higher values amortize spawn cost
    /// (~40ms/invocation on M-series) at the cost of coarser scheduling
    /// granularity. `None` → 3 (the curve is flat across bs=12-48 thanks
    /// to the priority queue, so a small default keeps tiny workspaces
    /// from collapsing into one batch).
    pub batch_size: Option<usize>,
}

#[derive(Debug)]
pub struct Report {
    pub failures: Vec<FileFailure>,
    pub exit_code: i32,
}

#[derive(Debug)]
pub struct FileFailure {
    pub file: PathBuf,
    pub manifest_dir: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("`workers` must be >= 1 (got {0})")]
    InvalidWorkers(usize),
    #[error("cargo metadata failed: {0}")]
    Metadata(#[from] cargo_metadata::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error(
        "unsupported edition `{edition}` for package `{package}`; cargo-ff knows 2015/2018/2021/2024 — bump the dep or pin a known edition"
    )]
    UnsupportedEdition { edition: String, package: String },
    #[error("package(s) not found in the workspace: {}", .0.join(", "))]
    UnknownPackages(Vec<String>),
    #[error("{0} thread panicked")]
    ThreadPanicked(&'static str),
    #[error("send failed (channel closed)")]
    SendClosed,
}

pub type Result<T> = std::result::Result<T, Error>;
