# Plan: 13.8b — SKIP LOCKED

Phase: 13 — Advanced PostgreSQL
Task: SKIP LOCKED pessimistic-locking wait policy
Spec: specs/fase-13/spec-13.8b-skip-locked.md
Status: done

## Summary

Three steps in dependency order. Step 1 adds `try_acquire_record_lock_sync`
to `axiomdb-lock` (non-blocking try-acquire that returns Ok(bool) instead of
blocking or erroring on conflict). Step 2 adds `LockWaitPolicy::SkipLocked`
to the AST and wires the parser to recognise `SKIP LOCKED` after any `FOR …`
strength keyword. Step 3 redesigns the executor's lock pipeline so LIMIT is
applied *after* skip-lock filtering, then adds integration tests and docs.
All steps follow TDD order.

## Dependencies

Must be done first:
- [x] spec-13.7-select-for-update approved and implemented

Blocks (until this plan is done):
- [ ] Phase 13 closure (13.8b is the last subphase)

## Affected files

Modified files:
- `crates/axiomdb-lock/src/manager.rs` — add `try_acquire_record_lock_sync`
- `crates/axiomdb-lock/src/lib.rs` — re-export new method (already public via impl)
- `crates/axiomdb-sql/src/ast.rs` — `LockWaitPolicy::SkipLocked` variant
- `crates/axiomdb-sql/src/parser/dml.rs` — `parse_lock_clause` SKIP LOCKED branch
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — redesigned lock pipeline
- `docs-site/src/user-guide/sql-reference/dml.md` — FOR UPDATE table updated
- `docs-site/src/user-guide/features/transactions.md` — remove SKIP LOCKED limitation

New files:
- `crates/axiomdb-sql/tests/integration_skip_locked.rs` — integration tests

---

## Step 1 — `try_acquire_record_lock_sync` in axiomdb-lock

**Goal:** Non-blocking lock attempt — returns `Ok(true)` if granted, `Ok(false)` if conflicting lock held by another txn; never blocks.
**Files:** `crates/axiomdb-lock/src/manager.rs`

### Tests to add

```rust
// crates/axiomdb-lock/src/manager.rs (in #[cfg(test)] mod tests)

#[test]
fn try_acquire_returns_true_when_no_conflict() {
    let lm = LockManager::new();
    let result = lm.try_acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive);
    assert_eq!(result, Ok(true));
}

#[test]
fn try_acquire_returns_false_when_conflicting_lock_held() {
    let lm = LockManager::new();
    // txn 1 holds exclusive lock
    lm.acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive, LockFlags::NONE).unwrap();
    // txn 2 tries shared — conflicts, should return false (not block, not error)
    let result = lm.try_acquire_record_lock_sync(2, 100, 0, LockMode::Shared);
    assert_eq!(result, Ok(false));
}

#[test]
fn try_acquire_returns_true_for_compatible_shared_locks() {
    let lm = LockManager::new();
    lm.acquire_record_lock_sync(1, 100, 0, LockMode::Shared, LockFlags::NONE).unwrap();
    // txn 2 also wants shared — compatible, no conflict
    let result = lm.try_acquire_record_lock_sync(2, 100, 0, LockMode::Shared);
    assert_eq!(result, Ok(true));
}

#[test]
fn try_acquire_returns_true_when_same_txn_already_holds() {
    let lm = LockManager::new();
    lm.acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive, LockFlags::NONE).unwrap();
    // same txn tries again — idempotent, granted
    let result = lm.try_acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive);
    assert_eq!(result, Ok(true));
}

#[test]
fn try_acquire_does_not_enqueue_waiter() {
    let lm = LockManager::new();
    lm.acquire_record_lock_sync(1, 100, 0, LockMode::Exclusive, LockFlags::NONE).unwrap();
    // try from txn 2 — conflict, skip
    let _ = lm.try_acquire_record_lock_sync(2, 100, 0, LockMode::Exclusive);
    // txn 1 releases; txn 2 should NOT be notified (it was never enqueued)
    lm.release_all_for_txn(1);
    // verify: lock queue should be empty (no waiters from txn 2)
    let result = lm.try_acquire_record_lock_sync(3, 100, 0, LockMode::Exclusive);
    assert_eq!(result, Ok(true)); // txn 3 gets it clean
}
```

