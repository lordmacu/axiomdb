# Spec: 40.11 — Executor Refactoring

## What to build (not how)

Update all SQL executor code to:
1. Use `&dyn StorageEngine` instead of `&mut dyn StorageEngine` (184 signatures)
2. Integrate row-level lock acquisition at the correct points in each DML operation
3. Accept `ConnectionTxn` and shared subsystems instead of `&mut TxnManager`
4. Handle deadlock errors from the lock manager

## Research findings

### InnoDB lock acquisition sequence (primary reference)
- **INSERT**: `lock_table(IX)` → index traverse → `lock_rec_insert_check_and_lock()`
  (gap lock check only, no explicit lock on new row) → insert record
- **UPDATE**: `lock_table(IX)` → cursor restore to old row →
  `lock_clust_rec_modify_check_and_lock(X | REC_NOT_GAP)` BEFORE any modification →
  update in-place or delete+insert
- **DELETE**: `lock_table(IX)` → find row → `lock_clust_rec_modify_check_and_lock(X)`
  BEFORE marking deleted
- **SELECT**: MVCC consistent read — NO locks (unless FOR UPDATE/FOR SHARE)
- **SELECT FOR UPDATE**: `lock_clust_rec_read_check_and_lock(X)` per row

### PostgreSQL lock acquisition
- **Table lock in InitPlan** (before executor runs) — RowExclusiveLock for DML
- **Row lock inside heap functions** — `heap_update()` acquires buffer X-lock +
  `HeapTupleSatisfiesUpdate()` check before modification
- **Plain SELECT**: NO locks — pure MVCC
- **Deadlock handling**: at executor level, catches error and propagates to client

### Critical rule for AxiomDB (both databases agree)
```
RULE: Acquire row lock BEFORE modifying the row. Never modify then lock.
```

This prevents the race condition:
```
Thread A: reads row → modifies row → tries to lock (too late! Thread B already modified)
Thread B: reads row → modifies row → tries to lock (both corrupted)
```

## Current executor signatures (what changes)

### Pattern today (190 signatures)
```rust
fn execute_insert_ctx(
    stmt: InsertStmt,
    storage: &mut dyn StorageEngine,    // ← becomes &dyn
    txn: &mut TxnManager,               // ← becomes &mut ConnectionTxn + &TxnCoordinator
    bloom: &mut BloomRegistry,           // ← becomes &RwLock<BloomRegistry>
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>
```

### Pattern after
```rust
fn execute_insert_ctx(
    stmt: InsertStmt,
    storage: &dyn StorageEngine,         // ← &self (interior mutability from 40.3)
    txn: &mut ConnectionTxn,             // ← per-connection txn (from 40.2)
    coord: &TxnCoordinator,             // ← shared coordinator (from 40.2)
    lock_mgr: &LockManager,             // ← shared lock manager (from 40.5)
    wal: &ConcurrentWalWriter,          // ← shared WAL (from 40.4)
    page_batch: &mut LocalPageBatch,    // ← per-txn page allocator (from 40.9)
    bloom: &BloomRegistry,              // ← read-only ref (write via RwLock elsewhere)
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError>
```

**Alternative**: Bundle shared subsystems into a context struct to reduce parameter count:
```rust
pub struct ExecutionContext<'a> {
    pub storage: &'a dyn StorageEngine,
    pub coord: &'a TxnCoordinator,
    pub lock_mgr: &'a LockManager,
    pub wal: &'a ConcurrentWalWriter,
    pub bloom: &'a BloomRegistry,
}
```

## Lock integration points per DML operation

### INSERT
```
1. Acquire IS(table) from lock_mgr                      ← table intention lock
2. Parse and coerce values
3. Encode row data
4. HeapChain::insert()                                   ← acquires page X-latch internally
   - No explicit row lock needed (new row, no conflicts)
   - Gap lock check if needed (isolation ≥ REPEATABLE READ)
5. Insert into all secondary indexes
6. Record WAL entry via ConnectionTxn → ConcurrentWalWriter
7. Return (page_id, slot_id) as RecordId
```

**No explicit row lock for INSERT** (matches InnoDB): the row doesn't exist yet.
Gap locks (40.5 extension) prevent phantom inserts at the same position.

### UPDATE
```
1. Acquire IX(table) from lock_mgr                      ← table intention lock
2. Find candidate rows (scan or index lookup)
3. For each candidate row:
   a. Acquire X(row) from lock_mgr                      ← ROW LOCK BEFORE MODIFICATION
      - If blocked: wait (may trigger deadlock detection from 40.6)
      - If deadlock: return DbError::Deadlock → rollback
   b. Re-read row under lock (verify still visible via MVCC)
   c. Evaluate SET expressions
   d. If changed: modify row (page X-latch from storage)
   e. Update secondary indexes if key columns changed
   f. Record WAL entry
```

**Critical**: step (a) BEFORE step (d). Lock then modify. Never reverse.

### DELETE
```
1. Acquire IX(table) from lock_mgr                      ← table intention lock
2. Find candidate rows
3. For each candidate row:
   a. Acquire X(row) from lock_mgr                      ← ROW LOCK BEFORE DELETION
   b. Re-read row under lock (verify still visible)
   c. Mark RowHeader.txn_id_deleted = current_txn
   d. Update page
   e. Record WAL entry
```

### SELECT (plain — no locks)
```
1. Acquire IS(table) from lock_mgr                      ← table intention lock only
2. Scan or index lookup
3. For each row: check MVCC visibility → include if visible
4. NO row locks acquired
```

