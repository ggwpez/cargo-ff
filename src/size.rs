//! Size proxies for the coalescer's batch packing.
//!
//! Plays no role in correctness — rustfmt produces the same output
//! regardless of which strategy we pick. Only the *relative* magnitudes
//! between crates matter, since the coalescer uses size_bytes to balance
//! batches. Strategies trade discovery cost for accuracy.

use std::path::{Path, PathBuf};

/// Active size proxy. Swap the body to experiment.
///
/// Called once per crate during discovery; cost matters — but the files
/// stat'd here end up in the page cache for rustfmt's own reads, so the
/// extra I/O is mostly recouped downstream rather than added on top.
pub(crate) fn estimate(manifest_dir: &Path, entry_points: &[PathBuf]) -> u64 {
    let _ = entry_points;
    manifest_dir_rs_bytes(manifest_dir)
}

/// Sum of bytes of the crate's target entry points (`src/lib.rs`,
/// `src/bin/foo.rs`, `tests/it.rs`, …). Free — we already have these
/// paths from cargo metadata. Undercounts: rustfmt walks the `mod`
/// tree from each entry, so a 200-byte `lib.rs` declaring submodules
/// actually represents far more work than 200 bytes.
#[allow(dead_code)]
pub(crate) fn entry_point_bytes(entries: &[PathBuf]) -> u64 {
    entries
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum()
}

/// Sum of bytes of every `*.rs` file under `manifest_dir`, skipping
/// `target/`. Closer to what rustfmt actually parses (it walks `mod`
/// declarations from entry points and reads the same files we'd
/// enumerate here, modulo `#[path]` and excluded modules). Costs one
/// walkdir per crate during discovery — for 580 crates with ~10 files
/// each, on the order of 50ms total.
pub(crate) fn manifest_dir_rs_bytes(manifest_dir: &Path) -> u64 {
    walkdir::WalkDir::new(manifest_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .filter(|e| e.path().extension().map(|x| x == "rs").unwrap_or(false))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}
