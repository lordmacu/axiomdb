//! Latch coupling infrastructure for the B-tree.
//!
//! Phase 40.8 implements read-path latch coupling and write-path page
//! latching using the [`PageLockTable`] from Phase 40.3. This enables
//! concurrent SELECTs to descend in parallel while a single writer
//! modifies the tree (the current executor model under
//! `Arc<RwLock<Database>>`).
//!
//! ## Latch modes
//!
//! - [`TreeLatchMode::SearchLeaf`]: read-only descent (`lookup`, range scan).
//!   S-latch is acquired at each level; the parent is released after the
//!   child latch is acquired (coupling). Final leaf is read under S-latch.
//!
//! - [`TreeLatchMode::ModifyLeaf`]: optimistic write. S-latch on internal
//!   nodes, X-latch at leaf. Used by `insert`/`delete` in 40.8b when the
//!   leaf is not expected to split/merge.
//!
//! - [`TreeLatchMode::ModifyTree`]: pessimistic write. X-latch at every
//!   level via lexical scope (parent held during child recursion). Used
//!   by 40.8b when a split or merge needs to propagate upward.
//!
//! ## Coupling protocol
//!
//! The recursive B-tree code naturally implements latch coupling via
//! Rust's lexical scope: a latch guard is acquired on entry to each
//! recursive level and dropped when the function returns. Child
//! recursion happens WHILE the parent guard is still alive, so the
//! parent latch is effectively "held while descending to child".
//!
//! For the iterative `lookup` path, the caller manually manages two
//! guards: a `current_guard` holding the current level's latch, and
//! a freshly-acquired `child_guard` which replaces it after the parent
//! is dropped.
//!
//! ## Safety under single writer
//!
//! As of Phase 40.8, writers on the same B-tree are serialized by
//! `&mut self` on `BTree` AND by `Arc<RwLock<Database>>` in the
//! executor. This means:
//!
//! - Read-path latch coupling is sufficient for safety (no writer
//!   can be mid-update when a reader descends).
//! - Write-path X-latch is defensive — it prevents a partial read
//!   from seeing an inconsistent page during a write.
//! - True concurrent writers (40.10) will require the full
//!   optimistic/pessimistic protocol from 40.8b.

/// Latch mode for B-tree descent.
///
/// Determines whether to acquire shared (S) or exclusive (X) latches
/// at each level of the tree. See module-level docs for details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum TreeLatchMode {
    /// Read-only descent: S-latch coupling.
    #[allow(dead_code)]
    SearchLeaf,
    /// Optimistic write: S-latch internals, X-latch leaf.
    /// Reserved for 40.8b when concurrent writers are enabled.
    #[allow(dead_code)]
    ModifyLeaf,
    /// Pessimistic write: X-latch every level via lexical scope.
    /// Reserved for 40.8b.
    #[allow(dead_code)]
    ModifyTree,
}

/// Outcome of an optimistic operation that may need pessimistic retry.
///
/// Reserved for 40.8b when concurrent writers are enabled.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) enum OptimisticOutcome<T> {
    /// Operation completed successfully under optimistic mode.
    Done(T),
    /// Operation needs pessimistic retry (split/merge propagation required).
    NeedPessimistic,
}
