use crate::dispatch::PriorityQueue;
use crate::types::{Batch, CrateUnit, Edition, Result};
use crossbeam_channel::Receiver;
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

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
    rx: Receiver<CrateUnit>,
    queue: &PriorityQueue,
    batch_size: usize,
    pack_multiplier: usize,
    solo_threshold_bytes: u64,
) -> Result<()> {
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

    Ok(())
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
        let Reverse((size, idx)) = heap.pop().expect("bins non-empty");
        let new_size = size + unit.size_bytes;
        bins[idx].push(unit);
        heap.push(Reverse((new_size, idx)));
    }

    for bin in bins {
        if bin.is_empty() {
            continue;
        }
        queue.push(Batch { edition, units: bin });
    }
}
