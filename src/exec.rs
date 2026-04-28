use crate::dispatch::PriorityQueue;
use crate::types::{Batch, BatchResult, Config};
use crossbeam_channel::Sender;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;

pub(crate) fn worker(queue: Arc<PriorityQueue>, tx: Sender<BatchResult>, cfg: &Config) {
    while let Some(batch) = queue.pop() {
        let result = format_batch(&batch, cfg);
        if tx.send(result).is_err() {
            break;
        }
    }
}

fn format_batch(batch: &Batch, cfg: &Config) -> BatchResult {
    let sort_key = batch.sort_key();
    let mut files: Vec<(PathBuf, PathBuf)> = Vec::with_capacity(batch.file_count());
    for unit in &batch.units {
        for f in &unit.files {
            files.push((f.clone(), unit.manifest_dir.clone()));
        }
    }

    let mut cmd = Command::new("rustfmt");
    // cwd doesn't affect rustfmt.toml resolution — rustfmt walks up from
    // each *file's* path. Pass absolute file paths (we canonicalize in
    // discover) and any cwd works. Pick the sort_key crate so spawn
    // diagnostics are at least pointing at one of the batch's crates.
    cmd.current_dir(&sort_key);
    cmd.arg("--edition").arg(batch.edition.as_str());
    if cfg.check {
        cmd.arg("--check");
    }
    for arg in &cfg.rustfmt_args {
        cmd.arg(arg);
    }
    for unit in &batch.units {
        for f in &unit.files {
            cmd.arg(f);
        }
    }

    let output = match cmd.output() {
        Ok(o) => o,
        Err(e) => {
            let crates: Vec<String> = batch
                .units
                .iter()
                .map(|u| u.manifest_dir.display().to_string())
                .collect();
            let msg = format!(
                "failed to spawn rustfmt for batch ({} crates: {}): {e}\n",
                crates.len(),
                crates.join(", ")
            );
            return BatchResult {
                sort_key,
                stdout: Vec::new(),
                stderr: msg.into_bytes(),
                exit_code: 2,
                files,
            };
        }
    };

    BatchResult {
        sort_key,
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code().unwrap_or(-1),
        files,
    }
}
