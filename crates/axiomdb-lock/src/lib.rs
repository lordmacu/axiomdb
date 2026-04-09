//! Row-level lock manager for AxiomDB.
//!
//! InnoDB-inspired architecture with PostgreSQL enhancements:
//! - 5 lock modes (IS, IX, S, X, AutoIncrement) with const conflict matrix
//! - 64-shard hash table for record locks, 16-shard for table locks
//! - Per-page slot bitmap (InnoDB `lock_rec_t` pattern)
//! - FIFO wait queue with async `tokio::sync::Notify`
//! - Per-transaction lock tracking for bulk release on COMMIT/ROLLBACK
//!
//! ## Phase 40.5 scope
//!
//! Core lock manager infrastructure. Does NOT include:
//! - Deadlock detection (40.6)
//! - Implicit lock detection via RowHeader (40.7/40.11)
//! - Executor integration (40.11)

pub mod bitmap;
pub mod entry;
pub mod manager;
pub mod mode;
pub mod txn_locks;

pub use bitmap::SlotBitmap;
pub use entry::{LockEntry, LockQueue, LockWaiter, WaitAbortReason};
pub use manager::{LockManager, LockResult};
pub use mode::{conflicts, LockFlags, LockMode, CONFLICT_MATRIX};
pub use txn_locks::{LockRef, TxnLockTracker};
