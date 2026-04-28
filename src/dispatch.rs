//! Priority dispatch from coalescer to workers.
//!
//! Replaces a FIFO channel with a size-ordered queue. Workers pop the
//! largest-`size_bytes` batch pending, so a long-pole solo giant doesn't
//! sit behind several already-flushed packed batches and become the
//! wall-time tail. With this in place we don't need a 2-priority-level
//! distinction — every batch is just compared on its size, and giants
//! win naturally because their batch's size is huge.
//!
//! Built on `Mutex<BinaryHeap>` + `Condvar`. At our scale (12 workers,
//! ~600 batches, work units 10–100ms) lock contention is invisible
//! relative to formatting time, and the simplicity is worth more than
//! lock-free trickery.

use crate::types::Batch;
use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::sync::{Condvar, Mutex};

pub(crate) struct PriorityQueue {
    inner: Mutex<Inner>,
    cv: Condvar,
}

struct Inner {
    heap: BinaryHeap<Item>,
    closed: bool,
}

struct Item {
    size: u64,
    batch: Batch,
}

// `BinaryHeap` is a max-heap, so larger `size` = higher priority falls
// out for free. Ordering ignores the `batch` field — ties between
// equally-sized batches resolve in heap-internal (unspecified) order,
// which is fine since equally-sized batches imply equal expected work.
impl PartialEq for Item {
    fn eq(&self, other: &Self) -> bool {
        self.size == other.size
    }
}
impl Eq for Item {}
impl PartialOrd for Item {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Item {
    fn cmp(&self, other: &Self) -> Ordering {
        self.size.cmp(&other.size)
    }
}

impl PriorityQueue {
    pub(crate) fn new() -> Self {
        PriorityQueue {
            inner: Mutex::new(Inner {
                heap: BinaryHeap::new(),
                closed: false,
            }),
            cv: Condvar::new(),
        }
    }

    pub(crate) fn push(&self, batch: Batch) {
        let size = batch.size_bytes();
        let mut inner = self.inner.lock().expect("queue mutex poisoned");
        inner.heap.push(Item { size, batch });
        self.cv.notify_one();
    }

    /// Mark the queue closed: no more pushes will arrive. Workers
    /// blocked in `pop()` wake up and observe `None` once the heap
    /// drains.
    pub(crate) fn close(&self) {
        let mut inner = self.inner.lock().expect("queue mutex poisoned");
        inner.closed = true;
        self.cv.notify_all();
    }

    /// Block until a batch is available or the queue is closed and
    /// empty. Returns `None` only after `close()` has been called and
    /// every queued batch has been popped.
    pub(crate) fn pop(&self) -> Option<Batch> {
        let mut inner = self.inner.lock().expect("queue mutex poisoned");
        loop {
            if let Some(item) = inner.heap.pop() {
                return Some(item.batch);
            }
            if inner.closed {
                return None;
            }
            inner = self.cv.wait(inner).expect("queue mutex poisoned");
        }
    }
}
