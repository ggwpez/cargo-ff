//! Result aggregation — the pipeline's sink stage.
//!
//! Its own module (rather than inlined into `lib::run`) to keep the
//! one-module-per-pipeline-stage layout intact: `discover` → `coalesce` →
//! `dispatch` → `exec` → `report`. Concentrating the deterministic-ordering
//! sort, the stdout/stderr flush, and the `--check` failure attribution here
//! keeps `run` readable as a wiring diagram rather than a grab-bag of stages.

use crate::types::{BatchResult, FileFailure, Report};
use crossbeam_channel::Receiver;
use std::io::Write;

pub(crate) fn aggregate(rx: Receiver<BatchResult>) -> Report {
    let mut results: Vec<BatchResult> = rx.into_iter().collect();
    results.sort_by(|a, b| a.sort_key.cmp(&b.sort_key));

    let mut failures = Vec::new();
    let mut exit_code = 0;

    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();

    for r in &results {
        let _ = out.write_all(&r.stdout);
        let _ = err.write_all(&r.stderr);
        if r.exit_code != 0 {
            exit_code = 1;
            for bf in &r.files {
                failures.push(FileFailure {
                    file: bf.file.clone(),
                    manifest_dir: bf.manifest_dir.clone(),
                });
            }
        }
    }

    Report {
        failures,
        exit_code,
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; `unwrap` is the idiomatic way to fail loudly.
    #![allow(clippy::unwrap_used)]

    use super::aggregate;
    use crate::types::{BatchFile, BatchResult};
    use crossbeam_channel::unbounded;
    use std::path::PathBuf;

    fn result(sort_key: &str, exit_code: i32, files: &[(&str, &str)]) -> BatchResult {
        BatchResult {
            sort_key: PathBuf::from(sort_key),
            stdout: Vec::new(),
            stderr: Vec::new(),
            exit_code,
            files: files
                .iter()
                .map(|(f, m)| BatchFile {
                    file: PathBuf::from(f),
                    manifest_dir: PathBuf::from(m),
                })
                .collect(),
        }
    }

    #[test]
    fn clean_results_produce_no_failures() {
        let (tx, rx) = unbounded();
        tx.send(result("a", 0, &[("a/src/lib.rs", "a")])).unwrap();
        drop(tx);
        let report = aggregate(rx);
        assert_eq!(report.exit_code, 0);
        assert!(report.failures.is_empty());
    }

    #[test]
    fn nonzero_exit_attributes_every_file_in_the_batch() {
        let (tx, rx) = unbounded();
        tx.send(result("a", 1, &[("a/src/lib.rs", "a"), ("a/src/main.rs", "a")]))
            .unwrap();
        drop(tx);
        let report = aggregate(rx);
        assert_eq!(report.exit_code, 1);
        assert_eq!(report.failures.len(), 2, "each file in a failing batch is reported");
    }

    #[test]
    fn failures_follow_deterministic_sort_key_order() {
        let (tx, rx) = unbounded();
        // Sent out of order; the aggregator sorts by sort_key before emitting.
        tx.send(result("z", 1, &[("z/src/lib.rs", "z")])).unwrap();
        tx.send(result("a", 1, &[("a/src/lib.rs", "a")])).unwrap();
        drop(tx);
        let report = aggregate(rx);
        let dirs: Vec<_> = report.failures.iter().map(|f| f.manifest_dir.clone()).collect();
        assert_eq!(dirs, vec![PathBuf::from("a"), PathBuf::from("z")]);
    }
}
