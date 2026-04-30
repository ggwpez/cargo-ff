pub mod types;

mod cache;
mod coalesce;
mod discover;
mod dispatch;
mod exec;
mod report;
mod size;

#[cfg(feature = "cli")]
pub mod cli;

pub use types::{Config, Edition, Error, FileFailure, Report, Result};

#[doc(hidden)]
pub mod __test_only {
    use crate::types::{Config, CrateUnit, Result};
    use crossbeam_channel::Sender;

    pub fn discover_run(cfg: &Config, tx: Sender<CrateUnit>) -> Result<()> {
        crate::discover::run(cfg, tx).map(drop)
    }
}

use crossbeam_channel::bounded;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

pub fn run(cfg: &Config) -> Result<Report> {
    if matches!(cfg.workers, Some(0)) {
        return Err(Error::InvalidWorkers(0));
    }

    let n = cfg
        .workers
        .or_else(|| thread::available_parallelism().ok().map(|p| p.get()))
        .unwrap_or(1);
    let cap = cfg.channel_capacity.unwrap_or(512);
    let batch_size = cfg.batch_size.unwrap_or(3);
    // Same cutoff used by the size proxy: any crate the proxy clamped
    // to `HUGE_CUTOFF_BYTES` is by definition the threshold or above,
    // so the comparison `>= HUGE_CUTOFF_BYTES` exactly catches them.
    let solo_threshold = size::HUGE_CUTOFF_BYTES;

    let (unit_tx, unit_rx) = bounded::<types::CrateUnit>(cap);
    let queue = Arc::new(dispatch::PriorityQueue::new());
    let (result_tx, result_rx) = bounded::<types::BatchResult>(cap);

    let cfg_d = cfg.clone();
    let producer = thread::spawn(move || discover::run(&cfg_d, unit_tx));

    // Coalescer pushes batches into `queue`; closes it on exit so
    // workers' `pop()` returns `None` once everything is drained.
    let coalescer_q = queue.clone();
    let coalescer = thread::spawn(move || {
        let r = coalesce::run(unit_rx, &coalescer_q, batch_size, 4, solo_threshold);
        coalescer_q.close();
        r
    });

    let mut workers = Vec::with_capacity(n);
    for _ in 0..n {
        let q = queue.clone();
        let tx = result_tx.clone();
        let cfg_w = cfg.clone();
        workers.push(thread::spawn(move || exec::worker(q, tx, &cfg_w)));
    }
    drop(result_tx);

    let report = report::aggregate(result_rx);

    let cache_opt = join_fallible(producer, "producer")?;
    join_fallible(coalescer, "coalescer")?;
    for w in workers {
        join_void(w, "worker")?;
    }

    // Commit the skip cache. Drop pending entries for crates that had
    // any --check failure so future runs re-fingerprint and surface them.
    if let Some(mut cache) = cache_opt {
        for f in &report.failures {
            cache.invalidate(&f.manifest_dir);
        }
        let _ = cache.commit_and_save();
    }

    Ok(report)
}

fn join_fallible<T>(h: JoinHandle<Result<T>>, name: &'static str) -> Result<T> {
    h.join().map_err(|_| Error::ThreadPanicked(name))?
}

fn join_void(h: JoinHandle<()>, name: &'static str) -> Result<()> {
    h.join().map_err(|_| Error::ThreadPanicked(name))
}
