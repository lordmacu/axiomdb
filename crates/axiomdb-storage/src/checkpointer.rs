//! Background frame-log checkpointer (project B, subphase 6f — Lever 2 / Task 1).
//!
//! Frame-only redo ([`RedoMode::FrameOnly`](crate::config::RedoMode)) appends every
//! page write to a frame log and lets a *checkpoint* copy committed frames back to the
//! main file and recycle the log. Without an automatic trigger that log grows
//! unbounded → disk exhaustion. This module supplies the trigger as a dedicated
//! background thread, modelled on PostgreSQL's checkpointer / InnoDB's page cleaner
//! and SQLite's WAL auto-checkpoint threshold (`research/sqlite/src/wal.c`,
//! `sqlite3WalDefaultHook` fires `sqlite3_wal_checkpoint` once the WAL passes
//! `pPager->nWalCkpt` frames):
//!
//! - The thread runs the checkpoint **off the commit path**, so commits never pay the
//!   apply+fsync latency — the frame-only autocommit win is preserved without latency
//!   spikes.
//! - The synchronous **back-pressure** at the commit boundary (subphase 6f step 3) is
//!   the hard guarantee that bounds the log even if this thread falls behind or dies.
//!   The background thread keeps that path off the steady state; it is not the safety
//!   mechanism.
//!
//! ## Wake model
//!
//! The thread sleeps on a [`Condvar`] with a timeout. It wakes on:
//! - a frame append crossing the soft threshold ([`CheckpointTrigger::note`], a cheap
//!   lock-free size compare on the write path — it only locks + notifies once the log
//!   has actually grown past `soft`);
//! - the poll timeout — a robustness fallback so a missed notify still re-checks
//!   within one interval;
//! - [`CheckpointTrigger`] stop on shutdown, which also forces a final checkpoint.
//!
//! On each wake the thread calls
//! [`maybe_checkpoint_frames`](crate::engine::StorageEngine::maybe_checkpoint_frames),
//! the single guarded entry shared with the back-pressure path, so all triggered
//! checkpoints serialize on the storage's checkpoint lock. `force` is set only on
//! shutdown, draining the log so a restart begins with a current main file.

use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::engine::StorageEngine;

/// Default background poll interval: the thread re-checks the log size at least this
/// often even without a wake notification (the robustness fallback; the soft-threshold
/// notify drives the responsive path).
pub const DEFAULT_CHECKPOINT_POLL: Duration = Duration::from_millis(200);

/// Shared wake primitive between the storage write path (producer:
/// [`note`](Self::note)) and the [`FrameCheckpointer`] thread (consumer). Cheap to
/// poke from the hot path: under the soft threshold `note` is a single integer compare
/// with no lock.
pub struct CheckpointTrigger {
    /// Soft threshold in bytes (typically `DbConfig.max_wal_size_mb`). A frame append
    /// whose log offset reaches this wakes the checkpointer. Immutable after
    /// construction.
    soft_bytes: u64,
    state: Mutex<TriggerState>,
    cv: Condvar,
}

#[derive(Default)]
struct TriggerState {
    /// Set by [`note`](CheckpointTrigger::note) / `signal`; consumed by the thread to
    /// mean "re-check the log size now".
    wake: bool,
    /// Set by `stop`; the thread runs a final forced checkpoint and exits.
    stop: bool,
}

impl CheckpointTrigger {
    fn new(soft_bytes: u64) -> Self {
        Self {
            soft_bytes,
            state: Mutex::new(TriggerState::default()),
            cv: Condvar::new(),
        }
    }

    /// Hot-path hook: called after a frame append with the log's current written
    /// offset. Wakes the checkpointer only when the log has reached the soft
    /// threshold — the common (under-threshold) case is a single compare, no lock.
    /// Over-threshold the wake is idempotent (the thread coalesces repeated signals).
    #[inline]
    pub fn note(&self, written_offset: u64) {
        if written_offset >= self.soft_bytes {
            self.signal();
        }
    }