### Implementation outline

```rust
// In crates/axiomdb-lock/src/manager.rs, after acquire_record_lock_sync:

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

    // Fast path: same txn already holds compatible lock on this page.
    if let Some(idx) = queue.find_granted_idx(txn_id, mode) {
        if let Some(ref mut bm) = queue.granted[idx].bitmap {
            bm.set(slot_id);
        }
        return Ok(true);
    }

    // Check conflict with granted locks.
    let blocking_txn = queue.find_conflict(txn_id, mode, Some(slot_id));

    // FIFO: also blocked if there are waiters (to preserve ordering).
    if blocking_txn.is_none() && queue.waiting.is_empty() {
        // Grant immediately.
        let mut entry = LockEntry {
            txn_id,
            mode,
            flags: LockFlags::NONE,
            requested_at: Instant::now(),
            bitmap: Some(SlotBitmap::with_slot(slot_id)),
        };
        entry.flags = entry.flags.difference(LockFlags::WAITING);
        queue.granted.push(entry);
        return Ok(true);
    }

    // Conflict or waiters ahead — skip, do NOT enqueue.
    if queue.is_empty() {
        shard.remove(&page_id);
    }
    Ok(false)
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-lock
./tools/vm.sh clippy -p axiomdb-lock -- -D warnings
```

### Commit

```
feat(fase-13): add LockManager::try_acquire_record_lock_sync (13.8b step 1)

Non-blocking try-acquire: Ok(true) if granted, Ok(false) if conflicting
lock held by another txn. Never enqueues a waiter, never blocks.
5 unit tests.
```

---

## Step 2 — AST `SkipLocked` + Parser `SKIP LOCKED`

**Goal:** Add `LockWaitPolicy::SkipLocked` to AST and parse `FOR UPDATE SKIP LOCKED` (all four strength variants).
**Files:** `crates/axiomdb-sql/src/ast.rs`, `crates/axiomdb-sql/src/parser/dml.rs`

### Tests to add

Added inline to the existing parser integration test suite. Key cases:

```rust
// in crates/axiomdb-sql/tests/integration_ddl_parser.rs or a new parse test

#[test]
fn parse_for_update_skip_locked() {
    let stmt = parse_one("SELECT id FROM t FOR UPDATE SKIP LOCKED");
    let sel = unwrap_select(stmt);
    let lc = sel.lock_clause.unwrap();
    assert_eq!(lc.strength, LockStrength::ForUpdate);
    assert_eq!(lc.wait_policy, LockWaitPolicy::SkipLocked);
}

#[test]
fn parse_for_share_skip_locked() {
    let stmt = parse_one("SELECT id FROM t FOR SHARE SKIP LOCKED");
    let sel = unwrap_select(stmt);
    assert_eq!(sel.lock_clause.unwrap().wait_policy, LockWaitPolicy::SkipLocked);
}

#[test]
fn parse_for_no_key_update_skip_locked() {
    let stmt = parse_one("SELECT id FROM t FOR NO KEY UPDATE SKIP LOCKED");
    let sel = unwrap_select(stmt);
    assert_eq!(sel.lock_clause.unwrap().wait_policy, LockWaitPolicy::SkipLocked);
}

#[test]
fn parse_for_key_share_skip_locked() {
    let stmt = parse_one("SELECT id FROM t FOR KEY SHARE SKIP LOCKED");
    let sel = unwrap_select(stmt);
    assert_eq!(sel.lock_clause.unwrap().wait_policy, LockWaitPolicy::SkipLocked);
}

#[test]
fn parse_nowait_still_works() {
    let stmt = parse_one("SELECT id FROM t FOR UPDATE NOWAIT");
    let lc = unwrap_select(stmt).lock_clause.unwrap();
    assert_eq!(lc.wait_policy, LockWaitPolicy::NoWait);
}
```

