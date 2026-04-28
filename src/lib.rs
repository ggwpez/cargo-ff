pub mod types;

mod coalesce;
mod discover;
mod exec;
mod report;
mod size;

#[cfg(feature = "cli")]
pub mod cli;

pub use types::{Config, Edition, Error, FileFailure, MessageFormat, Report, Result};

#[doc(hidden)]
pub mod __test_only {
    pub use crate::discover::run as discover_run;
}

use crossbeam_channel::bounded;
use std::thread;

pub fn run(cfg: &Config) -> Result<Report> {
    let n = cfg
        .workers
        .or_else(|| thread::available_parallelism().ok().map(|p| p.get()))
        .unwrap_or(1);
    // 512 is empirically a no-op vs the n*2 default on Polkadot
    // (~580 crates) but keeps the producer from ever blocking on
    // small workspaces — channel size doesn't affect wall time, so
    // pick generous and forget about it. Memory is bounded by
    // CrateUnit/BatchResult sizes (entry-point lists, captured
    // stdout) and stays sub-MB even for big workspaces.
    let cap = cfg.channel_capacity.unwrap_or(512);
    // Default of 3 is the conservative sweet spot from the spawn-vs-work
    // tradeoff on sdk2: per-crate work is ~11ms vs ~40ms spawn, so
    // batches of 3 amortize spawn ~3× while keeping per-batch work
    // (~33ms) well below the level where one straggling batch dominates.
    // Larger batches benefit from size-aware packing, which we don't yet
    // do — so 3 is the safe streaming default.
    let batch_size = cfg.batch_size.unwrap_or(3);

    let (unit_tx, unit_rx) = bounded::<types::CrateUnit>(cap);
    let (batch_tx, batch_rx) = bounded::<types::Batch>(cap);
    let (result_tx, result_rx) = bounded::<types::BatchResult>(cap);

    let cfg_d = cfg.clone();
    let producer = thread::spawn(move || discover::run(&cfg_d, unit_tx));

    // 1MB threshold ≈ p98 of crate sizes on polkadot-sdk (~10 crates go
    // solo). Tunable later if needed; this is the round number that
    // covers the giants without trigger-happily soloing medium crates.
    let solo_threshold = 1_000_000u64;
    let coalescer =
        thread::spawn(move || coalesce::run(unit_rx, batch_tx, batch_size, 4, solo_threshold));

    let mut workers = Vec::with_capacity(n);
    for _ in 0..n {
        let rx = batch_rx.clone();
        let tx = result_tx.clone();
        let cfg_w = cfg.clone();
        workers.push(thread::spawn(move || exec::worker(rx, tx, &cfg_w)));
    }
    drop(batch_rx);
    drop(result_tx);

    let report = report::aggregate(result_rx);

    match producer.join() {
        Ok(res) => res?,
        Err(_) => return Err(Error::WorkerPanic),
    }
    match coalescer.join() {
        Ok(res) => res?,
        Err(_) => return Err(Error::WorkerPanic),
    }
    for w in workers {
        if w.join().is_err() {
            return Err(Error::WorkerPanic);
        }
    }
    Ok(report)
}
