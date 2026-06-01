use crate::dispatch::PriorityQueue;
use crate::types::{Batch, CrateUnit, Edition};
use crossbeam_channel::Receiver;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

/// Units buffered per edition bucket before a window flush, as a multiple of
/// `batch_size`. A larger window gives the LPT packer more units to balance
/// within one flush, at the cost of latency before the first batch ships; 4
/// keeps early batches flowing while still smoothing size skew per flush.
pub(crate) const DEFAULT_PACK_MULTIPLIER: usize = 4;

/// Group `CrateUnit`s by edition and emit `Batch`es packed by LPT
/// (Longest Processing Time first) bin-packing within a sliding window.
///
/// **Solo dispatch.** A unit at or above `solo_threshold_bytes` ships
/// immediately as its own batch. Two reasons: amortizing spawn cost
/// (~40ms) is moot when one crate's formatting work already dwarfs it,
/// and shipping the giant on arrival lets a worker start on it from
/// roughly t=0 instead of waiting until the next window flush. The
/// downstream priority queue then ensures the giant is picked up before
/// any smaller packed batch even when other batches arrive earlier.
///
/// **Window.** Each per-edition bucket flushes once it accumulates
/// `batch_size * pack_multiplier` units. Within that window we run a
/// proper LPT pass: sort DESC by `size_bytes`, assign each unit to the
/// bin with smallest current total. This buys most of LPT's
/// makespan-balancing without buffering the entire workspace before
/// emitting anything — first batches ship after one window fills, not
/// after the producer closes.
///
/// **Why per-edition.** rustfmt is invoked with one `--edition` flag per
/// process; mixing editions is a parse-correctness issue (a 2021 crate
/// using `let gen = 5;` fails under `--edition 2024`).
pub(crate) fn run(
    rx: &Receiver<CrateUnit>,
    queue: &PriorityQueue,
    batch_size: usize,
    pack_multiplier: usize,
    solo_threshold_bytes: u64,
) {
    let batch_size = batch_size.max(1);
    let pack_multiplier = pack_multiplier.max(1);
    let window = batch_size.saturating_mul(pack_multiplier);
    let mut buckets: HashMap<Edition, Vec<CrateUnit>> = HashMap::new();

    while let Ok(unit) = rx.recv() {
        if unit.size_bytes >= solo_threshold_bytes {
            let edition = unit.edition;
            queue.push(Batch {
                edition,
                units: vec![unit],
            });
            continue;
        }

        let bucket = buckets.entry(unit.edition).or_default();
        bucket.push(unit);
        if bucket.len() >= window {
            flush_window(std::mem::take(bucket), batch_size, queue);
        }
    }

    for (_edition, units) in buckets {
        flush_window(units, batch_size, queue);
    }
}

/// LPT-pack `units` into `ceil(len / batch_size)` bins and push each as
/// a `Batch`.
fn flush_window(mut units: Vec<CrateUnit>, batch_size: usize, queue: &PriorityQueue) {
    if units.is_empty() {
        return;
    }
    let edition = units[0].edition;
    let n_batches = units.len().div_ceil(batch_size).max(1);
    units.sort_by_key(|u| Reverse(u.size_bytes));

    let mut bins: Vec<Vec<CrateUnit>> = vec![Vec::new(); n_batches];
    let mut heap: BinaryHeap<Reverse<(u64, usize)>> =
        (0..n_batches).map(|i| Reverse((0u64, i))).collect();

    for unit in units {
        // Invariant: the heap holds exactly `n_batches >= 1` bins — we pop one
        // and push it back every iteration — so it is never empty here.
        let Some(Reverse((size, idx))) = heap.pop() else {
            unreachable!("LPT heap seeded with n_batches >= 1 bins and refilled each iteration")
        };
        let new_size = size + unit.size_bytes;
        bins[idx].push(unit);
        heap.push(Reverse((new_size, idx)));
    }

    for bin in bins {
        if bin.is_empty() {
            continue;
        }
        queue.push(Batch {
            edition,
            units: bin,
        });
    }
}

#[cfg(test)]
mod tests {
    // Tests assert by panicking; `unwrap` is the idiomatic way to fail loudly.
    #![allow(clippy::unwrap_used)]

    use super::{CrateUnit, PriorityQueue, run};
    use crate::types::{Batch, Edition};
    use crossbeam_channel::bounded;
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn unit(edition: Edition, size_bytes: u64, name: &str) -> CrateUnit {
        CrateUnit {
            edition,
            manifest_dir: PathBuf::from(name),
            files: vec![PathBuf::from(name)],
            size_bytes,
        }
    }

    /// Drive `run` to completion against a fully-staged channel, then drain
    /// every batch the coalescer pushed.
    fn coalesce(units: Vec<CrateUnit>, batch_size: usize, solo_threshold: u64) -> Vec<Batch> {
        let (tx, rx) = bounded::<CrateUnit>(64);
        for u in units {
            tx.send(u).unwrap();
        }
        drop(tx); // closes the channel so `run`'s recv loop terminates
        let queue = PriorityQueue::new();
        run(&rx, &queue, batch_size, 4, solo_threshold);
        queue.close();
        let mut batches = Vec::new();
        while let Some(b) = queue.pop() {
            batches.push(b);
        }
        batches
    }

    #[test]
    fn units_at_or_above_threshold_ship_solo() {
        let batches = coalesce(
            vec![
                unit(Edition::E2021, 1_000_000, "giant"),
                unit(Edition::E2021, 10, "small"),
            ],
            3,
            1_000_000,
        );
        let solo = batches
            .iter()
            .find(|b| b.units.iter().any(|u| u.size_bytes >= 1_000_000));
        assert!(solo.is_some(), "the giant must be dispatched");
        assert_eq!(
            solo.unwrap().units.len(),
            1,
            "a unit at the solo threshold ships as its own batch",
        );
    }

    #[test]
    fn batches_never_mix_editions() {
        let mut units = Vec::new();
        for i in 0..6 {
            units.push(unit(Edition::E2018, 10 + i, &format!("a{i}")));
            units.push(unit(Edition::E2021, 10 + i, &format!("b{i}")));
        }
        let batches = coalesce(units, 3, 1_000_000);

        for b in &batches {
            let editions: HashSet<Edition> = b.units.iter().map(|u| u.edition).collect();
            assert_eq!(editions.len(), 1, "rustfmt takes one --edition per process");
        }
        let total: usize = batches.iter().map(|b| b.units.len()).sum();
        assert_eq!(total, 12, "every unit lands in exactly one batch");
    }

    #[test]
    fn packs_all_units_without_dropping_any() {
        // 5 small same-edition units, batch_size 2 → 3 non-empty bins.
        let units = (0..5)
            .map(|i| unit(Edition::E2021, 10, &format!("c{i}")))
            .collect();
        let batches = coalesce(units, 2, 1_000_000);

        let total: usize = batches.iter().map(|b| b.units.len()).sum();
        assert_eq!(total, 5);
        assert!(
            batches.iter().all(|b| !b.units.is_empty()),
            "the packer never emits an empty batch",
        );
    }
}
