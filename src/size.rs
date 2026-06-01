//! Size proxies for the coalescer's batch packing.
//!
//! Plays no role in correctness — rustfmt produces the same output
//! regardless of which strategy we pick. Only the *relative* magnitudes
//! between crates matter, since the coalescer uses `size_bytes` to balance
//! batches. Strategies trade discovery cost for accuracy.

use std::path::Path;

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
pub(crate) fn estimate(manifest_dir: &Path) -> u64 {
    manifest_dir_rs_bytes_clamped(manifest_dir, HUGE_CUTOFF_BYTES)
}

/// Sum of `*.rs` bytes under `manifest_dir`, returning early once the
/// running total reaches `cap` (returning `cap`). Skips `target/` and
/// non-files. Lets us stop walking after a crate is classified as
/// giant, instead of fully enumerating multi-MB trees we'll only use
/// for one bit of "is this huge?" information.
fn manifest_dir_rs_bytes_clamped(manifest_dir: &Path, cap: u64) -> u64 {
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
        if entry.path().extension().is_none_or(|x| x != "rs") {
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

#[cfg(test)]
mod tests {
    // Tests assert by panicking; `unwrap` is the idiomatic way to fail loudly.
    #![allow(clippy::unwrap_used)]

    use super::{HUGE_CUTOFF_BYTES, estimate, manifest_dir_rs_bytes_clamped};
    use std::path::PathBuf;

    fn unique_tmp(prefix: &str) -> PathBuf {
        std::env::temp_dir().join(format!("{prefix}-{}", std::process::id()))
    }

    #[test]
    fn estimate_sums_rs_bytes_and_ignores_other_files() {
        let dir = unique_tmp("ff-size-sum");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), vec![b'x'; 100]).unwrap();
        std::fs::write(dir.join("b.rs"), vec![b'y'; 50]).unwrap();
        std::fs::write(dir.join("notes.txt"), vec![b'z'; 9999]).unwrap();

        let bytes = estimate(&dir);
        assert_eq!(bytes, 150, "only the two .rs files count");
        assert!(bytes < HUGE_CUTOFF_BYTES);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn clamping_returns_the_cap_verbatim_once_reached() {
        let dir = unique_tmp("ff-size-clamp");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), vec![b'x'; 1000]).unwrap();
        std::fs::write(dir.join("b.rs"), vec![b'y'; 1000]).unwrap();

        // A cap below the total is hit mid-walk and returned exactly — this is
        // the "giant" classifier the coalescer's solo-dispatch check relies on.
        assert_eq!(manifest_dir_rs_bytes_clamped(&dir, 500), 500);
        // A cap above the total yields the true sum.
        assert_eq!(manifest_dir_rs_bytes_clamped(&dir, 10_000), 2000);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
