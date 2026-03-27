# Plan: 6.19 — WAL Fsync Pipeline

## Files to create/modify

### New file
- `crates/axiomdb-wal/src/fsync_pipeline.rs` — Leader-based fsync coalescing primitive

### Modified files
- `crates/axiomdb-wal/src/txn.rs` — Replace inline `wal.commit()` / deferred path with pipeline call
- `crates/axiomdb-wal/src/lib.rs` — Expose `fsync_pipeline` module
- `crates/axiomdb-wal/Cargo.toml` — Add `tokio` dependency (for oneshot channel)
- `crates/axiomdb-network/src/mysql/database.rs` — Remove `CommitCoordinator` usage; wire pipeline into execute path
- `crates/axiomdb-network/src/mysql/handler.rs` — Simplify `await_commit_rx` to use pipeline's future
- `crates/axiomdb-network/src/mysql/mod.rs` — Remove `commit_coordinator` and `group_commit` modules
- `crates/axiomdb-storage/src/config.rs` — Remove `group_commit_interval_ms` / `group_commit_max_batch` (pipeline is always-on)

### Files to delete
- `crates/axiomdb-network/src/mysql/commit_coordinator.rs` — Superseded by pipeline
- `crates/axiomdb-network/src/mysql/group_commit.rs` — Superseded by pipeline

### Tests
- `crates/axiomdb-wal/tests/integration_fsync_pipeline.rs` — New integration tests
- `crates/axiomdb-wal/tests/integration_group_commit.rs` — Remove or adapt

## Algorithm / Data structure

### `FsyncPipeline` — core primitive

```rust
/// Leader-based fsync coalescing for the WAL commit path.
///
/// Inspired by MariaDB's `group_commit_lock` (log0sync.h).
/// Tracks a monotonically increasing `flushed_lsn` and uses leader
/// election to batch fsyncs across concurrent (or pipelined) commits.
pub struct FsyncPipeline {
    inner: std::sync::Mutex<PipelineState>,
}

struct PipelineState {
    /// LSN up to which the WAL is durably fsynced.
    flushed_lsn: u64,
    /// True when a leader is currently performing flush+fsync.
    leader_active: bool,
    /// LSN the leader promises to fsync up to (allows EXPIRED check).
    pending_lsn: u64,
    /// Waiters queued while a leader is active.
    waiters: Vec<Waiter>,
}

struct Waiter {
    lsn: u64,
    tx: tokio::sync::oneshot::Sender<Result<(), DbError>>,
}

enum AcquireResult {
    /// flushed_lsn >= requested lsn — no fsync needed.
    Expired,
    /// Caller is the leader — must perform flush+fsync, then call release().
    Acquired { pending: Vec<Waiter> },
    /// Caller is queued — await the oneshot receiver.
    Queued(tokio::sync::oneshot::Receiver<Result<(), DbError>>),
}
```

### `acquire(lsn)` — O(1) fast path

```
lock inner:
  if flushed_lsn >= lsn → return Expired
  if !leader_active:
    leader_active = true
    pending_lsn = lsn
    drain waiters → return Acquired { pending: drained }
  else:
    create oneshot (tx, rx)
    push Waiter { lsn, tx }
    pending_lsn = max(pending_lsn, lsn)
    return Queued(rx)
```

### `release(new_flushed_lsn, result)` — wake followers

```
lock inner:
  flushed_lsn = new_flushed_lsn
  leader_active = false
  // Partition waiters: satisfied (lsn <= new_flushed_lsn) vs remaining
  drain satisfied waiters → to_wake
  if remaining waiters exist:
    // Designate next leader: pop one waiter, set leader_active = true
    leader_active = true
    pending_lsn = max(remaining lsns)
    next_leader_waiter = remaining.pop()
    → will be woken as Acquired
unlock
// Outside lock: send result to all satisfied waiters
// Send Acquired signal to next leader (via special variant or by returning
// the drained batch through the oneshot)
```

### Integration into TxnManager::commit()

```rust
// Current code (immediate mode):
//   self.wal.commit()?;           // flush + sync_all
//   self.max_committed = txn_id;
//
// Current code (deferred mode):
//   self.pending_deferred_txn_id = Some(txn_id);
//
// New code (pipeline, always):
//   let commit_lsn = self.wal.current_lsn();
//   // BufWriter has the Commit entry but NOT flushed
//   return Ok(CommitAction::Pipeline { commit_lsn, txn_id })

pub enum CommitAction {
    /// Read-only txn — already flush_no_sync'd, max_committed advanced.
    Done,
    /// DML txn — caller must drive the pipeline to fsync.
    Pipeline { commit_lsn: u64, txn_id: TxnId },
}
```

### Integration into Database::execute_query()

