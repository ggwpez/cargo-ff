use crate::types::{Batch, CrateUnit, Edition, Result};
use crossbeam_channel::{Receiver, Sender};
use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

/// Group `CrateUnit`s by edition and emit `Batch`es packed by LPT
/// (Longest Processing Time first) bin-packing within a sliding window.
///
/// **Window.** Each per-edition bucket flushes once it accumulates
/// `batch_size * pack_multiplier` units. Within that window we run a
/// proper LPT pass: sort DESC by `size_bytes`, assign each unit to the
/// bin with smallest current total. This buys most of LPT's
/// makespan-balancing without buffering the entire workspace before
/// emitting anything — the first batches ship after one window fills
/// (~`window` units of producer work), not after the producer closes.
///
/// **Why per-edition.** rustfmt is invoked with one `--edition` flag per
/// process; mixing editions is a parse-correctness issue (a 2021 crate
/// using `let gen = 5;` fails under `--edition 2024`).
///
/// **Multiplier tradeoff.** Larger `M` = better balance within a window
/// (LPT shines when there are many jobs to pack) but later first
/// emission. M=1 degenerates to a single bin per window, defeating the
/// point. M=4 is a sensible default: 4 bins per window means LPT
/// actually has room to balance, while a window of `4 * batch_size`
/// crates fills quickly enough that workers don't sit idle long.
pub(crate) fn run(
    rx: Receiver<CrateUnit>,
    tx: Sender<Batch>,
    batch_size: usize,
    pack_multiplier: usize,
) -> Result<()> {
    let batch_size = batch_size.max(1);
    let pack_multiplier = pack_multiplier.max(1);
    let window = batch_size.saturating_mul(pack_multiplier);
    let mut buckets: HashMap<Edition, Vec<CrateUnit>> = HashMap::new();

    while let Ok(unit) = rx.recv() {
        let bucket = buckets.entry(unit.edition).or_default();
        bucket.push(unit);
        if bucket.len() >= window {
            let units = std::mem::take(bucket);
            if !flush_window(units, batch_size, &tx) {
                return Ok(());
            }
        }
    }

    for (_edition, units) in buckets {
        if !flush_window(units, batch_size, &tx) {
            return Ok(());
        }
    }

    Ok(())
}

/// LPT-pack `units` into `ceil(len / batch_size)` bins and emit each as
/// a `Batch`. Returns `false` if the receiver has gone away (caller
/// should stop).
fn flush_window(mut units: Vec<CrateUnit>, batch_size: usize, tx: &Sender<Batch>) -> bool {
    if units.is_empty() {
        return true;
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
        if tx.send(Batch { edition, units: bin }).is_err() {
            return false;
        }
    }
    true
}
