pub mod types;

mod discover;
mod exec;
mod report;

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
    // CrateUnit/CrateResult sizes (entry-point lists, captured
    // stdout) and stays sub-MB even for big workspaces.
    let cap = cfg.channel_capacity.unwrap_or(512);

    let (unit_tx, unit_rx) = bounded::<types::CrateUnit>(cap);
    let (result_tx, result_rx) = bounded::<types::CrateResult>(cap);

    let cfg_d = cfg.clone();
    let producer = thread::spawn(move || discover::run(&cfg_d, unit_tx));

    let mut workers = Vec::with_capacity(n);
    for _ in 0..n {
        let rx = unit_rx.clone();
        let tx = result_tx.clone();
        let cfg_w = cfg.clone();
        workers.push(thread::spawn(move || exec::worker(rx, tx, &cfg_w)));
    }
    drop(unit_rx);
    drop(result_tx);

    let report = report::aggregate(result_rx);

    // Producer errors are surfaced; worker panics are turned into an Error.
    match producer.join() {
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
