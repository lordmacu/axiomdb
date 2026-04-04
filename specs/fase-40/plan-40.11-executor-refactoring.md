# Plan: 40.11 — Executor Refactoring

## Files to modify

| File | Signatures | Change type |
|---|---|---|
| `crates/axiomdb-sql/src/executor/mod.rs` | ~15 | Dispatch: add ExecutionContext param |
| `crates/axiomdb-sql/src/executor/insert.rs` | ~12 | `&mut` → `&` + lock_mgr.acquire(IX) |
| `crates/axiomdb-sql/src/executor/update.rs` | ~15 | `&mut` → `&` + lock_mgr.acquire(IX,X) per row |
| `crates/axiomdb-sql/src/executor/delete.rs` | ~12 | `&mut` → `&` + lock_mgr.acquire(IX,X) per row |
| `crates/axiomdb-sql/src/executor/select.rs` | ~8 | `&mut` → `&` + lock_mgr.acquire(IS) |
| `crates/axiomdb-sql/src/executor/ddl.rs` | ~10 | `&mut` → `&` (DDL uses catalog_lock) |
| `crates/axiomdb-sql/src/table.rs` | ~20 | TableEngine methods: `&mut` → `&` |
| `crates/axiomdb-sql/src/index_maintenance.rs` | ~15 | `&mut` → `&` |
| `crates/axiomdb-sql/src/fk_enforcement.rs` | ~8 | `&mut` → `&` |
| `crates/axiomdb-sql/src/vacuum.rs` | ~5 | `&mut` → `&` |
| `crates/axiomdb-storage/src/heap_chain.rs` | ~43 | `&mut` → `&` (mechanical) |
| `crates/axiomdb-index/src/tree.rs` | ~25 | `&mut` → `&` (mechanical) |
| `crates/axiomdb-wal/src/txn.rs` | ~11 | Adapt to ConnectionTxn API |
| `crates/axiomdb-catalog/src/bootstrap.rs` | ~5 | `&mut` → `&` |
| Tests (~10 files) | ~20 | Adapt to new API |

## Implementation phases

### Phase 1: ExecutionContext struct
```rust
/// Bundles shared subsystems passed to every executor function.
/// Avoids 6+ extra parameters on every function.
pub struct ExecutionContext<'a> {
    pub storage: &'a dyn StorageEngine,
    pub coord: &'a TxnCoordinator,
    pub lock_mgr: &'a LockManager,
    pub wal: &'a ConcurrentWalWriter,
    pub bloom: &'a BloomRegistry,
    pub allocator: &'a GlobalPageAllocator,
}
```

### Phase 2: Mechanical `&mut` → `&` across storage + index crates
**Order**: axiomdb-storage → axiomdb-index → axiomdb-catalog → axiomdb-wal
Compile after each crate. No logic changes — just remove `mut`.

### Phase 3: TxnManager → ConnectionTxn + TxnCoordinator
Replace `txn: &mut TxnManager` parameters with:
- `txn: &mut ConnectionTxn` (per-connection, holds undo_ops, snapshot)
- `coord: &TxnCoordinator` (via ExecutionContext, holds atomic IDs, WAL)

WAL recording methods move from TxnManager to ConcurrentWalWriter,
called via ConnectionTxn's wal_scratch buffer.

### Phase 4: Lock integration in INSERT executor
```rust
fn execute_insert_ctx(
    stmt: InsertStmt,
    exec: &ExecutionContext,
    txn: &mut ConnectionTxn,
    page_batch: &mut LocalPageBatch,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    // 1. Resolve table
    let resolved = resolve_table_cached(...)?;

    // 2. Acquire table intention lock
    exec.lock_mgr.acquire_table(txn.txn_id, resolved.def.id, LockMode::IX)?;

    // 3. Execute insert (no row lock needed for new rows)
    // ... existing insert logic with &dyn StorageEngine ...
}
```

