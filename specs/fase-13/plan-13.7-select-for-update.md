# Plan: 13.7 SELECT FOR UPDATE / FOR SHARE — Row-level pessimistic locking

Phase: 13 — Advanced PostgreSQL features
Task: Row-level locking via `SELECT … FOR UPDATE / FOR SHARE [NOWAIT]`
Spec: specs/fase-13/spec-13.7-select-for-update.md
Status: in-progress

## Summary

Four steps extend the existing `axiomdb-lock` crate and SQL layer to honour `SELECT … FOR UPDATE/SHARE [NOWAIT]`:

1. **Lock infra** — add `LockFlags::NOWAIT` and the matching fast-exit on the sync slow path.
2. **AST** — replace the stub `LockMode` with `LockStrength + LockWaitPolicy + SelectLockClause`; fix every call site.
3. **Parser** — parse all four strength keywords (`FOR KEY SHARE`, `FOR SHARE`, `FOR NO KEY UPDATE`, `FOR UPDATE`) plus `NOWAIT` and the MySQL alias `LOCK IN SHARE MODE`.
4. **Executor + tests** — thread `(RecordId, Row)` pairs through the `select_ctx.rs` pipeline (WHERE → ORDER BY → LIMIT), then acquire table-intention + row locks on the filtered+limited set before projecting; guard FDW/clustered; write 14+ integration tests; wire smoke; update docs.

Lock release is **already wired** (`exec_with_ctx.rs` lines 42 and 59 call `release_all_for_txn` on COMMIT and ROLLBACK). Nothing to add there.

## Dependencies

Must be done first:
- [x] spec-13.7-select-for-update approved
- [x] `axiomdb-lock` crate exists (Phase 40.11, complete)
- [x] `ExecutionContext.lock_mgr` wired from `SharedDatabase`

Blocks (until this plan is done):
- [ ] 13.8 deadlock-detection wire test
- [ ] 13.8b SKIP LOCKED

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_select_for_update.rs` — 14+ integration tests

Modified files:
- `crates/axiomdb-lock/src/mode.rs` — add `LockFlags::NOWAIT = 0x0020`
- `crates/axiomdb-lock/src/manager.rs` — NOWAIT fast-exit in `acquire_record_lock_sync` slow path
- `crates/axiomdb-sql/src/ast.rs` — add `LockStrength/LockWaitPolicy/SelectLockClause`; remove `LockMode`; rename `SelectStmt.lock_mode` → `lock_clause`
- `crates/axiomdb-sql/src/parser/dml.rs` — full locking clause grammar
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — carry RIDs through pipeline; acquire locks
- `docs-site/src/user-guide/features/transactions.md` — document SELECT FOR UPDATE
- `docs-site/src/sql-reference/dml.md` — locking clause syntax

---

## Step 1 — `LockFlags::NOWAIT` in axiomdb-lock

**Goal:** `NOWAIT` flag exists and the sync slow path returns `LockTimeout` immediately on conflict.

**Files:**
- `crates/axiomdb-lock/src/mode.rs`
- `crates/axiomdb-lock/src/manager.rs`

**Approach:** TDD — write a unit test that acquires a conflicting lock with `NOWAIT` and asserts `Err(DbError::LockTimeout)` is returned without sleeping.

### Test to add

```rust
// crates/axiomdb-lock/src/manager.rs — existing #[cfg(test)] block
#[test]
fn nowait_returns_lock_timeout_immediately() {
    let mgr = LockManager::new(Duration::from_secs(5));
    let page = PageId(1);
    let slot = 0u16;
    let txn_a = TxnId(1);
    let txn_b = TxnId(2);

    // txn_a holds Exclusive
    mgr.acquire_record_lock_sync(txn_a, page, slot, LockMode::Exclusive, LockFlags::REC_NOT_GAP)
        .unwrap();

    // txn_b requests Exclusive + NOWAIT → must fail instantly
    let flags = LockFlags::REC_NOT_GAP | LockFlags::NOWAIT;
    let start = std::time::Instant::now();
    let result = mgr.acquire_record_lock_sync(txn_b, page, slot, LockMode::Exclusive, flags);
    assert!(result.is_err(), "expected LockTimeout");
    assert!(start.elapsed() < Duration::from_millis(100), "must not wait");
}
```

### Implementation outline

```rust
// crates/axiomdb-lock/src/mode.rs
impl LockFlags {
    pub const NOWAIT: Self = Self(0x0020);
}

