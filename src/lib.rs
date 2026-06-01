pub mod types;

mod cache;
mod coalesce;
mod discover;
mod dispatch;
mod exec;
mod report;
mod size;
mod style;

#[cfg(feature = "cli")]
pub mod cli;

pub use types::{Config, Edition, Error, FileFailure, Report, Result};

/// Internal test surface — **not** part of the public API despite being `pub`.
///
/// Rust integration tests compile as a separate crate, so exercising a private
/// module like [`discover`](crate::discover) from `tests/` needs a `pub`
/// escape hatch. The `__` prefix plus `#[doc(hidden)]` keep it out of the docs
/// and signal "do not use": it carries no stability guarantee and may change
/// or disappear in any release. This is the conventional idiom for
/// integration-testing crate internals.
#[doc(hidden)]
pub mod __test_only {
    use crate::types::{Config, CrateUnit, Result};
    use crossbeam_channel::Sender;

    pub fn discover_run(cfg: &Config, tx: &Sender<CrateUnit>) -> Result<()> {
        crate::discover::run(cfg, tx).map(drop)
    }
}

use crossbeam_channel::bounded;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

/// Format every selected crate in the workspace, dispatching rustfmt across a
/// pool of worker threads, and return the aggregated [`Report`].
///
/// # Errors
///
/// Returns [`Error`] when `cargo metadata` fails, a requested package or
/// edition is unknown, `cfg.workers` is `Some(0)`, or a producer/worker thread
/// panics.
pub fn run(cfg: &Config) -> Result<Report> {
    if matches!(cfg.workers, Some(0)) {
        return Err(Error::InvalidWorkers(0));
    }

    if cfg.warnings {
        warn_if_stable_rustfmt();
    }

    let n = cfg
        .workers
        .or_else(|| thread::available_parallelism().ok().map(std::num::NonZeroUsize::get))
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
    // Each thread owns its channel endpoint; dropping it when the closure
    // exits is what propagates "no more work" down the pipeline.
    let producer = thread::spawn(move || discover::run(&cfg_d, &unit_tx));

    // Coalescer pushes batches into `queue`; closes it on exit so
    // workers' `pop()` returns `None` once everything is drained.
    let coalescer_q = queue.clone();
    let coalescer = thread::spawn(move || {
        coalesce::run(
            &unit_rx,
            &coalescer_q,
            batch_size,
            coalesce::DEFAULT_PACK_MULTIPLIER,
            solo_threshold,
        );
        coalescer_q.close();
    });

    let mut workers = Vec::with_capacity(n);
    for _ in 0..n {
        let q = queue.clone();
        let tx = result_tx.clone();
        let cfg_w = cfg.clone();
        workers.push(thread::spawn(move || exec::worker(&q, &tx, &cfg_w)));
    }
    drop(result_tx);

    let report = report::aggregate(result_rx);

    let cache_opt = join_fallible(producer, "producer")?;
    join_void(coalescer, "coalescer")?;
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

/// `rustfmt --version` prints e.g. `rustfmt 1.8.0-nightly (…)`. If the
/// `nightly` marker is absent, unstable `rustfmt.toml` options are silently
/// dropped and output diverges from `cargo +nightly fmt`.
fn warn_if_stable_rustfmt() {
    use std::fmt::Write as _;
    use std::io::Write as _;
    let Ok(out) = std::process::Command::new("rustfmt")
        .arg("--version")
        .output()
    else {
        return;
    };
    if !out.status.success() {
        return;
    }
    let version = String::from_utf8_lossy(&out.stdout);
    if version.contains("nightly") {
        return;
    }
    let palette = style::palette();
    let (w, wr) = (palette.warning.render(), palette.warning.render_reset());
    let (n, nr) = (palette.note.render(), palette.note.render_reset());
    let (h, hr) = (palette.help.render(), palette.help.render_reset());
    let mut buf = String::new();
    let _ = writeln!(buf, "{w}warning{wr}: rustfmt on PATH is the stable channel");
    let _ = writeln!(buf, "   {n}note{nr}: `{}`", version.trim());
    let _ = writeln!(
        buf,
        "   {n}note{nr}: unstable rustfmt.toml options will be silently ignored"
    );
    let _ = writeln!(
        buf,
        "   {h}help{hr}: run via `cargo +nightly ff` for parity with `cargo +nightly fmt`"
    );
    buf.push('\n');
    let _ = std::io::stderr().write_all(buf.as_bytes());
}
