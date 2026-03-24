//! CommitCoordinator — batches WAL fsyncs across concurrent connections.
//!
//! ## Purpose
//!
//! Under the default single-fsync-per-commit model, N concurrent DML
//! transactions pay N fsyncs in series. The `CommitCoordinator` amortises
//! this cost: connections write their Commit WAL entries to the `BufWriter`
//! (fast, in RAM) and register with the coordinator instead of fsyncing
//! inline. A background Tokio task wakes every `interval_ms` (or sooner
//! when `max_batch` connections are waiting), acquires the Database lock,
//! executes a single flush+fsync, advances `max_committed` for all
//! transactions in the batch, then notifies all waiting connections.
//!
//! ## Durability guarantee
//!
//! A connection does **not** receive `Ok` until the fsync covering its
//! Commit entry completes. If fsync fails, every connection in the batch
//! receives `Err(WalGroupCommitFailed)` and `max_committed` is not advanced.
//!
//! ## Disabled mode
//!
//! When `group_commit_interval_ms = 0`, no `CommitCoordinator` is created
//! and no background task is spawned. The existing inline-fsync path in
//! `TxnManager::commit()` is used unchanged.

use std::sync::{Arc, Mutex};

use tokio::sync::{oneshot, Notify};

use axiomdb_core::{error::DbError, TxnId};

// ── CommitTicket ─────────────────────────────────────────────────────────────

/// A single pending group-commit entry: the txn_id waiting for fsync
/// confirmation and the channel to notify the waiting connection.
pub(crate) struct CommitTicket {
    pub txn_id: TxnId,
    pub reply: oneshot::Sender<Result<(), DbError>>,
}

// ── CommitCoordinator ────────────────────────────────────────────────────────

/// Batches WAL fsyncs across concurrent connections.
///
/// Cheap to clone — the pending queue and trigger are reference-counted
/// internally. Cloning gives a second handle to the same coordinator state,
/// which is how the background task and each connection share the queue.
#[derive(Clone)]
pub struct CommitCoordinator {
    /// Queue of connections waiting for fsync confirmation.
    /// `std::sync::Mutex` (not Tokio) so `register_pending` can be called
    /// from synchronous code inside `Database::execute_query` without async.
    pub(crate) pending: Arc<Mutex<Vec<CommitTicket>>>,
    /// Notified when `pending.len() >= max_batch`, waking the background task
    /// immediately instead of waiting for the timer.
    pub(crate) trigger: Arc<Notify>,
    /// Maximum batch size before a forced early fsync.
    max_batch: usize,
}

impl CommitCoordinator {
    /// Creates a new `CommitCoordinator` with the given max batch size.
    ///
    /// The background task is **not** started here — call
    /// `spawn_group_commit_task` after creating the coordinator.
    pub fn new(max_batch: usize) -> Self {
        Self {
            pending: Arc::new(Mutex::new(Vec::new())),
            trigger: Arc::new(Notify::new()),
            max_batch,
        }
    }

    /// Registers `txn_id` as waiting for fsync confirmation.
    ///
    /// Returns a receiver that resolves to:
    /// - `Ok(())` when the fsync covering this entry completes.
    /// - `Err(WalGroupCommitFailed)` if the fsync fails.
    ///
    /// Synchronous (never blocks) — uses `std::sync::Mutex` internally.
    /// If `pending.len() + 1 >= max_batch` after this registration,
    /// `trigger` is notified to wake the background task immediately.
    pub fn register_pending(&self, txn_id: TxnId) -> oneshot::Receiver<Result<(), DbError>> {
        let (tx, rx) = oneshot::channel();
        let mut pending = self
            .pending
            .lock()
            .expect("CommitCoordinator lock poisoned");
        pending.push(CommitTicket { txn_id, reply: tx });
        let len = pending.len();
        drop(pending);
        if len >= self.max_batch {
            self.trigger.notify_one();
        }
        rx
    }

    /// Drains and returns all pending tickets atomically.
    ///
    /// Called by the background task before acquiring the Database lock.
    /// Returns an empty Vec if no connections are waiting (task loops cheaply).
    pub(crate) fn drain_pending(&self) -> Vec<CommitTicket> {
        let mut pending = self
            .pending
            .lock()
            .expect("CommitCoordinator lock poisoned");
        std::mem::take(&mut *pending)
    }
}