// crates/axiomdb-lock/src/manager.rs — acquire_record_lock_sync slow path
// After pushing waiter to queue:
if flags.contains(LockFlags::NOWAIT) {
    // remove waiter we just added, return immediately
    shard.waiters.retain(|w| w.txn_id != txn_id);
    return Err(DbError::LockTimeout("Lock wait timeout exceeded; try restarting transaction".into()));
}
// ... existing Condvar::wait_timeout ...
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-lock
./tools/vm.sh clippy -p axiomdb-lock -- -D warnings
```

### Commit

```
feat(fase-13): add LockFlags::NOWAIT + sync slow-path fast-exit

Step 1 of specs/fase-13/plan-13.7-select-for-update.md
```

---

## Step 2 — AST: `SelectLockClause` replaces `LockMode`

**Goal:** New AST types defined; old `LockMode` removed; all match/import/struct-literal sites compile.

**Files:**
- `crates/axiomdb-sql/src/ast.rs`

**Approach:** Add new enums, change `SelectStmt.lock_mode` to `lock_clause`, remove `LockMode`; fix the 3 struct-literal sites (`lock_mode: None` → `lock_clause: None`) and any pattern-match sites.

### Implementation outline

```rust
// crates/axiomdb-sql/src/ast.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockStrength {
    ForKeyShare,
    ForShare,
    ForNoKeyUpdate,
    ForUpdate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockWaitPolicy {
    Block,
    NoWait,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectLockClause {
    pub strength: LockStrength,
    pub wait_policy: LockWaitPolicy,
}

// SelectStmt field (line ~733):
// OLD: pub lock_mode: Option<LockMode>,
// NEW:
pub lock_clause: Option<SelectLockClause>,

// Remove: pub enum LockMode { ForUpdate, ShareMode }
```

Sites to migrate (3 struct literal `lock_mode: None` in ast.rs lines 1576, 1603, 1827) → `lock_clause: None`.

Parser import at `parser/dml.rs:10` (`use crate::ast::LockMode`) → remove. Any executor match on `lock_mode` → update.

### Verification

```bash
./tools/vm.sh build -p axiomdb-sql 2>&1 | grep -E "^error"
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-13): AST — SelectLockClause replaces LockMode (13.7)

Step 2 of specs/fase-13/plan-13.7-select-for-update.md
```

---

## Step 3 — Parser: full locking clause grammar

**Goal:** `FOR KEY SHARE`, `FOR SHARE`, `FOR NO KEY UPDATE`, `FOR UPDATE` (each with optional `NOWAIT`) and `LOCK IN SHARE MODE` all parse to the correct `SelectLockClause`.

**Files:**
- `crates/axiomdb-sql/src/parser/dml.rs`

**Approach:** Replace the current stub (lines 212-224) with a helper `parse_lock_clause()` that peeks at the next keyword sequence.

### Implementation outline

```rust
// crates/axiomdb-sql/src/parser/dml.rs

fn parse_lock_clause(p: &mut Parser) -> Option<SelectLockClause> {
    // MySQL alias
    if p.eat_keyword("LOCK") {
        p.expect_keyword("IN"); p.expect_keyword("SHARE"); p.expect_keyword("MODE");
        return Some(SelectLockClause { strength: LockStrength::ForShare, wait_policy: LockWaitPolicy::Block });
    }
    if !p.eat_keyword("FOR") { return None; }

    let strength = if p.eat_keyword("UPDATE") {
        LockStrength::ForUpdate
    } else if p.eat_keyword("SHARE") {
        LockStrength::ForShare
    } else if p.eat_keywords(&["NO", "KEY", "UPDATE"]) {
        LockStrength::ForNoKeyUpdate
    } else if p.eat_keywords(&["KEY", "SHARE"]) {
        LockStrength::ForKeyShare
    } else {
        return None; // parse error — caller handles
    };

    let wait_policy = if p.eat_keyword("NOWAIT") {
        LockWaitPolicy::NoWait
    } else {
        LockWaitPolicy::Block
    };

    Some(SelectLockClause { strength, wait_policy })
}
```

Call `parse_lock_clause` at the end of `parse_select_stmt`, after ORDER BY / LIMIT, and assign the result to `stmt.lock_clause`.

Reject `FOR UPDATE` in set-op branches (UNION/INTERSECT/EXCEPT) with `ParseError`.

### Test to add

```rust
// inline unit test in parser/dml.rs or tests/
#[test]
fn parse_for_update_nowait() {
    let ast = parse_sql("SELECT id FROM t FOR UPDATE NOWAIT").unwrap();
    let sel = ast.as_select();
    let lc = sel.lock_clause.as_ref().unwrap();
    assert_eq!(lc.strength, LockStrength::ForUpdate);
    assert_eq!(lc.wait_policy, LockWaitPolicy::NoWait);
}