### Phase 5: Lock integration in UPDATE executor
```rust
fn execute_update_ctx(
    stmt: UpdateStmt,
    exec: &ExecutionContext,
    txn: &mut ConnectionTxn,
    page_batch: &mut LocalPageBatch,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let resolved = resolve_table_cached(...)?;

    // 1. Table intention lock
    exec.lock_mgr.acquire_table(txn.txn_id, resolved.def.id, LockMode::IX)?;

    // 2. Collect candidates (existing logic)
    let candidates = collect_candidates(...)?;

    // 3. For each candidate: lock THEN modify
    for (rid, row) in candidates {
        // 3a. Acquire row X-lock (may wait or deadlock)
        let lock_result = exec.lock_mgr.acquire_row(
            txn.txn_id, rid.page_id, rid.slot_id, LockMode::X
        )?;

        // 3b. Re-verify if we had to wait
        if lock_result == LockResult::Waited {
            let page = exec.storage.read_page(rid.page_id)?;
            if !is_still_visible_and_matching(&page, rid.slot_id, &txn.snapshot, &where_clause) {
                continue; // row changed while we waited — skip
            }
        }

        // 3c. Now safe to modify
        modify_row(exec.storage, rid, new_values, txn)?;
    }

    Ok(QueryResult::Affected { count: matched, last_insert_id: None })
}
```

### Phase 6: Lock integration in DELETE executor
Same pattern as UPDATE: IX(table) → X(row) per candidate → re-verify → mark deleted.

### Phase 7: Lock integration in SELECT
```rust
fn execute_select_ctx(...) {
    // Only table intention lock (IS) — no row locks for plain SELECT
    exec.lock_mgr.acquire_table(txn.txn_id, table_id, LockMode::IS)?;
    // ... existing MVCC-based scan logic ...
}
```

### Phase 8: Deadlock error propagation
At every lock_mgr.acquire() call site, handle:
- `Ok(Granted)` → proceed normally
- `Ok(Waited)` → re-verify row, then proceed
- `Err(DbError::Deadlock)` → propagate immediately (DO NOT retry)
- `Err(DbError::LockTimeout)` → propagate immediately

The connection handler catches these errors and sends appropriate MySQL error
packets to the client (ER_LOCK_DEADLOCK, ER_LOCK_WAIT_TIMEOUT).

### Phase 9: Test adaptation
All existing tests use `&mut dyn StorageEngine` → change to `&dyn StorageEngine`.
Most tests don't test concurrency → lock_mgr can be a no-op stub for unit tests:
```rust
struct NoOpLockManager;
impl LockManager for NoOpLockManager {
    fn acquire_table(...) -> Result<LockResult> { Ok(LockResult::Granted) }
    fn acquire_row(...) -> Result<LockResult> { Ok(LockResult::Granted) }
    fn release_all(...) {}
}
```

## Tests to write

1. **INSERT + lock**: INSERT acquires IX(table), verify via lock_mgr state
2. **UPDATE + row lock**: UPDATE acquires X(row) per candidate, verify
3. **DELETE + row lock**: DELETE acquires X(row) per candidate, verify
4. **SELECT — no row lock**: plain SELECT acquires IS(table) only
5. **Deadlock propagation**: lock_mgr returns Deadlock → executor propagates error
6. **Lock timeout**: lock_mgr returns LockTimeout → executor propagates error
7. **Re-verify after wait**: UPDATE waits for lock → row changed → skipped correctly
8. **Concurrent UPDATE same row**: second UPDATE waits for first's X(row)
9. **Concurrent UPDATE different rows**: both proceed (different X locks)
10. **All existing unit tests pass with NoOpLockManager**

## Anti-patterns to avoid

- DO NOT acquire row lock AFTER modification (race condition → corruption)
- DO NOT hold row lock across WAL write (release row lock, then write WAL)
  Wait — actually InnoDB DOES hold latch through modification + WAL logging within MTR.
  For AxiomDB: hold row lock until transaction commit (this is correct — locks released on COMMIT)
- DO NOT retry deadlock inside executor (propagate to connection handler)
- DO NOT acquire table X-lock for DML (only IX — table X is for DDL only)
- DO NOT add lock_mgr parameter to every helper function — use ExecutionContext

## Risks

- **190 signature changes**: largest mechanical change in the project.
  Mitigation: compile after each crate. Start with leaf crates (storage, index),
  then intermediate (catalog, wal), then executor (sql).
- **Re-verification logic**: if incorrect, rows may be skipped or double-processed.
  Mitigation: thorough tests with concurrent access patterns.
- **Lock hold duration**: locks held from acquire until COMMIT. Long transactions
  hold locks for a long time → blocking others. This is correct behavior but
  applications should use short transactions.