### Implementation outline

```rust
// ast.rs
pub enum LockWaitPolicy {
    Block,
    NoWait,
    SkipLocked,  // add this variant
}

// parser/dml.rs — in parse_lock_clause, replace:
//   let wait_policy = if eat_ident_ci(p, "NOWAIT") { LockWaitPolicy::NoWait }
//                     else { LockWaitPolicy::Block };
// with:
let wait_policy = if eat_ident_ci(p, "NOWAIT") {
    LockWaitPolicy::NoWait
} else if eat_ident_ci(p, "SKIP") {
    // SKIP LOCKED — consume LOCKED identifier
    if !eat_ident_ci(p, "LOCKED") {
        let pos = p.current_pos();
        return Err(DbError::ParseError {
            message: "expected LOCKED after SKIP".into(),
            position: Some(pos),
        });
    }
    LockWaitPolicy::SkipLocked
} else {
    LockWaitPolicy::Block
};
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_ddl_parser
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(fase-13): AST LockWaitPolicy::SkipLocked + parser SKIP LOCKED (13.8b step 2)

Adds SkipLocked variant to LockWaitPolicy enum; parse_lock_clause now
recognises FOR UPDATE/SHARE/NO KEY UPDATE/KEY SHARE SKIP LOCKED.
NOWAIT still works unchanged. 5 parser tests.
```

---

## Step 3 — Executor pipeline + integration tests + docs

**Goal:** Redesign the FOR UPDATE block in `select_ctx.rs` for SkipLocked; apply LIMIT after skip-lock filtering; add integration tests and update docs.
**Files:** `crates/axiomdb-sql/src/executor/select_ctx.rs`, `crates/axiomdb-sql/tests/integration_skip_locked.rs`, `docs-site/…`

### Integration tests to add

```rust
// crates/axiomdb-sql/tests/integration_skip_locked.rs

// T1: basic — returns rows that can be locked
fn test_skip_locked_returns_unlocked_rows() { ... }

// T2: all rows locked → 0 rows, no error
fn test_skip_locked_all_locked_returns_empty() { ... }

// T3: LIMIT 1 SKIP LOCKED — first row locked, returns second
fn test_skip_locked_limit_skips_first_locked_row() { ... }

// T4: no rows match WHERE → 0 rows (no crash)
fn test_skip_locked_empty_table() { ... }

// T5: same txn already holds lock → row is included (idempotent)
fn test_skip_locked_same_txn_holds_lock_includes_row() { ... }

// T6: FOR SHARE SKIP LOCKED — compatible with another FOR SHARE
fn test_skip_locked_for_share_compatible() { ... }

// T7: autocommit mode → all rows returned, no error
fn test_skip_locked_autocommit_returns_all() { ... }

// T8: OFFSET + SKIP LOCKED — offset applied after filtering
fn test_skip_locked_offset_applied_after_filtering() { ... }

// T9: ORDER BY + SKIP LOCKED — deterministic row selection
fn test_skip_locked_order_by_deterministic() { ... }

// T10: SKIP LOCKED on clustered table → NotImplemented
fn test_skip_locked_clustered_table_not_implemented() { ... }
```

### Executor pipeline change outline

