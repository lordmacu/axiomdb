//! Opt-in I/O stats for write-path diagnostics (`axiomdb_bench --diagnose-*`).
//!
//! Zero-cost in production: each `pwrite` does a single `Relaxed` atomic load of
//! `ARMED`; timing is only taken when a diagnostic explicitly `arm()`s it. Used
//! to answer "how much of the commit is `pwrite` I/O (deferrable to a background
//! checkpoint) vs B-tree CPU work (not deferrable)".

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

static ARMED: AtomicBool = AtomicBool::new(false);
static PWRITE_NS: AtomicU64 = AtomicU64::new(0);
static PWRITE_COUNT: AtomicU64 = AtomicU64::new(0);
static PWRITE_BYTES: AtomicU64 = AtomicU64::new(0);

/// Enable `pwrite` timing. Call before a measured region.
pub fn arm() {
    ARMED.store(true, Ordering::Relaxed);
}

/// Disable timing (back to zero-cost).
pub fn disarm() {
    ARMED.store(false, Ordering::Relaxed);
}

/// `true` while timing is enabled. One `Relaxed` load on the write hot path.
#[inline]
pub fn armed() -> bool {
    ARMED.load(Ordering::Relaxed)
}

/// Zero the accumulators. Call right before a measured region.
pub fn reset() {
    PWRITE_NS.store(0, Ordering::Relaxed);
    PWRITE_COUNT.store(0, Ordering::Relaxed);
    PWRITE_BYTES.store(0, Ordering::Relaxed);
}

/// Record one `pwrite` (called only when armed).
#[inline]
pub fn record_pwrite(ns: u64, bytes: u64) {
    PWRITE_NS.fetch_add(ns, Ordering::Relaxed);
    PWRITE_COUNT.fetch_add(1, Ordering::Relaxed);
    PWRITE_BYTES.fetch_add(bytes, Ordering::Relaxed);
}

/// Snapshot `(total pwrite ns, pwrite count, total bytes)`.
pub fn snapshot() -> (u64, u64, u64) {
    (
        PWRITE_NS.load(Ordering::Relaxed),
        PWRITE_COUNT.load(Ordering::Relaxed),
        PWRITE_BYTES.load(Ordering::Relaxed),
    )
}
