# Plan: 3.5a + 3.5b + 3.5c — Autocommit + Implicit transactions + Statement-level rollback

## Files to create/modify

- `crates/axiomdb-wal/src/txn.rs` — add `savepoint()` + `rollback_to_savepoint()`
- `crates/axiomdb-sql/src/executor.rs` — read `ctx.autocommit`, change wrap logic, add savepoint on error
- `crates/axiomdb-network/src/mysql/database.rs` — pass autocommit context; handle DDL implicit commit
- `crates/axiomdb-sql/src/lib.rs` — expose new test helpers if needed
- `crates/axiomdb-network/src/mysql/session.rs` — verify `autocommit` accessible from `SessionContext` (likely already correct)
- `crates/axiomdb-sql/tests/` or `crates/axiomdb-network/tests/` — integration tests

---

## Algorithm / Data structure

### Step 1 — Savepoint in TxnManager (`txn.rs`)

`TxnManager` already has `undo_ops: Vec<UndoOp>`. A savepoint is just an index into it.

```rust
/// Opaque savepoint: the undo_ops length at the time of capture.
pub struct Savepoint(usize);

impl TxnManager {
    /// Capture current undo log position. O(1).
    /// Only valid when a transaction is active.
    pub fn savepoint(&self) -> Savepoint {
        Savepoint(self.undo_ops().len())   // undo_ops is Vec<UndoOp>
    }

    /// Undo all ops recorded after `sp`, leaving the transaction active.
    /// Replays undo ops in reverse from the end down to sp.0. O(ops since savepoint).
    pub fn rollback_to_savepoint(
        &mut self,
        sp: Savepoint,
        storage: &mut impl StorageEngine,
    ) -> Result<()> {
        // Replay undo ops from end down to sp.0, then truncate
        let ops_to_undo = self.undo_ops_mut().drain(sp.0..).rev().collect::<Vec<_>>();
        for op in ops_to_undo {
            apply_undo_op(op, storage)?;
        }
        Ok(())
    }
}
```

No new on-disk format. Savepoints are in-memory only — they don't survive crashes
(a crash still recovers the full transaction from WAL, or undoes it completely).

### Step 2 — Executor autocommit logic (`executor.rs`)

Replace the current autocommit wrap logic with one that reads `ctx.autocommit`.

```
Current logic (always autocommit):
  if active_txn: execute directly
  else: begin → execute → commit (or rollback on error)

New logic:
  if active_txn:
      sp = txn.savepoint()
      match execute(stmt):
          Ok(r)  => Ok(r)                          // savepoint abandoned
          Err(e) => rollback_to_savepoint(sp); Err(e) // statement rolled back, txn lives

  else if ctx.autocommit == true (default):
      begin → execute → commit   (or rollback on error) // unchanged behavior

  else (autocommit=false, no active txn):
      match stmt:
          BEGIN    => begin(); Ok(Empty)
          COMMIT   => Err(NoActiveTransaction)
          ROLLBACK => Err(NoActiveTransaction)
          DDL      => begin(); execute(); commit()  // DDL auto-commits in MySQL
          DML/SELECT => begin(); execute()          // implicit BEGIN, NO commit
                        // SELECT read-only: no begin needed, use snapshot
```

**Read-only SELECT optimization:** SELECT without FOR UPDATE doesn't need a transaction.
Use `txn.snapshot()` directly (already what happens today). No implicit BEGIN for SELECT.

### Step 3 — DDL implicit commit (`database.rs` or `executor.rs`)

MySQL behavior: DDL causes implicit COMMIT of any open transaction, then DDL runs in its own autocommit transaction.

```rust
// In execute_with_ctx, before dispatching DDL when autocommit=false and txn active:
if is_ddl(&stmt) && txn.active_txn_id().is_some() {
    txn.commit()?;   // implicit COMMIT of open txn
}
// Then fall through to autocommit-wrapped DDL execution
```

`is_ddl()` already exists as `is_schema_changing()` in `database.rs`.

---

## Implementation phases

