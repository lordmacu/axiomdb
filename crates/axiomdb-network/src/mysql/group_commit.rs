//! WAL Group Commit background task.
//!
//! `spawn_group_commit_task` starts a long-running Tokio task that batches
//! WAL fsyncs across concurrent connections. See `commit_coordinator.rs` for
//! the coordinator design and `spec-3.19-wal-group-commit.md` for the full spec.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tracing::{debug, error};

use axiomdb_core::{error::DbError, TxnId};

use super::{commit_coordinator::CommitCoordinator, database::Database};

/// Spawns the group commit background task.
///
/// The task runs until the `Database` is dropped (detected via `Weak`).
/// It wakes on two conditions — whichever comes first:
/// - The `interval_ms` timer fires.
/// - The `CommitCoordinator::trigger` is notified (batch full).
///
/// On each wake it:
/// 1. Clones the coordinator handle (brief Database lock, no I/O).
/// 2. Drains the pending queue via coordinator's own Mutex (no Database lock).
/// 3. If empty: loops back to sleep.
/// 4. Acquires the Database lock.
/// 5. Calls `wal_flush_and_fsync()`.
/// 6. On success: calls `advance_committed(txn_ids)`.
/// 7. Releases the lock.
/// 8. Notifies all drained connections with `Ok(())` or `Err(...)`.
///
/// The `std::sync::MutexGuard` from `drain_pending()` is always dropped
/// before any `.await` point — no async-unsafe MutexGuard crossing.
pub fn spawn_group_commit_task(db: Arc<Mutex<Database>>, interval_ms: u64) -> JoinHandle<()> {
    // Downgrade to Weak so the task exits cleanly when Database is dropped.
    let db_weak = Arc::downgrade(&db);

    tokio::spawn(async move {
        // Bootstrap: grab initial coordinator handles (trigger, max_batch).
        // Re-fetched each iteration so changes to coordinator are respected.
        loop {
            // Obtain a strong reference; exit if Database was dropped.
            let db_strong = match db_weak.upgrade() {
                Some(d) => d,
                None => {
                    debug!("group commit task: Database dropped, exiting");
                    return;
                }
            };

            // Read coordinator state with a brief lock — no I/O here.
            // CommitCoordinator is Clone, so we get our own handle.
            let coordinator: Option<CommitCoordinator> = {
                let guard = db_strong.lock().await;
                guard.coordinator.clone()
            }; // Database lock released

            let coordinator = match coordinator {
                Some(c) => c,
                None => {
                    debug!("group commit task: coordinator removed, exiting");
                    return;
                }
            };

            // Wait: timer OR trigger (max_batch reached).
            // We hold no locks during this sleep.
            tokio::select! {
                _ = coordinator.trigger.notified() => {}
                _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {}
            }

            // Drain pending tickets using the coordinator's own std::sync::Mutex.
            // IMPORTANT: the guard is dropped (via block end) before any .await,
            // so no std::sync::MutexGuard crosses an await point.
            let tickets = coordinator.drain_pending();

            if tickets.is_empty() {
                continue;
            }

            let batch_size = tickets.len();
            debug!(batch_size, "group commit: fsyncing batch");

            // Re-acquire strong ref for the fsync work.
            let db_strong = match db_weak.upgrade() {
                Some(d) => d,
                None => {
                    // Database dropped while we had tickets — send errors to all.
                    for ticket in tickets {
                        let _ = ticket.reply.send(Err(DbError::WalGroupCommitFailed {
                            message: "database shut down before fsync".into(),
                        }));
                    }
                    return;
                }
            };

            // Acquire Database lock, flush+fsync, advance max_committed.
            // The lock is held for the duration of the fsync (~0.1–10ms).
            let fsync_result: Result<(), DbError> = {
                let mut guard = db_strong.lock().await;
                let r = guard.txn.wal_flush_and_fsync();
                if r.is_ok() {
                    let ids: Vec<TxnId> = tickets.iter().map(|t| t.txn_id).collect();
                    guard.txn.advance_committed(&ids);
                }
                r
            }; // Database lock released here

            // Notify all waiting connections. Errors are cloned from the message
            // string so DbError (which wraps io::Error, non-Clone) can be sent
            // to multiple receivers.
            match &fsync_result {
                Ok(()) => {
                    debug!(batch_size, "group commit: fsync ok");
                    for ticket in tickets {
                        let _ = ticket.reply.send(Ok(()));
                    }
                }
                Err(e) => {
                    // max_committed was NOT advanced — no connection gets Ok.
                    let msg = e.to_string();
                    error!(
                        batch_size,
                        error = %msg,
                        "group commit: fsync FAILED — database in degraded state"
                    );
                    for ticket in tickets {
                        let _ = ticket.reply.send(Err(DbError::WalGroupCommitFailed {
                            message: msg.clone(),
                        }));
                    }
                }
            }
        }
    })
}