#[test]
fn parse_lock_in_share_mode() {
    let ast = parse_sql("SELECT id FROM t LOCK IN SHARE MODE").unwrap();
    let lc = ast.as_select().lock_clause.as_ref().unwrap();
    assert_eq!(lc.strength, LockStrength::ForShare);
    assert_eq!(lc.wait_policy, LockWaitPolicy::Block);
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test parser 2>&1 | tail -10
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-13): parser — FOR UPDATE/SHARE/NO KEY UPDATE/KEY SHARE [NOWAIT] (13.7)

Step 3 of specs/fase-13/plan-13.7-select-for-update.md
```

---

## Step 4 — Executor: acquire row locks in `select_ctx.rs`

**Goal:** When `stmt.lock_clause` is `Some`, acquire table-intention + row-level locks for every row that survives WHERE + ORDER BY + LIMIT, before projecting. Guard FDW and clustered tables.

**Files:**
- `crates/axiomdb-sql/src/executor/select_ctx.rs`

**Approach:**

The current pipeline in `select_ctx.rs` discards `RecordId` during the WHERE loop:
```rust
for (_rid, values) in raw_rows { … rows.push(values); }
```

Change to carry `(RecordId, Row)` pairs:

1. **Scan phase** — keep `Vec<(RecordId, Vec<Value>)>` through WHERE filtering.
2. **Sort phase** — sort by `(RecordId, Vec<Value>)` pairs keyed on the value component.
3. **LIMIT phase** — take the first N `(RecordId, Vec<Value>)` pairs.
4. **Lock phase** (new) — if `lock_clause` is `Some` and we have a lock manager and an active txn:
   a. For each FROM table that is a local heap table, acquire table-level intention lock.
   b. For each `(RecordId, _)` in the filtered+limited set, acquire row lock.
5. **Project phase** — extract and return `Vec<Value>` from the locked pairs.

```rust
// select_ctx.rs — new helper
fn acquire_row_locks(
    lock_clause: &SelectLockClause,
    rids: &[(RecordId, Vec<Value>)],
    table_ids: &[TableId],
    txn_id: TxnId,
    lm: &LockManager,
) -> Result<(), DbError> {
    use axiomdb_lock::{LockMode, LockFlags};

    let (table_mode, row_mode) = match lock_clause.strength {
        LockStrength::ForKeyShare | LockStrength::ForShare =>
            (LockMode::IntentionShared, LockMode::Shared),
        LockStrength::ForNoKeyUpdate | LockStrength::ForUpdate =>
            (LockMode::IntentionExclusive, LockMode::Exclusive),
    };
    let mut row_flags = LockFlags::REC_NOT_GAP;
    if lock_clause.wait_policy == LockWaitPolicy::NoWait {
        row_flags |= LockFlags::NOWAIT;
    }

    for &tid in table_ids {
        lm.acquire_table_lock_sync(txn_id, tid, table_mode)?;
    }
    for (rid, _) in rids {
        lm.acquire_record_lock_sync(txn_id, rid.page_id, rid.slot_id, row_mode, row_flags)?;
    }
    Ok(())
}
```

Guard conditions (return `NotImplemented`):
- `TableKind::Foreign` → `"FOR UPDATE is not supported on foreign tables"`
- `TableKind::Clustered` → `"FOR UPDATE on clustered tables not yet supported"`

txn_id: obtained from `conn_txn.map(|c| c.txn_id)`. If `None`, skip locking entirely.

### Test sketch (integration tests in Step 5)

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql 2>&1 | tail -10
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-13): executor — acquire IX/IS + X/S row locks post-LIMIT (13.7)

Step 4 of specs/fase-13/plan-13.7-select-for-update.md
```

