use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axiomdb_core::error::DbError;
use axiomdb_core::TxnId;

use crate::bitmap::SlotBitmap;
use crate::deadlock::{detect_cycle, select_victim, WaitEdge, WaitForMap};
use crate::entry::{LockEntry, LockQueue, LockWaiter, SyncNotify, WaitAbortReason, WaiterSignal};
use crate::mode::{LockFlags, LockMode};
use crate::txn_locks::LockRef;

/// Result of a lock acquisition attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockResult {
    /// Lock was granted immediately (no conflict).
    Granted,
    /// Lock was granted after waiting for conflicting locks to release.
    WaitGranted,
}

const RECORD_SHARDS: usize = 64;
const TABLE_SHARDS: usize = 16;
const DEFAULT_LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(50); // InnoDB default

/// Sharded row-level lock manager.
///
/// Architecture inspired by InnoDB's `lock_sys_t` (sharded hash table with
/// per-page record bitmaps) and PostgreSQL's partitioned `LockMethodLockHash`
/// (16 partitions with per-partition LWLock).
///
/// - 64 shards for record locks (keyed by `page_id`)
/// - 16 shards for table locks (keyed by `table_id`)
/// - Each shard: `Mutex<HashMap<key, LockQueue>>`
/// - `std::sync::Mutex` (not tokio): held for ~1-10µs, never across `.await`
///
/// Thread-safe (`Send + Sync`). Multiple connections can acquire/release
/// locks on different shards concurrently with zero contention.
pub struct LockManager {
    record_shards: Box<[Mutex<HashMap<u64, LockQueue>>]>,
    table_shards: Box<[Mutex<HashMap<u32, LockQueue>>]>,
    /// Wait-for graph for deadlock detection (InnoDB `wait_mutex` pattern).
    /// Separate from shard locks to avoid lock ordering issues.
    wait_for: Mutex<WaitForMap>,
    lock_wait_timeout: Duration,
}

// SAFETY: All fields are Send+Sync (Mutex<HashMap<..>>, Duration).
unsafe impl Send for LockManager {}
unsafe impl Sync for LockManager {}

impl LockManager {
    pub fn new() -> Self {
        Self::with_timeout(DEFAULT_LOCK_WAIT_TIMEOUT)
    }