1. **Add `savepoint()` + `rollback_to_savepoint()`** to `TxnManager` in `txn.rs`
   - Unit test: savepoint → insert → rollback_to_savepoint → row gone, txn still active

2. **Add autocommit flag to executor context**
   - Confirm `SessionContext` (the `ctx` parameter) has access to `autocommit`
   - If not: add `autocommit: bool` field to `SessionContext` or pass it directly
   - Read it in `execute_with_ctx()`

3. **Rewrite the no-active-txn branch in `execute_with_ctx()`**
   - `autocommit=true`: unchanged (begin/execute/commit)
   - `autocommit=false` + DML: begin (if not active) → execute, no commit
   - `autocommit=false` + SELECT: execute with snapshot, no begin
   - `autocommit=false` + DDL + open txn: commit open txn first, then DDL in autocommit

4. **Add savepoint wrap in active-txn branch**
   - `savepoint()` before dispatch
   - `rollback_to_savepoint()` on error

5. **Integration tests**
   - ORM flow: SET autocommit=0 → DML → COMMIT → verify committed
   - Rollback flow: SET autocommit=0 → DML → ROLLBACK → verify not committed
   - Error flow: BEGIN → ok DML → failing DML → ok DML → COMMIT → verify 2 rows
   - pymysql end-to-end: connect with autocommit=False, run typical ORM operations

---

## Tests to write

**Unit tests (`txn.rs`):**
- `test_savepoint_rollback_undoes_inserts` — insert 3 rows, savepoint, insert 2 more, rollback_to_savepoint, verify 3 rows remain, txn still active, can commit
- `test_savepoint_noop_on_success` — savepoint, insert, success, txn active, commit, verify row committed

**Integration tests (`tests/`):**
- `test_autocommit_false_no_auto_commit` — SET autocommit=0; INSERT; disconnect (no COMMIT); reconnect; SELECT → 0 rows
- `test_autocommit_false_explicit_commit` — SET autocommit=0; INSERT; COMMIT; SELECT → 1 row
- `test_autocommit_false_explicit_rollback` — SET autocommit=0; INSERT; ROLLBACK; SELECT → 0 rows
- `test_autocommit_false_select_no_txn` — SET autocommit=0; SELECT; verify no open txn after
- `test_ddl_commits_open_txn` — SET autocommit=0; INSERT; CREATE TABLE; verify INSERT committed
- `test_error_in_explicit_txn_keeps_txn_active` — BEGIN; INSERT ok; INSERT error (dup PK); INSERT ok; COMMIT; SELECT → 2 rows
- `test_autocommit_resets_on_reconnect` — SET autocommit=0; reconnect; verify autocommit=1
- `test_select_autocommit_show_variables` — SET autocommit=0; SELECT @@autocommit; verify returns 0

---

## Anti-patterns to avoid

- **DO NOT** add `autocommit: bool` as parameter to `execute_with_ctx()` — it's already
  in `ctx: &SessionContext`. Adding it as a separate param creates two sources of truth.
- **DO NOT** handle autocommit in `handler.rs` (wire layer) — it's executor/database logic,
  not protocol logic. Keep layers clean.
- **DO NOT** create savepoints for SELECT statements — no writes = nothing to undo.
- **DO NOT** rollback the whole transaction on any error — that's PostgreSQL behavior,
  not MySQL. Only rollback to savepoint (statement-level).
- **DO NOT** persist savepoints to WAL — they are in-memory only. Crash recovery
  handles everything at transaction granularity, not statement granularity.

---

## Risks

- **`SessionContext` might not expose `autocommit`** → check if it wraps `ConnectionState`
  or is a separate struct. If separate, add the field or thread it through. Low risk.
- **Undo op replay cost for large statements** → for a batch INSERT of 10K rows that
  fails, `rollback_to_savepoint` replays ~10K undos. Acceptable for now; optimize later
  if profiling shows it's a bottleneck.
- **DDL implicit commit changes test expectations** → some existing tests may assume
  DDL in a transaction does not commit. Audit existing tests before the change.