```rust
// In select_ctx.rs, inside the `if let Some(ref lc) = stmt.lock_clause` block:

if lc.wait_policy == LockWaitPolicy::SkipLocked {
    // SkipLocked pipeline: apply LIMIT *after* filtering out locked rows.
    if let (Some(lm), Some(ct)) = (exec_ctx.lock_manager(), conn_txn) {
        let (table_mode, row_mode) = /* same mapping as before */;

        // Acquire table intention lock (before iterating rows).
        lm.acquire_table_lock_sync(ct.txn_id, resolved.def.id, table_mode)?;

        // ORDER BY (no LIMIT yet).
        // ... sort rid_pairs ...

        // Try-lock loop — skip rows we can't lock.
        let mut locked_pairs: Vec<(RecordId, Row)> = Vec::new();
        for (rid, row) in rid_pairs {
            match lm.try_acquire_record_lock_sync(
                ct.txn_id, rid.page_id, rid.slot_id, row_mode,
            )? {
                true  => locked_pairs.push((rid, row)),
                false => { /* skip */ }
            }
        }

        // Apply LIMIT/OFFSET on the filtered set.
        let (limit_n, offset_n) = eval_limit_offset_usize(&stmt.limit, &stmt.offset)?;
        if offset_n > 0 {
            let skip = offset_n.min(locked_pairs.len());
            locked_pairs = locked_pairs[skip..].to_vec();
        }
        if let Some(n) = limit_n {
            locked_pairs.truncate(n);
        }

        // Project and early return.
        // ... same projection path as today ...
        return Ok(QueryResult::Rows { ... });
    }
    // No lock_manager or no conn_txn: fall through to normal pipeline.
}

// Block / NoWait: existing pipeline unchanged.
```

### Docs changes

In `docs-site/src/user-guide/sql-reference/dml.md` — FOR UPDATE table:

| Clause | Wait policy | Conflict behaviour |
|---|---|---|
| `FOR UPDATE` | Block | Wait up to `lock_timeout` |
| `FOR UPDATE NOWAIT` | NoWait | Fail immediately (1205) |
| `FOR UPDATE SKIP LOCKED` | SkipLocked | Omit locked rows silently |

In `docs-site/src/user-guide/features/transactions.md` — remove the
"SKIP LOCKED is planned" limitation note from the Row-Level Locking section;
replace with a usage example.

### Verification against spec

- [x] `try_acquire_record_lock_sync` implemented (Step 1)
- [x] `LockWaitPolicy::SkipLocked` in AST + parser (Step 2)
- [ ] LIMIT applied after skip-lock filtering
- [ ] `LIMIT 1 SKIP LOCKED` with pre-locked row → second row returned
- [ ] All rows locked → 0 rows, no error
- [ ] `FOR SHARE SKIP LOCKED` — compatible locks both visible
- [ ] autocommit → all rows returned
- [ ] OFFSET + SKIP LOCKED applied after filtering
- [ ] clustered table → `NotImplemented`
- [ ] `cargo nextest run -p axiomdb-sql` passes
- [ ] `cargo clippy --workspace -- -D warnings` clean
- [ ] Wire test updated + 528+ assertions

### Commit

```
feat(fase-13): complete SKIP LOCKED — executor pipeline + tests + docs (13.8b)

Implements specs/fase-13/spec-13.8b-skip-locked.md
- SkipLocked pipeline: ORDER BY → try-lock loop → LIMIT → project
- LIMIT applied after skip-filtering (correct job-queue semantics)
- 10 integration tests; wire test updated
- docs: FOR UPDATE table, SKIP LOCKED limitation removed
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `try_acquire` with FIFO waiters: should block if waiters ahead | medium | Spec says skip if `!queue.waiting.is_empty()` — same as NOWAIT, consistent with PostgreSQL |
| Two-session wire test requires threading | low | Use NOWAIT + pre-lock from same session (different txn_ids via two `SessionContext`) as in 13.7 |

## Rollback plan

1. `git reset --hard <commit before Step 1>`, or
2. Branch `abandoned/plan-13.8b-skip-locked-<date>`, spec back to `draft`.

## Estimated effort

Total: ~2 hours
- Step 1: 30 min
- Step 2: 20 min
- Step 3: 70 min (pipeline + 10 tests + docs)