    pub fn with_timeout(timeout: Duration) -> Self {
        let record_shards: Vec<_> = (0..RECORD_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();
        let table_shards: Vec<_> = (0..TABLE_SHARDS)
            .map(|_| Mutex::new(HashMap::new()))
            .collect();
        Self {
            record_shards: record_shards.into_boxed_slice(),
            table_shards: table_shards.into_boxed_slice(),
            wait_for: Mutex::new(HashMap::new()),
            lock_wait_timeout: timeout,
        }
    }

    /// Acquires a record lock on a specific slot (heap_no) within a page.
    ///
    /// If the lock cannot be granted immediately (conflict with another
    /// transaction), the caller awaits a `tokio::sync::Notify` until the
    /// holder releases or the timeout expires.
    ///
    /// InnoDB bitmap optimization: if the same transaction already holds a
    /// lock with the same mode on the same page, the slot is added to the
    /// existing entry's bitmap without creating a new `LockEntry`.
    pub async fn acquire_record_lock(
        &self,
        txn_id: TxnId,
        page_id: u64,
        slot_id: u16,
        mode: LockMode,
        flags: LockFlags,
    ) -> Result<LockResult, DbError> {
        let shard_idx = page_id as usize % RECORD_SHARDS;
        let (notify, blocker) = {
            let mut shard = self.record_shards[shard_idx].lock().unwrap();
            let queue = shard.entry(page_id).or_default();

            // Fast path: same txn already holds compatible lock on this page.
            // Extend the bitmap instead of creating a new entry.
            if let Some(idx) = queue.find_granted_idx(txn_id, mode) {
                if let Some(ref mut bm) = queue.granted[idx].bitmap {
                    bm.set(slot_id);
                }
                return Ok(LockResult::Granted);
            }

            // Check for upgrade: txn holds S, requests X.
            if mode == LockMode::Exclusive {
                if let Some(idx) = queue.find_granted_idx(txn_id, LockMode::Shared) {
                    // Check if any OTHER txn conflicts with X on this slot.
                    let has_other_conflict =
                        queue.find_conflict(txn_id, mode, Some(slot_id)).is_some();
                    if !has_other_conflict && queue.waiting.is_empty() {
                        // Upgrade in place: S → X.
                        queue.granted[idx].mode = LockMode::Exclusive;
                        if let Some(ref mut bm) = queue.granted[idx].bitmap {
                            bm.set(slot_id);
                        }
                        return Ok(LockResult::Granted);
                    }
                    // Fall through to wait — upgrade blocked by other holders.
                }
            }

            // Check conflict with granted locks.
            let blocking_txn = queue.find_conflict(txn_id, mode, Some(slot_id));

            // FIFO discipline: if there are waiters, we must also wait even if
            // our request is compatible with granted locks. This prevents
            // starvation of earlier waiters (PostgreSQL ProcSleep pattern).
            if blocking_txn.is_none() && queue.waiting.is_empty() {
                // Granted immediately.
                let mut entry = LockEntry {
                    txn_id,
                    mode,
                    flags,
                    requested_at: Instant::now(),
                    bitmap: Some(SlotBitmap::with_slot(slot_id)),
                };
                entry.flags = entry.flags.difference(LockFlags::WAITING);
                queue.granted.push(entry);
                return Ok(LockResult::Granted);
            }

            // Must wait. Create a Notify and enqueue.
            let notify = std::sync::Arc::new(tokio::sync::Notify::new());
            let waiter = LockWaiter {
                entry: LockEntry {
                    txn_id,
                    mode,
                    flags: flags.union(LockFlags::WAITING),
                    requested_at: Instant::now(),
                    bitmap: Some(SlotBitmap::with_slot(slot_id)),
                },
                signal: WaiterSignal::Async(std::sync::Arc::clone(&notify)),
                abort_reason: None,
            };
            queue.waiting.push_back(waiter);

            // Determine who blocks us: either a granted holder or the first
            // waiter ahead of us in the FIFO queue.
            let blocker = blocking_txn
                .unwrap_or_else(|| queue.waiting.front().map(|w| w.entry.txn_id).unwrap_or(0));

            (notify, blocker)
            // Shard mutex dropped here.
        };

        // ── Deadlock detection (InnoDB synchronous pattern) ──────────────
        let lock_ref = LockRef::Record { page_id, slot_id };
        {
            let mut wf = self.wait_for.lock().unwrap();
            wf.insert(
                txn_id,
                WaitEdge {
                    blocking_txn: blocker,
                    lock_ref: lock_ref.clone(),
                    wait_started: Instant::now(),
                    undo_count: 0, // caller can set via set_undo_count() if needed
                },
            );
            if let Some(cycle) = detect_cycle(txn_id, &wf) {
                let victim = select_victim(&cycle, &wf);
                let victim_ref = wf.get(&victim).map(|e| e.lock_ref.clone());
                wf.remove(&victim);
                drop(wf);
                if let Some(vr) = victim_ref {
                    self.abort_waiter(victim, &vr);
                }
                if victim == txn_id {
                    // We are the victim — remove from wait queue and return error.
                    let mut shard = self.record_shards[shard_idx].lock().unwrap();
                    if let Some(queue) = shard.get_mut(&page_id) {
                        queue.remove_waiter_by_txn(txn_id);
                        if queue.is_empty() {
                            shard.remove(&page_id);
                        }
                    }
                    return Err(DbError::DeadlockDetected);
                }
            }
        }

        // Wait outside the mutex.
        let result = match tokio::time::timeout(self.lock_wait_timeout, notify.notified()).await {
            Ok(()) => {
                // Woken up — check if we were granted or aborted.
                let shard = self.record_shards[shard_idx].lock().unwrap();
                if let Some(queue) = shard.get(&page_id) {
                    // Check if we're now in granted list.
                    if queue
                        .granted
                        .iter()
                        .any(|e| e.txn_id == txn_id && e.mode == mode)
                    {
                        Ok(LockResult::WaitGranted)
                    } else if queue.waiting.iter().any(|w| {
                        w.entry.txn_id == txn_id
                            && w.abort_reason == Some(WaitAbortReason::Deadlock)
                    }) {
                        Err(DbError::DeadlockDetected)
                    } else {
                        // Entry moved to granted and queue cleaned up.
                        Ok(LockResult::WaitGranted)
                    }
                } else {
                    Ok(LockResult::WaitGranted)
                }
            }
            Err(_) => {
                // Timeout — remove ourselves from the waiting queue.
                let mut shard = self.record_shards[shard_idx].lock().unwrap();
                if let Some(queue) = shard.get_mut(&page_id) {
                    queue.remove_waiter_by_txn(txn_id);
                    if queue.is_empty() {
                        shard.remove(&page_id);
                    }
                }
                Err(DbError::LockTimeout)
            }
        };

        // Clean up wait-for entry on all exit paths.
        self.wait_for.lock().unwrap().remove(&txn_id);
        result
    }

    /// Acquires a table-level lock (IS, IX, S, X, or AutoIncrement).
    ///
    /// Table locks are acquired BEFORE row locks:
    /// - Before S row lock → acquire IS on table
    /// - Before X row lock → acquire IX on table
    /// - DDL (DROP/ALTER) → acquire X on table
    pub async fn acquire_table_lock(
        &self,
        txn_id: TxnId,
        table_id: u32,
        mode: LockMode,
    ) -> Result<LockResult, DbError> {
        let shard_idx = table_id as usize % TABLE_SHARDS;
        let notify = {
            let mut shard = self.table_shards[shard_idx].lock().unwrap();
            let queue = shard.entry(table_id).or_default();

            // Already holds same or stronger lock?
            if queue
                .granted
                .iter()
                .any(|e| e.txn_id == txn_id && e.mode == mode)
            {
                return Ok(LockResult::Granted);
            }

            let has_conflict = queue.find_conflict(txn_id, mode, None).is_some();

            if !has_conflict && queue.waiting.is_empty() {
                queue.granted.push(LockEntry {
                    txn_id,
                    mode,
                    flags: LockFlags::TABLE_LOCK,
                    requested_at: Instant::now(),
                    bitmap: None,
                });
                return Ok(LockResult::Granted);
            }

            let notify = std::sync::Arc::new(tokio::sync::Notify::new());
            queue.waiting.push_back(LockWaiter {
                entry: LockEntry {
                    txn_id,
                    mode,
                    flags: LockFlags::TABLE_LOCK.union(LockFlags::WAITING),
                    requested_at: Instant::now(),
                    bitmap: None,
                },
                signal: WaiterSignal::Async(std::sync::Arc::clone(&notify)),
                abort_reason: None,
            });
            notify
        };

        match tokio::time::timeout(self.lock_wait_timeout, notify.notified()).await {
            Ok(()) => Ok(LockResult::WaitGranted),
            Err(_) => {
                let mut shard = self.table_shards[shard_idx].lock().unwrap();
                if let Some(queue) = shard.get_mut(&table_id) {
                    queue.remove_waiter_by_txn(txn_id);
                    if queue.is_empty() {
                        shard.remove(&table_id);
                    }
                }
                Err(DbError::LockTimeout)
            }
        }
    }

    /// Releases all locks held by a transaction (called on COMMIT/ROLLBACK).
    ///
    /// For each `LockRef`, acquires the appropriate shard mutex, removes the
    /// entry, promotes compatible waiters, then drops the mutex BEFORE
    /// notifying waiters (InnoDB pattern: never hold lock_sys latch during
    /// signal).
    ///
    /// Returns the number of waiters that were granted.
    pub fn release_all_locks(&self, txn_id: TxnId, held_locks: &[LockRef]) -> usize {
        let mut total_granted = 0;
        let mut all_signals: Vec<WaiterSignal> = Vec::new();

        for lock_ref in held_locks {
            match lock_ref {
                LockRef::Record { page_id, .. } => {
                    let shard_idx = *page_id as usize % RECORD_SHARDS;
                    let mut shard = self.record_shards[shard_idx].lock().unwrap();
                    if let Some(queue) = shard.get_mut(page_id) {
                        queue.remove_granted_by_txn(txn_id);
                        let signals = queue.try_grant_waiters();
                        total_granted += signals.len();
                        all_signals.extend(signals);
                        if queue.is_empty() {
                            shard.remove(page_id);
                        }
                    }
                }
                LockRef::Table { table_id } => {
                    let shard_idx = *table_id as usize % TABLE_SHARDS;
                    let mut shard = self.table_shards[shard_idx].lock().unwrap();
                    if let Some(queue) = shard.get_mut(table_id) {
                        queue.remove_granted_by_txn(txn_id);
                        let signals = queue.try_grant_waiters();
                        total_granted += signals.len();
                        all_signals.extend(signals);
                        if queue.is_empty() {
                            shard.remove(table_id);
                        }
                    }
                }
            }
        }

        // Notify all waiters AFTER dropping all shard mutexes.
        for signal in all_signals {
            signal.notify_one();
        }

        total_granted
    }

    /// Hook for the deadlock detector (40.6).
    ///
    /// Sets `abort_reason = Deadlock` on the victim's `LockWaiter` and
    /// notifies it so it wakes up and returns `DbError::DeadlockDetected`.
    pub fn abort_waiter(&self, txn_id: TxnId, lock_ref: &LockRef) {
        match lock_ref {
            LockRef::Record { page_id, .. } => {
                let shard_idx = *page_id as usize % RECORD_SHARDS;
                let mut shard = self.record_shards[shard_idx].lock().unwrap();
                if let Some(queue) = shard.get_mut(page_id) {
                    if let Some(waiter) =
                        queue.waiting.iter_mut().find(|w| w.entry.txn_id == txn_id)
                    {
                        waiter.abort_reason = Some(WaitAbortReason::Deadlock);
                        waiter.signal.notify_one();
                    }
                }
            }
            LockRef::Table { table_id } => {
                let shard_idx = *table_id as usize % TABLE_SHARDS;
                let mut shard = self.table_shards[shard_idx].lock().unwrap();
                if let Some(queue) = shard.get_mut(table_id) {
                    if let Some(waiter) =
                        queue.waiting.iter_mut().find(|w| w.entry.txn_id == txn_id)
                    {
                        waiter.abort_reason = Some(WaitAbortReason::Deadlock);
                        waiter.signal.notify_one();
                    }
                }
            }
        }
    }

    /// Releases ALL locks (record + table) held by a transaction by scanning
    /// every shard. Called on COMMIT/ROLLBACK when per-txn lock tracking is not
    /// yet wired (Phase 40.11 interim). Promotes compatible waiters.
    ///
    /// Equivalent to InnoDB's `lock_trx_release_locks()` but uses brute-force
    /// shard scan instead of per-txn linked list. Acceptable because:
    /// - 64 + 16 = 80 shards, each scan is O(entries_in_shard)
    /// - Called once per transaction end, not per-statement
    /// - Phase 40.12 can add per-txn tracking for O(held_locks) release
    pub fn release_all_for_txn(&self, txn_id: TxnId) -> usize {
        let mut total_granted = 0;
        let mut all_signals: Vec<WaiterSignal> = Vec::new();

        // Record shards.
        for shard_mutex in self.record_shards.iter() {
            let mut shard = shard_mutex.lock().unwrap();
            let mut empty_pages = Vec::new();
            for (&page_id, queue) in shard.iter_mut() {
                let removed = queue.remove_granted_by_txn(txn_id);
                if !removed.is_empty() {
                    let signals = queue.try_grant_waiters();
                    total_granted += signals.len();
                    all_signals.extend(signals);
                }
                if queue.is_empty() {
                    empty_pages.push(page_id);
                }
            }
            for pid in empty_pages {
                shard.remove(&pid);
            }
        }

        // Table shards.
        for shard_mutex in self.table_shards.iter() {
            let mut shard = shard_mutex.lock().unwrap();
            let mut empty_tables = Vec::new();
            for (&table_id, queue) in shard.iter_mut() {
                let removed = queue.remove_granted_by_txn(txn_id);
                if !removed.is_empty() {
                    let signals = queue.try_grant_waiters();
                    total_granted += signals.len();
                    all_signals.extend(signals);
                }
                if queue.is_empty() {
                    empty_tables.push(table_id);
                }
            }
            for tid in empty_tables {
                shard.remove(&tid);
            }
        }

        // Notify all after dropping all shard mutexes.
        for signal in all_signals {
            signal.notify_one();
        }

        // Clean up any stale wait-for entry.
        self.wait_for.lock().unwrap().remove(&txn_id);

        total_granted
    }

    // ── Synchronous API (Phase 40.11) ──────────────────────────────────────

    /// Synchronous variant of [`acquire_record_lock`] for the sync executor.
    ///
    /// Fast path (no conflict): immediate grant — zero async overhead.
    /// Slow path (conflict): `Condvar::wait_timeout` — blocks the OS thread.
    ///
    /// The grant/conflict logic is identical to the async path; only the wait
    /// mechanism differs (`Condvar` vs `tokio::sync::Notify`).
    pub fn acquire_record_lock_sync(
        &self,
        txn_id: TxnId,
        page_id: u64,
        slot_id: u16,
        mode: LockMode,
        flags: LockFlags,
    ) -> Result<LockResult, DbError> {
        let shard_idx = page_id as usize % RECORD_SHARDS;
        let (sync_notify, blocker) = {
            let mut shard = self.record_shards[shard_idx].lock().unwrap();
            let queue = shard.entry(page_id).or_default();

            // Fast path: same txn already holds compatible lock on this page.
            if let Some(idx) = queue.find_granted_idx(txn_id, mode) {
                if let Some(ref mut bm) = queue.granted[idx].bitmap {
                    bm.set(slot_id);
                }
                return Ok(LockResult::Granted);
            }

            // Check for upgrade: txn holds S, requests X.
            if mode == LockMode::Exclusive {
                if let Some(idx) = queue.find_granted_idx(txn_id, LockMode::Shared) {
                    let has_other_conflict =
                        queue.find_conflict(txn_id, mode, Some(slot_id)).is_some();
                    if !has_other_conflict && queue.waiting.is_empty() {
                        queue.granted[idx].mode = LockMode::Exclusive;
                        if let Some(ref mut bm) = queue.granted[idx].bitmap {
                            bm.set(slot_id);
                        }
                        return Ok(LockResult::Granted);
                    }
                }
            }

            // Check conflict with granted locks.
            let blocking_txn = queue.find_conflict(txn_id, mode, Some(slot_id));

            // FIFO discipline.
            if blocking_txn.is_none() && queue.waiting.is_empty() {
                let mut entry = LockEntry {
                    txn_id,
                    mode,
                    flags,
                    requested_at: Instant::now(),
                    bitmap: Some(SlotBitmap::with_slot(slot_id)),
                };
                entry.flags = entry.flags.difference(LockFlags::WAITING);
                queue.granted.push(entry);
                return Ok(LockResult::Granted);
            }

            // Must wait — enqueue with SyncNotify.
            let sync_notify = std::sync::Arc::new(SyncNotify::new());
            let waiter = LockWaiter {
                entry: LockEntry {
                    txn_id,
                    mode,
                    flags: flags.union(LockFlags::WAITING),
                    requested_at: Instant::now(),
                    bitmap: Some(SlotBitmap::with_slot(slot_id)),
                },
                signal: WaiterSignal::Sync(std::sync::Arc::clone(&sync_notify)),
                abort_reason: None,
            };
            queue.waiting.push_back(waiter);

            let blocker = blocking_txn
                .unwrap_or_else(|| queue.waiting.front().map(|w| w.entry.txn_id).unwrap_or(0));
            (sync_notify, blocker)
            // Shard mutex dropped here.
        };

        // NOWAIT: remove ourselves from the queue and fail immediately.
        if flags.contains(LockFlags::NOWAIT) {
            let mut shard = self.record_shards[shard_idx].lock().unwrap();
            if let Some(queue) = shard.get_mut(&page_id) {
                queue.remove_waiter_by_txn(txn_id);
                if queue.is_empty() {
                    shard.remove(&page_id);
                }
            }
            return Err(DbError::LockTimeout);
        }

        // ── Deadlock detection ───────────────────────────────────────────
        let lock_ref = LockRef::Record { page_id, slot_id };
        {
            let mut wf = self.wait_for.lock().unwrap();
            wf.insert(
                txn_id,
                WaitEdge {
                    blocking_txn: blocker,
                    lock_ref: lock_ref.clone(),
                    wait_started: Instant::now(),
                    undo_count: 0,
                },
            );
            if let Some(cycle) = detect_cycle(txn_id, &wf) {
                let victim = select_victim(&cycle, &wf);
                let victim_ref = wf.get(&victim).map(|e| e.lock_ref.clone());
                wf.remove(&victim);
                drop(wf);
                if let Some(vr) = victim_ref {
                    self.abort_waiter(victim, &vr);
                }
                if victim == txn_id {
                    let mut shard = self.record_shards[shard_idx].lock().unwrap();
                    if let Some(queue) = shard.get_mut(&page_id) {
                        queue.remove_waiter_by_txn(txn_id);
                        if queue.is_empty() {
                            shard.remove(&page_id);
                        }
                    }
                    return Err(DbError::DeadlockDetected);
                }
            }
        }

        // Wait via Condvar (blocks the OS thread).
        let result = if sync_notify.wait_timeout(self.lock_wait_timeout) {
            // Woken up — check if granted or aborted.
            let shard = self.record_shards[shard_idx].lock().unwrap();
            if let Some(queue) = shard.get(&page_id) {
                if queue
                    .granted
                    .iter()
                    .any(|e| e.txn_id == txn_id && e.mode == mode)
                {
                    Ok(LockResult::WaitGranted)
                } else if queue.waiting.iter().any(|w| {
                    w.entry.txn_id == txn_id && w.abort_reason == Some(WaitAbortReason::Deadlock)
                }) {
                    Err(DbError::DeadlockDetected)
                } else {
                    Ok(LockResult::WaitGranted)
                }
            } else {
                Ok(LockResult::WaitGranted)
            }
        } else {
            // Timeout.
            let mut shard = self.record_shards[shard_idx].lock().unwrap();
            if let Some(queue) = shard.get_mut(&page_id) {
                queue.remove_waiter_by_txn(txn_id);
                if queue.is_empty() {
                    shard.remove(&page_id);
                }
            }
            Err(DbError::LockTimeout)
        };

        self.wait_for.lock().unwrap().remove(&txn_id);
        result
    }

    /// Synchronous variant of [`acquire_table_lock`].
    /// Non-blocking try-acquire for a record lock (SKIP LOCKED path).
    ///
    /// Returns `Ok(true)` if the lock was granted immediately.
    /// Returns `Ok(false)` if a conflicting lock is held by another transaction
    /// or if there are waiters ahead in the FIFO queue — the row should be skipped.
    /// Never enqueues a waiter, never blocks, never runs deadlock detection.
    pub fn try_acquire_record_lock_sync(
        &self,
        txn_id: TxnId,
        page_id: u64,
        slot_id: u16,
        mode: LockMode,
    ) -> Result<bool, DbError> {
        let shard_idx = page_id as usize % RECORD_SHARDS;
        let mut shard = self.record_shards[shard_idx].lock().unwrap();
        let queue = shard.entry(page_id).or_default();

        // Fast path: same txn already holds a compatible lock on this page.
        if let Some(idx) = queue.find_granted_idx(txn_id, mode) {
            if let Some(ref mut bm) = queue.granted[idx].bitmap {
                bm.set(slot_id);
            }
            return Ok(true);
        }

        // Check conflict with granted locks for this specific slot.
        let blocking_txn = queue.find_conflict(txn_id, mode, Some(slot_id));

        // Skip if conflicting lock held or FIFO waiters are ahead.
        if blocking_txn.is_some() || !queue.waiting.is_empty() {
            if queue.is_empty() {
                shard.remove(&page_id);
            }
            return Ok(false);
        }

        // Grant immediately.
        queue.granted.push(LockEntry {
            txn_id,
            mode,
            flags: LockFlags::NONE,
            requested_at: Instant::now(),
            bitmap: Some(SlotBitmap::with_slot(slot_id)),
        });
        Ok(true)
    }

    pub fn acquire_table_lock_sync(
        &self,
        txn_id: TxnId,
        table_id: u32,
        mode: LockMode,
    ) -> Result<LockResult, DbError> {
        let shard_idx = table_id as usize % TABLE_SHARDS;
        let sync_notify = {
            let mut shard = self.table_shards[shard_idx].lock().unwrap();
            let queue = shard.entry(table_id).or_default();

            if queue
                .granted
                .iter()
                .any(|e| e.txn_id == txn_id && e.mode == mode)
            {
                return Ok(LockResult::Granted);
            }

            let has_conflict = queue.find_conflict(txn_id, mode, None).is_some();

            if !has_conflict && queue.waiting.is_empty() {
                queue.granted.push(LockEntry {
                    txn_id,
                    mode,
                    flags: LockFlags::TABLE_LOCK,
                    requested_at: Instant::now(),
                    bitmap: None,
                });
                return Ok(LockResult::Granted);
            }

            let sync_notify = std::sync::Arc::new(SyncNotify::new());
            queue.waiting.push_back(LockWaiter {
                entry: LockEntry {
                    txn_id,
                    mode,
                    flags: LockFlags::TABLE_LOCK.union(LockFlags::WAITING),
                    requested_at: Instant::now(),
                    bitmap: None,
                },
                signal: WaiterSignal::Sync(std::sync::Arc::clone(&sync_notify)),
                abort_reason: None,
            });
            sync_notify
        };

        if sync_notify.wait_timeout(self.lock_wait_timeout) {
            Ok(LockResult::WaitGranted)
        } else {
            let mut shard = self.table_shards[shard_idx].lock().unwrap();
            if let Some(queue) = shard.get_mut(&table_id) {
                queue.remove_waiter_by_txn(txn_id);
                if queue.is_empty() {
                    shard.remove(&table_id);
                }
            }
            Err(DbError::LockTimeout)
        }
    }
}

impl Default for LockManager {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn two_txns_different_rows_both_granted() {
        let rt = rt();
        rt.block_on(async {
            let lm = LockManager::new();
            let r1 = lm
                .acquire_record_lock(1, 100, 0, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();
            let r2 = lm
                .acquire_record_lock(2, 100, 1, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();
            assert_eq!(r1, LockResult::Granted);
            assert_eq!(r2, LockResult::Granted);
        });
    }

    #[test]
    fn s_s_same_row_both_granted() {
        let rt = rt();
        rt.block_on(async {
            let lm = LockManager::new();
            let r1 = lm
                .acquire_record_lock(1, 100, 5, LockMode::Shared, LockFlags::NONE)
                .await
                .unwrap();
            let r2 = lm
                .acquire_record_lock(2, 100, 5, LockMode::Shared, LockFlags::NONE)
                .await
                .unwrap();
            assert_eq!(r1, LockResult::Granted);
            assert_eq!(r2, LockResult::Granted);
        });
    }

    #[test]
    fn s_x_same_row_x_waits_then_granted() {
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::new());

            // Txn 1 acquires S.
            lm.acquire_record_lock(1, 100, 5, LockMode::Shared, LockFlags::NONE)
                .await
                .unwrap();

            let lm2 = std::sync::Arc::clone(&lm);
            // Txn 2 tries X — should wait.
            let handle = tokio::spawn(async move {
                lm2.acquire_record_lock(2, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                    .await
            });

            // Give txn 2 time to enqueue.
            tokio::time::sleep(Duration::from_millis(50)).await;

            // Release txn 1's locks.
            lm.release_all_locks(
                1,
                &[LockRef::Record {
                    page_id: 100,
                    slot_id: 5,
                }],
            );

            let result = handle.await.unwrap().unwrap();
            assert_eq!(result, LockResult::WaitGranted);
        });
    }

