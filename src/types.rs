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
}

#[derive(Debug)]
pub struct CrateResult {
    pub sort_key: PathBuf,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub exit_code: i32,
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MessageFormat {
    #[default]
    Human,
    Short,
    Json,
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
    pub message_format: MessageFormat,
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
    #[error("cargo metadata failed: {0}")]
    Metadata(#[from] cargo_metadata::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("unsupported edition `{edition}` for package `{package}`; cargo-ffmt knows 2015/2018/2021/2024 — bump the dep or pin a known edition")]
    UnsupportedEdition { edition: String, package: String },
    #[error("package(s) not found in the workspace: {}", .0.join(", "))]
    UnknownPackages(Vec<String>),
    #[error("worker thread panicked")]
    WorkerPanic,
    #[error("send failed (channel closed)")]
    SendClosed,
}

pub type Result<T> = std::result::Result<T, Error>;
