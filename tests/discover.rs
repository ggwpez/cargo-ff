//! Producer-only smoke test. Runs discover against the current crate
//! (a tiny workspace) and against an env-supplied path for a real workspace.
//!
//! Note: `CrateUnit.files` holds *target entry points* (lib.rs, main.rs,
//! tests/it.rs, …), not every .rs file. rustfmt walks the `mod` tree
//! from each entry point itself.

use cargo_ffmt::Config;
use cargo_ffmt::types::CrateUnit;
use crossbeam_channel::bounded;
use std::path::PathBuf;
use std::time::Instant;

fn drain_discover(cfg: &Config) -> (Vec<CrateUnit>, Vec<u128>) {
    let (tx, rx) = bounded::<CrateUnit>(64);
    let cfg_d = cfg.clone();
    let start = Instant::now();
    let producer = std::thread::spawn(move || cargo_ffmt::__test_only::discover_run(&cfg_d, tx));

    let mut units = Vec::new();
    let mut send_times = Vec::new();
    while let Ok(unit) = rx.recv() {
        send_times.push(start.elapsed().as_micros());
        units.push(unit);
    }
    producer.join().unwrap().unwrap();
    (units, send_times)
}

#[test]
fn discover_runs_on_self() {
    let cfg = Config::default();
    let (units, _) = drain_discover(&cfg);
    // Self repo has 1 crate. Targets: lib + bin + 1 integration test = 3 entry points.
    assert_eq!(units.len(), 1, "expected exactly one CrateUnit");
    let entries = &units[0].files;
    assert!(
        entries.len() >= 3,
        "expected ≥3 target entry points, got {}",
        entries.len()
    );
}

#[test]
#[ignore]
fn discover_on_big_repo() {
    let path = match std::env::var("FFMT_BIG_REPO") {
        Ok(p) => PathBuf::from(p),
        Err(_) => {
            eprintln!("FFMT_BIG_REPO not set; skipping");
            return;
        }
    };
    let cfg = Config {
        manifest_path: Some(path.join("Cargo.toml")),
        ..Config::default()
    };
    let (units, send_times) = drain_discover(&cfg);
    let total_entries: usize = units.iter().map(|u| u.files.len()).sum();
    let unique_entries: std::collections::HashSet<&std::path::PathBuf> =
        units.iter().flat_map(|u| u.files.iter()).collect();
    eprintln!(
        "discovered {} crates, {total_entries} target entry points, {} unique; first emit at {}us, last at {}us",
        units.len(),
        unique_entries.len(),
        send_times.first().copied().unwrap_or(0),
        send_times.last().copied().unwrap_or(0),
    );
    assert_eq!(
        total_entries,
        unique_entries.len(),
        "cross-crate dedup broken"
    );
    assert!(!units.is_empty());
    // Sanity: emissions should be incremental, not all at once.
    if send_times.len() > 10 {
        let first = send_times[0];
        let last = *send_times.last().unwrap();
        assert!(
            last > first + 1000,
            "emissions look batched: first={first}us last={last}us"
        );
    }
}