    #[test]
    fn x_x_same_row_second_waits() {
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::new());
            lm.acquire_record_lock(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();

            let lm2 = std::sync::Arc::clone(&lm);
            let handle = tokio::spawn(async move {
                lm2.acquire_record_lock(2, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                    .await
            });

            tokio::time::sleep(Duration::from_millis(50)).await;
            lm.release_all_locks(
                1,
                &[LockRef::Record {
                    page_id: 100,
                    slot_id: 5,
                }],
            );

            let result = handle.await.unwrap().unwrap();
            assert_eq!(result, LockResult::WaitGranted);
        });
    }

    #[test]
    fn table_ix_ix_compatible() {
        let rt = rt();
        rt.block_on(async {
            let lm = LockManager::new();
            let r1 = lm
                .acquire_table_lock(1, 10, LockMode::IntentionExclusive)
                .await
                .unwrap();
            let r2 = lm
                .acquire_table_lock(2, 10, LockMode::IntentionExclusive)
                .await
                .unwrap();
            assert_eq!(r1, LockResult::Granted);
            assert_eq!(r2, LockResult::Granted);
        });
    }

    #[test]
    fn table_ix_s_conflict() {
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::new());
            lm.acquire_table_lock(1, 10, LockMode::IntentionExclusive)
                .await
                .unwrap();

