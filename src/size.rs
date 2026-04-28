//! Size proxies for the coalescer's batch packing.
//!
//! Plays no role in correctness — rustfmt produces the same output
//! regardless of which strategy we pick. Only the *relative* magnitudes
//! between crates matter, since the coalescer uses size_bytes to balance
//! batches. Strategies trade discovery cost for accuracy.

use std::path::{Path, PathBuf};

/// Cap above which a crate is unambiguously a "giant" for solo
/// dispatch. We only need a binary classifier here — the priority queue
/// orders giants relative to each other by their (clamped) size, and
/// since they all clamp to the same value the dispatch order between
/// giants is unspecified, which is fine: any giant landing in an empty
/// queue takes off first regardless.
pub(crate) const HUGE_CUTOFF_BYTES: u64 = 1_000_000;

/// Active size proxy. Walks the crate's `*.rs` files but stops as soon
/// as accumulated bytes hit `HUGE_CUTOFF_BYTES`, returning the cutoff
/// verbatim. So tiny crates pay full walk cost (which is tiny because
/// they have few files), and giants pay only as much as it takes to
/// classify them — typically tens of files instead of thousands. Files
/// stat'd here end up in the page cache for rustfmt's own reads, so
/// the I/O is recouped downstream.
pub(crate) fn estimate(manifest_dir: &Path, entry_points: &[PathBuf]) -> u64 {
    let _ = entry_points;
    manifest_dir_rs_bytes_clamped(manifest_dir, HUGE_CUTOFF_BYTES)
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
#[allow(dead_code)]
pub(crate) fn manifest_dir_rs_bytes(manifest_dir: &Path) -> u64 {
    manifest_dir_rs_bytes_clamped(manifest_dir, u64::MAX)
}

/// Sum of `*.rs` bytes under `manifest_dir`, returning early once the
/// running total reaches `cap` (returning `cap`). Skips `target/` and
/// non-files. Lets us stop walking after a crate is classified as
/// giant, instead of fully enumerating multi-MB trees we'll only use
/// for one bit of "is this huge?" information.
pub(crate) fn manifest_dir_rs_bytes_clamped(manifest_dir: &Path, cap: u64) -> u64 {
    let mut total: u64 = 0;
    let walker = walkdir::WalkDir::new(manifest_dir)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| e.file_name() != "target");
    for entry in walker {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().map(|x| x != "rs").unwrap_or(true) {
            continue;
        }
        if let Ok(m) = entry.metadata() {
            total = total.saturating_add(m.len());
            if total >= cap {
                return cap;
            }
        }
    }
    total
}
