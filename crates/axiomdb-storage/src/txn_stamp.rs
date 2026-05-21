//! Per-thread current-transaction stamp, shared by the storage backends.
//!
//! `write_page` reads it to stamp the writing txn into a page frame (project B REDO
//! recovery). A thread-local is correct under multi-writer because a statement runs
//! synchronously on one thread (no `spawn_blocking`/rayon between [`set`] and the
//! statement's `write_page`s). The executor sets it per statement and resets it to 0
//! at statement end. `MmapStorage` and `FaultInjectionStorage` share this so the stamp
//! behaves identically regardless of backend. `0` = non-transactional / system write.

use std::cell::Cell;

thread_local! {
    static CURRENT_TXN: Cell<u64> = const { Cell::new(0) };
}

/// Sets the current thread's transaction stamp.
pub(crate) fn set(txn_id: u64) {
    CURRENT_TXN.with(|c| c.set(txn_id));
}

/// Reads the current thread's transaction stamp (`0` = system write).
pub(crate) fn get() -> u64 {
    CURRENT_TXN.with(|c| c.get())
}