            let lm2 = std::sync::Arc::clone(&lm);
            let handle =
                tokio::spawn(async move { lm2.acquire_table_lock(2, 10, LockMode::Shared).await });

            tokio::time::sleep(Duration::from_millis(50)).await;
            lm.release_all_locks(1, &[LockRef::Table { table_id: 10 }]);

            let result = handle.await.unwrap().unwrap();
            assert_eq!(result, LockResult::WaitGranted);
        });
    }

    #[test]
    fn lock_upgrade_s_to_x() {
        let rt = rt();
        rt.block_on(async {
            let lm = LockManager::new();
            // Txn 1 acquires S.
            lm.acquire_record_lock(1, 100, 5, LockMode::Shared, LockFlags::NONE)
                .await
                .unwrap();
            // Txn 1 upgrades to X (no other holders).
            let r = lm
                .acquire_record_lock(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();
            assert_eq!(r, LockResult::Granted);
        });
    }

    #[test]
    fn same_txn_same_mode_extends_bitmap() {
        let rt = rt();
        rt.block_on(async {
            let lm = LockManager::new();
            lm.acquire_record_lock(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();
            // Same txn, same page, same mode, different slot → bitmap extension.
            let r = lm
                .acquire_record_lock(1, 100, 10, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();
            assert_eq!(r, LockResult::Granted);

            // Verify bitmap has both slots.
            let shard = lm.record_shards[100 % RECORD_SHARDS].lock().unwrap();
            let queue = shard.get(&100).unwrap();
            assert_eq!(queue.granted.len(), 1); // single entry
            let bm = queue.granted[0].bitmap.as_ref().unwrap();
            assert!(bm.test(5));
            assert!(bm.test(10));
        });
    }

    #[test]
    fn bulk_release_grants_waiters() {
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::new());

            // Txn 1 locks 3 different rows.
            for slot in 0..3u16 {
                lm.acquire_record_lock(1, 200, slot, LockMode::Exclusive, LockFlags::NONE)
                    .await
                    .unwrap();
            }

            // 3 waiters, each on a different slot.
            let mut handles = Vec::new();
            for slot in 0..3u16 {
                let lm2 = std::sync::Arc::clone(&lm);
                let txn_id = 10 + slot as u64;
                handles.push(tokio::spawn(async move {
                    lm2.acquire_record_lock(txn_id, 200, slot, LockMode::Exclusive, LockFlags::NONE)
                        .await
                }));
            }

            tokio::time::sleep(Duration::from_millis(50)).await;

            // Bulk release.
            let refs: Vec<_> = (0..3u16)
                .map(|s| LockRef::Record {
                    page_id: 200,
                    slot_id: s,
                })
                .collect();
            let granted = lm.release_all_locks(1, &refs);
            assert_eq!(granted, 3);

            for h in handles {
                assert_eq!(h.await.unwrap().unwrap(), LockResult::WaitGranted);
            }
        });
    }

    #[test]
    fn lock_wait_timeout() {
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::with_timeout(Duration::from_millis(100)));
            lm.acquire_record_lock(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();

            let lm2 = std::sync::Arc::clone(&lm);
            let result = lm2
                .acquire_record_lock(2, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                .await;
            assert!(result.is_err());
            match result.unwrap_err() {
                DbError::LockTimeout => {}
                other => panic!("expected LockTimeout, got {:?}", other),
            }
        });
    }

    #[test]
    fn fifo_ordering() {
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::new());
            lm.acquire_record_lock(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();

            // 3 waiters enqueue in order.
            let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let mut handles = Vec::new();
            for txn in 2..=4u64 {
                let lm2 = std::sync::Arc::clone(&lm);
                let order2 = std::sync::Arc::clone(&order);
                handles.push(tokio::spawn(async move {
                    lm2.acquire_record_lock(txn, 100, 5, LockMode::Exclusive, LockFlags::NONE)
                        .await
                        .unwrap();
                    order2.lock().unwrap().push(txn);
                    // Release immediately so next waiter can proceed.
                    lm2.release_all_locks(
                        txn,
                        &[LockRef::Record {
                            page_id: 100,
                            slot_id: 5,
                        }],
                    );
                }));
                tokio::time::sleep(Duration::from_millis(20)).await;
            }

            // Release holder — triggers FIFO grant chain.
            lm.release_all_locks(
                1,
                &[LockRef::Record {
                    page_id: 100,
                    slot_id: 5,
                }],
            );

            for h in handles {
                h.await.unwrap();
            }

            // X locks are exclusive — only one at a time.
            // FIFO order: 2, 3, 4.
            let grant_order = order.lock().unwrap().clone();
            assert_eq!(grant_order, vec![2, 3, 4]);
        });
    }

    #[test]
    fn stress_8_tasks_1000_locks_disjoint() {
        // Each task locks a non-overlapping range of pages → zero contention.
        // Validates shard concurrency and memory cleanup under load.
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::new());
            let mut handles = Vec::new();

            for task in 0..8u64 {
                let lm2 = std::sync::Arc::clone(&lm);
                handles.push(tokio::spawn(async move {
                    let txn_id = 100 + task;
                    let base_page = task * 1000; // non-overlapping
                    let mut refs = Vec::new();
                    for i in 0..1000u16 {
                        let page_id = base_page + i as u64;
                        let slot_id = i % 64;
                        lm2.acquire_record_lock(
                            txn_id,
                            page_id,
                            slot_id,
                            LockMode::Exclusive,
                            LockFlags::NONE,
                        )
                        .await
                        .unwrap();
                        refs.push(LockRef::Record { page_id, slot_id });
                    }
                    lm2.release_all_locks(txn_id, &refs);
                }));
            }

            for h in handles {
                h.await.unwrap();
            }
        });
    }

    #[test]
    fn stress_contended_acquire_release_cycles() {
        // 4 tasks contend on the SAME row with short hold times.
        // Each acquires X, does "work" (brief sleep), releases, repeats.
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::with_timeout(Duration::from_secs(5)));
            let mut handles = Vec::new();

            for task in 0..4u64 {
                let lm2 = std::sync::Arc::clone(&lm);
                handles.push(tokio::spawn(async move {
                    let txn_id = 200 + task;
                    for _ in 0..50 {
                        // Retry on deadlock — transient false positives are
                        // possible when concurrent tasks release/acquire rapidly.
                        loop {
                            match lm2
                                .acquire_record_lock(
                                    txn_id,
                                    999,
                                    0,
                                    LockMode::Exclusive,
                                    LockFlags::NONE,
                                )
                                .await
                            {
                                Ok(_) => break,
                                Err(DbError::DeadlockDetected) => {
                                    tokio::task::yield_now().await;
                                    continue;
                                }
                                Err(e) => panic!("unexpected error: {:?}", e),
                            }
                        }
                        tokio::task::yield_now().await;
                        lm2.release_all_locks(
                            txn_id,
                            &[LockRef::Record {
                                page_id: 999,
                                slot_id: 0,
                            }],
                        );
                    }
                }));
            }

            for h in handles {
                h.await.unwrap();
            }
        });
    }

    // ── Sync API tests (Phase 40.11) ─────────────────────────────────────

    #[test]
    fn sync_record_lock_immediate_grant() {
        let lm = LockManager::new();
        let r = lm
            .acquire_record_lock_sync(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        assert_eq!(r, LockResult::Granted);
    }

    #[test]
    fn sync_two_txns_different_slots_both_granted() {
        let lm = LockManager::new();
        let r1 = lm
            .acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        let r2 = lm
            .acquire_record_lock_sync(2, 100, 1, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        assert_eq!(r1, LockResult::Granted);
        assert_eq!(r2, LockResult::Granted);
    }

    #[test]
    fn sync_same_txn_extends_bitmap() {
        let lm = LockManager::new();
        lm.acquire_record_lock_sync(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        let r = lm
            .acquire_record_lock_sync(1, 100, 10, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        assert_eq!(r, LockResult::Granted);

        let shard = lm.record_shards[100 % RECORD_SHARDS].lock().unwrap();
        let queue = shard.get(&100).unwrap();
        assert_eq!(queue.granted.len(), 1);
        let bm = queue.granted[0].bitmap.as_ref().unwrap();
        assert!(bm.test(5));
        assert!(bm.test(10));
    }

    #[test]
    fn sync_x_x_same_row_wait_then_granted() {
        let lm = std::sync::Arc::new(LockManager::new());

        // Txn 1 acquires X on slot 5.
        lm.acquire_record_lock_sync(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();

        // Txn 2 waits for X on slot 5 from another thread.
        let lm2 = std::sync::Arc::clone(&lm);
        let handle = std::thread::spawn(move || {
            lm2.acquire_record_lock_sync(2, 100, 5, LockMode::Exclusive, LockFlags::NONE)
        });

        // Give txn 2 time to enqueue.
        std::thread::sleep(Duration::from_millis(50));

        // Release txn 1.
        lm.release_all_locks(
            1,
            &[LockRef::Record {
                page_id: 100,
                slot_id: 5,
            }],
        );

        let result = handle.join().unwrap().unwrap();
        assert_eq!(result, LockResult::WaitGranted);
    }

    #[test]
    fn sync_lock_timeout() {
        let lm = std::sync::Arc::new(LockManager::with_timeout(Duration::from_millis(100)));
        lm.acquire_record_lock_sync(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();

        // Txn 2 should timeout.
        let result = lm.acquire_record_lock_sync(2, 100, 5, LockMode::Exclusive, LockFlags::NONE);
        assert!(result.is_err());
        match result.unwrap_err() {
            DbError::LockTimeout => {}
            other => panic!("expected LockTimeout, got {:?}", other),
        }
    }

    #[test]
    fn sync_table_lock_ix_ix_compatible() {
        let lm = LockManager::new();
        let r1 = lm
            .acquire_table_lock_sync(1, 10, LockMode::IntentionExclusive)
            .unwrap();
        let r2 = lm
            .acquire_table_lock_sync(2, 10, LockMode::IntentionExclusive)
            .unwrap();
        assert_eq!(r1, LockResult::Granted);
        assert_eq!(r2, LockResult::Granted);
    }

    #[test]
    fn sync_table_lock_ix_s_wait_then_granted() {
        let lm = std::sync::Arc::new(LockManager::new());
        lm.acquire_table_lock_sync(1, 10, LockMode::IntentionExclusive)
            .unwrap();

        let lm2 = std::sync::Arc::clone(&lm);
        let handle =
            std::thread::spawn(move || lm2.acquire_table_lock_sync(2, 10, LockMode::Shared));

        std::thread::sleep(Duration::from_millis(50));
        lm.release_all_locks(1, &[LockRef::Table { table_id: 10 }]);

        let result = handle.join().unwrap().unwrap();
        assert_eq!(result, LockResult::WaitGranted);
    }

    #[test]
    fn sync_mixed_async_sync_waiters() {
        // Async waiter and sync waiter on the same row — both should be
        // granted after the holder releases.
        let rt = rt();
        rt.block_on(async {
            let lm = std::sync::Arc::new(LockManager::new());

            // Txn 1 holds X.
            lm.acquire_record_lock(1, 300, 0, LockMode::Exclusive, LockFlags::NONE)
                .await
                .unwrap();

            // Txn 2: async waiter for S.
            let lm2 = std::sync::Arc::clone(&lm);
            let async_handle = tokio::spawn(async move {
                lm2.acquire_record_lock(2, 300, 0, LockMode::Shared, LockFlags::NONE)
                    .await
            });

            // Txn 3: sync waiter for S (from a blocking thread).
            let lm3 = std::sync::Arc::clone(&lm);
            let sync_handle = std::thread::spawn(move || {
                lm3.acquire_record_lock_sync(3, 300, 0, LockMode::Shared, LockFlags::NONE)
            });

            tokio::time::sleep(Duration::from_millis(50)).await;

            // Release holder — both S waiters should be granted.
            lm.release_all_locks(
                1,
                &[LockRef::Record {
                    page_id: 300,
                    slot_id: 0,
                }],
            );

            let async_result = async_handle.await.unwrap().unwrap();
            let sync_result = sync_handle.join().unwrap().unwrap();
            assert!(async_result == LockResult::WaitGranted || async_result == LockResult::Granted);
            assert!(sync_result == LockResult::WaitGranted || sync_result == LockResult::Granted);
        });
    }

    #[test]
    fn sync_nowait_returns_lock_timeout_immediately() {
        let lm = std::sync::Arc::new(LockManager::new());

        // Txn 1 holds Exclusive on slot 5.
        lm.acquire_record_lock_sync(1, 100, 5, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();

        // Txn 2 requests Exclusive + NOWAIT → must fail instantly without sleeping.
        let flags = LockFlags::REC_NOT_GAP.union(LockFlags::NOWAIT);
        let start = std::time::Instant::now();
        let result = lm.acquire_record_lock_sync(2, 100, 5, LockMode::Exclusive, flags);
        assert!(result.is_err(), "expected LockTimeout");
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "NOWAIT must not block; elapsed={:?}",
            start.elapsed()
        );
        match result.unwrap_err() {
            DbError::LockTimeout => {}
            other => panic!("expected LockTimeout, got {:?}", other),
        }

        // Queue must be clean after NOWAIT failure.
        let shard = lm.record_shards[100 % RECORD_SHARDS].lock().unwrap();
        let queue = shard.get(&100).unwrap();
        assert!(queue.waiting.is_empty(), "waiter must be removed on NOWAIT");
    }

    // ── try_acquire_record_lock_sync (SKIP LOCKED) ────────────────────────

    #[test]
    fn try_acquire_granted_when_no_conflict() {
        let lm = LockManager::new();
        assert!(lm.try_acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive).unwrap());
    }

    #[test]
    fn try_acquire_returns_false_when_conflicting_exclusive_held() {
        let lm = LockManager::new();
        lm.acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        // txn 2 wants shared — conflicts with exclusive held by txn 1
        assert!(!lm.try_acquire_record_lock_sync(2, 100, 0, LockMode::Shared).unwrap());
    }

    #[test]
    fn try_acquire_returns_true_for_compatible_shared_locks() {
        let lm = LockManager::new();
        lm.acquire_record_lock_sync(1, 100, 0, LockMode::Shared, LockFlags::NONE)
            .unwrap();
        // txn 2 also wants shared — compatible, no conflict
        assert!(lm.try_acquire_record_lock_sync(2, 100, 0, LockMode::Shared).unwrap());
    }

    #[test]
    fn try_acquire_returns_true_when_same_txn_already_holds() {
        let lm = LockManager::new();
        lm.acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        // same txn tries again — idempotent fast path
        assert!(lm.try_acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive).unwrap());
    }

    #[test]
    fn try_acquire_does_not_enqueue_waiter() {
        let lm = LockManager::new();
        lm.acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive, LockFlags::NONE)
            .unwrap();
        // txn 2 tries — conflict, returns false without enqueueing
        let _ = lm.try_acquire_record_lock_sync(2, 100, 0, LockMode::Exclusive);
        // release txn 1
        lm.release_all_for_txn(1);
        // txn 3 should get the lock cleanly (no phantom waiter from txn 2)
        assert!(lm.try_acquire_record_lock_sync(3, 100, 0, LockMode::Exclusive).unwrap());
    }
}