    fn signal(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.wake = true;
        self.cv.notify_one();
    }

    fn stop(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.stop = true;
        self.cv.notify_all();
    }
}

/// A background thread that bounds the frame log by checkpointing it at the soft
/// threshold. The storage handle + committed predicate are moved into the thread; the
/// commit path keeps nothing from here. Join + a final checkpoint happen on
/// [`stop_and_join`](Self::stop_and_join) or `Drop`.
pub struct FrameCheckpointer {
    trigger: Arc<CheckpointTrigger>,
    handle: Option<JoinHandle<()>>,
}

impl FrameCheckpointer {
    /// Spawns the checkpointer. `storage` and `is_committed` are moved into the thread;
    /// `soft_bytes` is the background checkpoint threshold (typically
    /// `DbConfig.max_wal_size_mb`); `poll` is the fallback re-check interval.
    ///
    /// Install [`trigger`](Self::trigger) into the storage
    /// ([`MmapStorage::set_checkpoint_trigger`](crate::mmap::MmapStorage::set_checkpoint_trigger))
    /// so the write path can wake the thread immediately on crossing `soft_bytes`.
    pub fn spawn(
        storage: Arc<dyn StorageEngine + Send + Sync>,
        is_committed: Arc<dyn Fn(u64) -> bool + Send + Sync>,
        soft_bytes: u64,
        poll: Duration,
    ) -> Self {
        let trigger = Arc::new(CheckpointTrigger::new(soft_bytes));
        let t = Arc::clone(&trigger);
        let handle = std::thread::Builder::new()
            .name("axiomdb-frame-ckpt".to_string())
            .spawn(move || run(storage, is_committed, soft_bytes, poll, t))
            .expect("spawn frame checkpointer thread");
        Self {
            trigger,
            handle: Some(handle),
        }
    }

    /// The shared wake primitive — install it into the storage so a frame append
    /// crossing the soft threshold wakes this thread immediately (instead of waiting
    /// for the poll interval).
    pub fn trigger(&self) -> Arc<CheckpointTrigger> {
        Arc::clone(&self.trigger)
    }

    /// Signals the thread to drain (a final forced checkpoint) and joins it. Leaves a
    /// current main file + a recycled (bounded) log so a restart begins clean.
    /// Idempotent: a second call after the handle is joined is a no-op.
    pub fn stop_and_join(&mut self) {
        self.trigger.stop();
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for FrameCheckpointer {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

/// The thread body. Sleeps on the trigger; on each wake runs the guarded checkpoint.
fn run(
    storage: Arc<dyn StorageEngine + Send + Sync>,
    is_committed: Arc<dyn Fn(u64) -> bool + Send + Sync>,
    soft_bytes: u64,
    poll: Duration,
    trigger: Arc<CheckpointTrigger>,
) {
    let pred = |t: u64| (*is_committed)(t);
    loop {
        // Wait for a wake (notify), the poll timeout, or stop.
        let stopping = {
            let mut s = trigger.state.lock().unwrap_or_else(|e| e.into_inner());
            while !s.stop && !s.wake {
                let (g, res) = trigger
                    .cv
                    .wait_timeout(s, poll)
                    .unwrap_or_else(|e| e.into_inner());
                s = g;
                if res.timed_out() {
                    break; // periodic re-check
                }
            }
            s.wake = false; // consume the wake edge
            s.stop
        };
        // Guarded: under `soft_bytes` (and not stopping) this is a no-op. On stop,
        // `force` drains the log so shutdown leaves a current main + recycled log.
        if let Err(e) = storage.maybe_checkpoint_frames(&pred, soft_bytes, stopping) {
            tracing::error!(
                target: "axiomdb::checkpointer",
                "frame checkpoint failed: {e}"
            );
        }
        if stopping {
            break;
        }
    }
}