```rust
let (result, commit_action) = execute_and_commit(...);
match commit_action {
    CommitAction::Done => return Ok((result, None)),
    CommitAction::Pipeline { commit_lsn, txn_id } => {
        // Try acquire WHILE HOLDING the Database lock (fast — no I/O)
        match self.pipeline.acquire(commit_lsn) {
            Expired => {
                // Another leader already fsynced past our LSN
                self.txn.advance_committed_single(txn_id);
                return Ok((result, None));
            }
            Acquired { pending } => {
                // We are the leader — do flush+fsync under Database lock
                self.txn.wal_flush_and_fsync()?;
                let flushed_lsn = self.txn.wal_current_lsn();
                self.txn.advance_committed_single(txn_id);
                // advance committed for all pending waiters too
                for w in &pending { self.txn.advance_committed_single(w.txn_id); }
                self.pipeline.release(flushed_lsn, Ok(()));
                // wake pending (outside lock ideally, but ok under lock for now)
                return Ok((result, None));
            }
            Queued(rx) => {
                // Release Database lock, then await
                return Ok((result, Some(rx)));
            }
        }
    }
}
```

Key insight: the **leader does fsync under the Database lock**, same as current
group commit. The `Expired` and `Acquired` paths return immediately (no async wait).
Only `Queued` followers release the lock and await.

## Implementation phases

### Phase 1: Create `FsyncPipeline` struct (crate: axiomdb-wal)
1. Add `fsync_pipeline.rs` with `FsyncPipeline`, `PipelineState`, `AcquireResult`
2. Implement `acquire(lsn)` and `release(flushed_lsn, result)`
3. Unit tests: Expired path, Acquired path, Queued path, error propagation
4. Add `tokio` dep to axiomdb-wal Cargo.toml (only `sync` feature for oneshot)

### Phase 2: Wire into TxnManager
1. Add `CommitAction` enum to txn.rs
2. Modify `TxnManager::commit()` to return `CommitAction` instead of `Result<()>`
3. Remove `deferred_commit_mode` / `pending_deferred_txn_id` fields
4. Add `advance_committed_single(txn_id)` method
5. Fix all call sites that use `txn.commit()` (executor, tests)

### Phase 3: Wire into Database + Handler
1. Add `pipeline: FsyncPipeline` field to `Database`
2. Modify `execute_query` to handle `CommitAction::Pipeline`
3. Modify handler's `await_commit_rx` to accept pipeline's oneshot
4. Remove `CommitCoordinator`, `group_commit.rs`, `commit_coordinator.rs`
5. Remove `enable_group_commit()`, `take_commit_rx()` old path
6. Remove `group_commit_interval_ms` / `group_commit_max_batch` from config
7. Update server startup to not call `enable_group_commit()`

### Phase 4: Tests + Benchmark
1. Integration tests: single-connection pipeline, multi-connection batching
2. Adapt existing group_commit integration tests
3. Run `local_bench.py --scenario insert_autocommit --rows 1000`
4. Verify ≥ 5K ops/s target

## Tests to write

### Unit (in fsync_pipeline.rs)
- `test_expired_when_already_flushed` — acquire(lsn=5) when flushed_lsn=10 → Expired
- `test_acquired_when_no_leader` — acquire(lsn=5) when flushed_lsn=0 → Acquired
- `test_queued_when_leader_active` — first acquire → Acquired, second acquire → Queued
- `test_release_wakes_followers` — Acquired + 3 Queued → release → all 3 get Ok
- `test_release_error_propagates` — Acquired + 2 Queued → release with Err → all get Err
- `test_next_leader_designated` — Acquired + 2 Queued (lsn > flushed) → release (partial) → one becomes next leader
- `test_flushed_lsn_monotonic` — release never regresses flushed_lsn

### Integration (in integration_fsync_pipeline.rs)
- `test_single_connection_autocommit_pipeline` — 100 INSERTs autocommit → all committed, data correct
- `test_pipeline_survives_crash` — INSERT → pipeline commit → kill → recover → data present
- `test_pipeline_fsync_error_enters_degraded` — simulate I/O error on fsync → DiskFull/degraded mode

## Anti-patterns to avoid

- **DO NOT hold std::sync::Mutex across async .await** — the pipeline's Mutex is only for the ~100ns state check, never for I/O.
- **DO NOT advance max_committed before fsync** — a client must never see Ok if the WAL isn't durable.
- **DO NOT remove the inline fsync path for embedded** — embedded crate has no network layer. Keep `TxnManager::commit()` returning `CommitAction` so the embedded caller can do its own fsync inline.
- **DO NOT create a background Tokio task** — the whole point is inline leader election, not timer-based batching.

## Risks

| Risk | Mitigation |
|---|---|
| Leader panics mid-fsync → followers stuck | oneshot Sender dropped → Receiver gets RecvError → treated as fsync fail |
| Lock contention on PipelineState mutex | Mutex held only for state read+update (~100ns). No I/O inside. |
| Embedded crate regression | CommitAction::Pipeline returned; embedded caller calls inline fsync. Add test. |
| WAL rotation during pipeline | Rotation already requires Database lock. Pipeline operates under same lock. Safe. |
| Breaking change to commit() return type | All callers updated in Phase 2. Compiler catches missing matches. |