### SELECT FOR UPDATE
```
1. Acquire IS(table) from lock_mgr
2. Scan or index lookup
3. For each visible row:
   a. Acquire X(row) or S(row) from lock_mgr            ← row lock per result row
   b. Re-check visibility (row may have changed between scan and lock)
   c. Include in results
```

## Deadlock handling at executor level

```rust
fn execute_update_with_locks(
    exec_ctx: &ExecutionContext,
    txn: &mut ConnectionTxn,
    lock_mgr: &LockManager,
    // ...
) -> Result<QueryResult, DbError> {
    // Acquire table intention lock
    lock_mgr.acquire_table(txn.txn_id, table_id, LockMode::IntentionExclusive)?;

    for (rid, row) in candidates {
        // Acquire row lock — may block or deadlock
        match lock_mgr.acquire_row(txn.txn_id, rid.page_id, rid.slot_id, LockMode::Exclusive) {
            Ok(LockResult::Granted) => {},
            Ok(LockResult::Waited) => {
                // Lock was acquired after waiting — re-verify row visibility
                let page = storage.read_page(rid.page_id)?;
                let hdr = read_row_header(&page, rid.slot_id);
                if !hdr.is_visible(&txn.snapshot) {
                    continue; // row was modified/deleted while we waited — skip
                }
            },
            Err(DbError::Deadlock { .. }) => {
                // Deadlock detected — we are the victim
                // DON'T retry here — propagate to caller for full rollback
                return Err(DbError::Deadlock { ... });
            },
            Err(DbError::LockTimeout) => {
                return Err(DbError::LockTimeout);
            },
            Err(e) => return Err(e),
        }

        // Now safe to modify — we hold X lock on this row
        modify_row(storage, rid, new_values)?;
    }

    Ok(QueryResult::Affected { count, last_insert_id: None })
}
```

## Re-verification after lock wait

**Critical correctness requirement** (both InnoDB and PostgreSQL do this):

When a row lock was WAITED (not immediately granted), the row may have been modified
by the transaction that previously held the lock. We MUST re-read the row and re-check:

1. **Is it still visible?** (another txn may have deleted it)
2. **Does it still match the WHERE clause?** (another txn may have updated it)
3. **Is the data still consistent?** (re-evaluate the update expression)

InnoDB calls this **"pessimistic re-check"**. PostgreSQL calls it **"EvalPlanQual"**
(re-evaluate the query plan on the locked tuple).

## Scope of 184 signature changes

### By crate
| Crate | Files | Signatures | Change |
|---|---|---|---|
| axiomdb-sql/executor/ | 8 files | ~100 | `&mut dyn StorageEngine` → `&dyn StorageEngine` + add lock_mgr param |
| axiomdb-sql/ | 5 files | ~30 | table.rs, index_maintenance.rs, fk_enforcement.rs, vacuum.rs |
| axiomdb-storage/ | 3 files | ~43 | heap_chain.rs, heap.rs, clustered_tree.rs |
| axiomdb-index/ | 1 file | ~25 | tree.rs |
| axiomdb-wal/ | 3 files | ~11 | txn.rs, checkpoint.rs, recovery.rs |
| axiomdb-catalog/ | 1 file | ~5 | bootstrap.rs |
| Tests | ~10 files | ~20 | Integration tests |

### Mechanical change (no logic change)
Most of the 184 changes are purely `&mut` → `&`. No logic changes needed for these.
The storage layer already has interior mutability (40.3).

### Logic changes (lock integration)
Only ~10 functions need actual logic changes (adding lock_mgr.acquire() calls):
- `execute_insert_ctx()`
- `execute_update_ctx()`
- `execute_delete_ctx()`
- `execute_select_ctx()` (for FOR UPDATE)
- `collect_delete_candidates()` (shared by UPDATE and DELETE)
- `apply_update_index_maintenance()`
- `TableEngine::insert_row()`
- `TableEngine::update_rows_preserve_rid()`

## Acceptance criteria

- [ ] Zero `&mut dyn StorageEngine` in production code
- [ ] Zero `&mut TxnManager` in production code (uses ConnectionTxn or TxnCoordinator)
- [ ] INSERT: acquires IX(table) before insertion
- [ ] UPDATE: acquires IX(table) + X(row) BEFORE modification per row
- [ ] DELETE: acquires IX(table) + X(row) BEFORE delete-mark per row
- [ ] SELECT: acquires IS(table) only (no row locks unless FOR UPDATE)
- [ ] Deadlock error propagated to caller (not retried internally)
- [ ] Lock timeout error propagated to caller
- [ ] Re-verification after lock wait (re-read row, re-check visibility and WHERE)
- [ ] ExecutionContext struct bundles shared subsystems
- [ ] All existing tests adapted and passing
- [ ] Wire protocol smoke test: concurrent INSERT + SELECT
- [ ] Wire protocol: UPDATE causes lock wait → second UPDATE succeeds after first commits

## Out of scope

- SELECT FOR UPDATE / FOR SHARE syntax (future SQL feature)
- Gap locks for phantom prevention (requires B-tree latch integration)
- Lock escalation (row → table)
- Predicate locking for serializable isolation

## Dependencies

- 40.3 (StorageEngine interior mutability) — &dyn StorageEngine with &self
- 40.5 (Lock Manager) — lock_mgr.acquire_table(), lock_mgr.acquire_row()
- 40.6 (Deadlock Detection) — DbError::Deadlock handling
- 40.10 (Database lock redesign) — SharedDatabase provides subsystems
