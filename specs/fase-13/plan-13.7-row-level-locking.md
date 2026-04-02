# Plan: 13.7 Row-level locking

## Files to create/modify

- `crates/axiomdb-network/src/mysql/database.rs` — split global database state
  from per-connection transaction state; remove the assumption that one
  `TxnManager.active` transaction represents the whole server.
- `crates/axiomdb-network/src/mysql/handler.rs` — replace `db.write()` as the
  write-concurrency gate with async row-lock acquisition and connection-owned
  transaction execution.
- `crates/axiomdb-network/src/mysql/lock_manager.rs` — new async row-lock
  manager keyed by `(table_id, RecordId)`, including waiter queues and release.
- `crates/axiomdb-wal/src/txn.rs` — split shared transaction oracle/WAL
  coordination from per-transaction undo state so multiple write transactions
  can exist concurrently.
- `crates/axiomdb-sql/src/executor/update.rs` — expose a two-phase update flow:
  discover target rows, then apply mutation to rows already locked by the caller.
- `crates/axiomdb-sql/src/executor/delete.rs` — same two-phase flow for delete.
- `crates/axiomdb-sql/src/session.rs` — keep connection-owned transaction
  runtime and owned row-lock tokens; clear them on commit/rollback.
- `crates/axiomdb-network/tests/integration_concurrency.rs` — add row-lock
  conflict and disjoint-writer tests.
- `crates/axiomdb-network/tests/integration_isolation_levels.rs` — upgrade
  tests from single-session snapshot checks to true cross-session write cases.

## Algorithm / Data structure

Use a two-layer concurrency model:

1. Shared transaction/WAL oracle
   - `next_txn_id: AtomicU64`
   - `max_committed: AtomicU64`
   - WAL append/flush coordinated behind a dedicated mutex/pipeline
   - no global `active: Option<ActiveTxn>`

2. Connection-owned transaction state
   - `txn_id`
   - `snapshot_id_at_begin`
   - isolation level
   - undo log
   - deferred frees
   - owned row locks

3. Async row-lock manager
   - key: `(table_id, RecordId)`
   - value: owner txn + FIFO waiter queue
   - same-owner reentry succeeds immediately
   - conflicting owner causes waiter registration

Pseudocode:

```text
BEGIN(session):
  if session.txn exists -> TransactionAlreadyActive
  txn_id = shared.next_txn_id.fetch_add(1)
  wal.append(BEGIN(txn_id))
  session.txn = ActiveTxn { txn_id, snapshot_id_at_begin, isolation, undo_ops, locks=[] }

UPDATE/DELETE(session, stmt):
  snap = session.txn.active_snapshot_or_shared()
  candidates = discover_candidate_rids(stmt, snap)
  sort candidates by (table_id, page_id, slot_id)
  await lock_manager.acquire_many(session.txn_id, candidates, timeout)
  refreshed = reread_and_recheck(stmt, candidates, session.snapshot_policy)
  apply_heap_and_index_mutation(refreshed, session.txn.undo_ops, wal, storage)

COMMIT(session):
  wal.append(COMMIT(txn_id))
  wal.flush/fsync via shared pipeline
  shared.max_committed.store(txn_id)
  release row locks owned by txn_id
  release deferred frees now that commit is durable
  session.txn = None

ROLLBACK(session):
  apply undo_ops in reverse
  wal.append(ROLLBACK(txn_id))
  release row locks owned by txn_id
  discard deferred frees
  session.txn = None
```

Critical recheck rule:

```text
for each locked RecordId:
  row = read current tuple image
  if row is gone or invisible to this txn -> skip
  if WHERE no longer matches current row -> skip
  else mutate
```

This mirrors PostgreSQL/MySQL behavior where a waiter must re-evaluate the
row after the blocker commits.

## Implementation phases

1. Split transaction ownership
   - Introduce shared transaction oracle + per-session active transaction state.
   - Remove the assumption that the whole server has only one active writer txn.

2. Add row-lock manager
   - Implement async acquire/release for `(table_id, RecordId)`.
   - Support same-owner reentry and timeout cleanup.

3. Refactor UPDATE/DELETE into discover + apply
   - Candidate discovery remains snapshot-based and read-only.
   - Mutation path receives already-locked rows and performs post-lock recheck.

4. Rewire server execution path
   - Read-only queries keep the lock-free path.
   - Mutating queries acquire row locks, then run the apply phase.
   - `COMMIT` / `ROLLBACK` release all owned row locks.

5. Validate concurrency semantics
   - Add integration tests for disjoint writers, same-row conflicts,
     rollback release, and reader non-blocking behavior.

## Tests to write

- unit: row-lock manager acquire/release, same-owner reentry, timeout cleanup
- unit: deterministic lock ordering for `acquire_many`
- unit: post-lock recheck skips rows deleted by the blocker
- integration: two sessions update different rows concurrently
- integration: two sessions update the same row and the second waits
- integration: rollback releases row locks and wakes waiters
- integration: delete/update conflict returns 0 affected rows after recheck
- integration: read-only SELECT is not blocked by row locks
- bench: 2, 4, 8 concurrent UPDATEs on disjoint rows compared with the current
  single-writer baseline

## Anti-patterns to avoid

- Do not keep `Arc<RwLock<Database>>::write()` as the effective write gate.
- Do not wait for row locks while holding WAL, storage, or schema-cache mutexes.
- Do not acquire row locks in planner/discovery order; sort by stable row key.
- Do not leave transaction ownership in a single global `TxnManager.active`.
- Do not block the Tokio runtime with `std::sync::Condvar` waits inside the
  connection handler path.

## Risks

- `TxnManager` is structurally single-active today.
  Mitigation: split shared monotonic state/WAL coordination from per-session undo state first.

- `MmapStorage` and page growth assume whole-database exclusivity.
  Mitigation: keep growth and durable flush under short internal critical sections;
  do not hold them across row-lock waits.

- Concurrent uniqueness checks may still race on insert-heavy workloads.
  Mitigation: keep `13.7` scoped to row-locked `UPDATE` / `DELETE`; document the
  remaining insert/key-lock gap explicitly.

- Async waiting can deadlock if waiters hold unrelated resources.
  Mitigation: row-lock acquire happens before heap/index mutation and before WAL flush.

- Without `13.8`, true deadlocks degrade to timeout-based failure.
  Mitigation: acquire rows in sorted order now, then add wait-graph DFS in `13.8`.
