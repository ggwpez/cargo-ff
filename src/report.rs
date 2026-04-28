use crate::types::{CrateResult, FileFailure, Report};
use crossbeam_channel::Receiver;
use std::io::Write;

pub(crate) fn aggregate(rx: Receiver<CrateResult>) -> Report {
    let mut results: Vec<CrateResult> = rx.into_iter().collect();
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
            for f in &r.files {
                failures.push(FileFailure {
                    file: f.clone(),
                    manifest_dir: r.sort_key.clone(),
                });
            }
        }
    }

    Report {
        failures,
        exit_code,
    }
}
