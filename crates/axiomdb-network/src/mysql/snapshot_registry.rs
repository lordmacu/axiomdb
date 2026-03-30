//! Snapshot registry for epoch-based page reclamation (Phase 7.8).
//!
//! Tracks active snapshot IDs across all connections. Each connection gets a
//! fixed slot (indexed by connection ID). Operations are lock-free using
//! `AtomicU64` — no additional mutex beyond the existing `RwLock<Database>`.
//!
//! ## Usage
//!
//! ```text
//! // Before query execution:
//! registry.register(conn_id, snapshot_id);
//!
//! // After query completes:
//! registry.unregister(conn_id);
//!
//! // In flush():
//! let oldest = registry.oldest_active();
//! storage.release_deferred_frees(oldest);
//! ```
//!
//! ## Design reference
//!
//! DuckDB tracks `lowest_active_start` via an active transaction list. InnoDB
//! uses `clone_oldest_view()` to merge all active ReadViews. SQLite uses
//! `nFetchOut` + `aReadMark[]`. AxiomDB uses a fixed-size atomic slot array
//! for O(N) scan without locking — the simplest correct approach.

use std::sync::atomic::{AtomicU64, Ordering};

/// Default maximum connections (slots in the registry).
pub const DEFAULT_MAX_CONNECTIONS: usize = 1024;

/// Thread-safe registry of active snapshot IDs.
///
/// Each connection has a dedicated slot. A slot value of 0 means "idle" (no
/// active snapshot). Any non-zero value is the snapshot_id the connection is
/// currently reading with.
pub struct SnapshotRegistry {
    slots: Vec<AtomicU64>,
}

impl SnapshotRegistry {
    /// Creates a registry with `max_connections` slots, all initialized to 0.
    pub fn new(max_connections: usize) -> Self {
        let mut slots = Vec::with_capacity(max_connections);
        for _ in 0..max_connections {
            slots.push(AtomicU64::new(0));
        }
        Self { slots }
    }

    /// Registers an active snapshot for a connection.
    ///
    /// Called before query execution. `conn_id` is the connection's 0-based
    /// index (typically `conn_id % max_connections`).
    pub fn register(&self, conn_id: u32, snapshot_id: u64) {
        let idx = conn_id as usize % self.slots.len();
        self.slots[idx].store(snapshot_id, Ordering::Release);
    }

    /// Clears the active snapshot for a connection.
    ///
    /// Called after query completes. The slot becomes available for the next
    /// query on this connection.
    pub fn unregister(&self, conn_id: u32) {
        let idx = conn_id as usize % self.slots.len();
        self.slots[idx].store(0, Ordering::Release);
    }

    /// Returns the minimum active snapshot ID across all connections,
    /// or `u64::MAX` if no connection is currently reading.
    ///
    /// Used by `flush()` to determine which deferred-free pages are safe
    /// to release. Pages freed at epoch ≤ `oldest_active()` can be returned
    /// to the freelist; pages freed at a later epoch must remain queued.
    pub fn oldest_active(&self) -> u64 {
        let mut min_snap = u64::MAX;
        for slot in &self.slots {
            let val = slot.load(Ordering::Acquire);
            if val != 0 && val < min_snap {
                min_snap = val;
            }
        }
        min_snap
    }

    /// Returns the number of connections with active snapshots.
    pub fn active_count(&self) -> usize {
        self.slots
            .iter()
            .filter(|s| s.load(Ordering::Relaxed) != 0)
            .count()
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_empty_returns_max() {
        let reg = SnapshotRegistry::new(8);
        assert_eq!(reg.oldest_active(), u64::MAX);
        assert_eq!(reg.active_count(), 0);
    }

    #[test]
    fn test_single_reader() {
        let reg = SnapshotRegistry::new(8);
        reg.register(0, 42);
        assert_eq!(reg.oldest_active(), 42);
        assert_eq!(reg.active_count(), 1);
    }

    #[test]
    fn test_multiple_readers_returns_min() {
        let reg = SnapshotRegistry::new(8);
        reg.register(0, 100);
        reg.register(1, 50);
        reg.register(2, 200);
        assert_eq!(reg.oldest_active(), 50);
        assert_eq!(reg.active_count(), 3);
    }

    #[test]
    fn test_unregister_clears_slot() {
        let reg = SnapshotRegistry::new(8);
        reg.register(0, 42);
        reg.unregister(0);
        assert_eq!(reg.oldest_active(), u64::MAX);
        assert_eq!(reg.active_count(), 0);
    }

    #[test]
    fn test_oldest_after_unregister() {
        let reg = SnapshotRegistry::new(8);
        reg.register(0, 10);
        reg.register(1, 20);
        reg.register(2, 30);

        reg.unregister(0); // remove the oldest
        assert_eq!(reg.oldest_active(), 20);
        assert_eq!(reg.active_count(), 2);
    }

    #[test]
    fn test_register_overwrites_slot() {
        let reg = SnapshotRegistry::new(8);
        reg.register(0, 100);
        reg.register(0, 200); // new query on same connection
        assert_eq!(reg.oldest_active(), 200);
    }

    #[test]
    fn test_conn_id_wraps_with_modulo() {
        let reg = SnapshotRegistry::new(4);
        reg.register(4, 50); // 4 % 4 = 0
        assert_eq!(reg.oldest_active(), 50);
        assert_eq!(reg.active_count(), 1);
    }
}
