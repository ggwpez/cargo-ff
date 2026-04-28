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
            for (file, manifest_dir) in &r.files {
                failures.push(FileFailure {
                    file: file.clone(),
                    manifest_dir: manifest_dir.clone(),
                });
            }
        }
    }

    Report {
        failures,
        exit_code,
    }
}