---

## Step 5 — Integration tests, wire smoke, docs, close

**Goal:** 14+ integration tests pass; wire smoke works; docs updated; subphase closed.

**Files:**
- `crates/axiomdb-sql/tests/integration_select_for_update.rs` (new)
- `docs-site/src/user-guide/features/transactions.md`
- `docs-site/src/sql-reference/dml.md`
- `tools/wire-test.py` (append FOR UPDATE scenario)
- `docs/progreso.md`
- `memory/project_state.md`

### Tests to write

```rust
// Basic FOR UPDATE returns rows
#[test] fn test_for_update_returns_rows() { … }

// Blocking: txn A locks, txn B waits (thread-based, short timeout)
#[test] fn test_for_update_blocks_concurrent_writer() { … }

// NOWAIT: txn B fails immediately
#[test] fn test_for_update_nowait_fails_immediately() { … }

// FOR SHARE + FOR SHARE compatible (both granted)
#[test] fn test_for_share_compatible_with_for_share() { … }

// FOR SHARE blocked by FOR UPDATE
#[test] fn test_for_share_blocked_by_for_update() { … }

// ROLLBACK releases locks, blocked txn proceeds
#[test] fn test_rollback_releases_locks() { … }

// Deadlock auto-detected
#[test] fn test_deadlock_detected() { … }

// FOR UPDATE + LIMIT locks only returned rows
#[test] fn test_for_update_with_limit() { … }

// Autocommit: no error, rows returned
#[test] fn test_for_update_autocommit_no_error() { … }

// FDW table → NotImplemented
#[test] fn test_for_update_on_fdw_table_not_implemented() { … }

// FOR KEY SHARE maps to Shared row lock
#[test] fn test_for_key_share_shared_lock() { … }

// FOR NO KEY UPDATE maps to Exclusive row lock
#[test] fn test_for_no_key_update_exclusive_lock() { … }

// FOR UPDATE on empty table: no rows, returns immediately
#[test] fn test_for_update_empty_table() { … }

// Lock upgrade: S then X on same row
#[test] fn test_lock_upgrade_s_to_x() { … }
```

### Closing gate

```bash
./tools/vm.sh test --workspace 2>&1 | tail -5
./tools/vm.sh clippy --workspace -- -D warnings 2>&1 | head -5
cargo fmt --check

# Wire smoke (rebuild server first)
pkill axiomdb-server || true
./tools/vm.sh build -p axiomdb-server --release
python3 tools/wire-test.py
```

### Commit

```
feat(fase-13): complete SELECT FOR UPDATE / FOR SHARE (13.7)

Implements specs/fase-13/spec-13.7-select-for-update.md
- LockFlags::NOWAIT in axiomdb-lock
- LockStrength/LockWaitPolicy/SelectLockClause in AST
- Parser: FOR UPDATE/SHARE/NO KEY UPDATE/KEY SHARE [NOWAIT] + LOCK IN SHARE MODE
- Executor: IX/IS table + X/S row locks acquired post-filter, post-LIMIT
- 14 integration tests
- Docs: transactions.md, dml.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `select_ctx.rs` pipeline refactor breaks existing SELECT tests | medium | Run `-p axiomdb-sql` after Step 4 before proceeding |
| Deadlock test is timing-sensitive on VM | medium | Use explicit `Arc<Barrier>` + `thread::spawn`; assert on error kind, not wall time |
| `acquire_table_lock_sync` signature differs from expected | low | Check `manager.rs` signature before Step 4 coding |
| `parse_lock_clause` conflicts with set-op subqueries | low | Guard at the call site: only call on top-level SELECT, not set-op branches |

## Rollback plan

If the plan is abandoned mid-way:

1. `git reset --hard <commit before Step 1>` — or
2. Leave partial work on branch `abandoned/plan-13.7-select-for-update-<date>`
3. Revert spec status to `draft` with a note

## Estimated effort

| Step | Estimate |
|------|----------|
| Step 1 — NOWAIT flag | 15 min |
| Step 2 — AST refactor | 20 min |
| Step 3 — Parser | 25 min |
| Step 4 — Executor | 45 min |
| Step 5 — Tests + docs + close | 60 min |
| **Total** | **~2.5 hours** |
