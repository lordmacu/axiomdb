# Plan: MVCC Visibility Rules (Phase 7.1)

## Files to create/modify

- `crates/axiomdb-core/src/traits.rs` — add `IsolationLevel` enum next to `TransactionSnapshot`
- `crates/axiomdb-wal/src/txn.rs` — add `isolation_level` to `ActiveTxn`; `begin_with_isolation()`; change `active_snapshot()` to return fresh or frozen snapshot based on level
- `crates/axiomdb-sql/src/session.rs` — add `transaction_isolation` (session default) and `next_txn_isolation` (per-txn override) to `SessionContext`; helper `effective_isolation()` that consumes override
- `crates/axiomdb-sql/src/executor/mod.rs` — thread isolation level through `BEGIN` dispatch; handle `SET TRANSACTION ISOLATION LEVEL` and `SET SESSION transaction_isolation`
- `crates/axiomdb-sql/src/ast.rs` — extend `SetStmt` or add new variant for `SET TRANSACTION ISOLATION LEVEL` if parser needs it
- `crates/axiomdb-sql/src/parser/mod.rs` — parse `SET TRANSACTION ISOLATION LEVEL ...`
- `crates/axiomdb-network/src/mysql/session.rs` — expose `@@transaction_isolation` / `@@tx_isolation` in `get_variable()`
- `crates/axiomdb-network/src/mysql/database.rs` — pass isolation level from session to `BEGIN`
- `crates/axiomdb-sql/tests/integration_namespacing.rs` or new `integration_isolation.rs` — formal isolation level tests
- `tools/wire-test.py` — wire smoke for SET/SELECT @@transaction_isolation

## Algorithm / Data structure

IsolationLevel enum:

```text
enum IsolationLevel {
    ReadCommitted,
    RepeatableRead,
    Serializable,   // stored as-is, snapshot policy = RR
}
```

Session state:

```text
SessionContext {
    transaction_isolation: IsolationLevel,      // session default (RR)
    next_txn_isolation: Option<IsolationLevel>,  // per-txn override, consumed by BEGIN
}
```

ActiveTxn extension:

```text
ActiveTxn {
    txn_id: TxnId,
    snapshot_id_at_begin: u64,      // frozen snapshot (always captured)
    isolation_level: IsolationLevel, // RC or RR/Serializable
    undo_ops: Vec<UndoOp>,
    ...
}
```

Snapshot policy (the core change):

```text
TxnManager::active_snapshot():
    match active.isolation_level:
        ReadCommitted:
            // Fresh snapshot per statement — see all committed now + own writes
            TransactionSnapshot {
                snapshot_id: self.max_committed + 1,
                current_txn_id: active.txn_id,
            }
        RepeatableRead | Serializable:
            // Frozen snapshot from BEGIN — existing behavior
            TransactionSnapshot {
                snapshot_id: active.snapshot_id_at_begin,
                current_txn_id: active.txn_id,
            }
```

BEGIN flow:

```text
execute BEGIN:
    level = ctx.next_txn_isolation.take()
                .unwrap_or(ctx.transaction_isolation)
    txn.begin_with_isolation(level)
```

SET dispatch:

```text
SET SESSION transaction_isolation = 'READ-COMMITTED':
    ctx.transaction_isolation = ReadCommitted

SET TRANSACTION ISOLATION LEVEL READ COMMITTED:
    if txn.is_active(): error
    ctx.next_txn_isolation = Some(ReadCommitted)

SELECT @@transaction_isolation:
    return ctx.transaction_isolation.to_mysql_string()
```

Autocommit policy (unchanged):

```text
autocommit statement:
    txn.begin()                    // implicit
    snap = txn.snapshot()          // TransactionSnapshot::committed(max_committed)
    execute statement with snap
    txn.commit()                   // implicit
    // Always uses fresh snapshot — isolation level irrelevant
```

## Implementation phases

1. Add `IsolationLevel` enum to `axiomdb-core` and `isolation_level` field to `ActiveTxn`.
   Add `begin_with_isolation()` to `TxnManager`. Change `active_snapshot()` to
   branch on isolation level. Existing `begin()` calls `begin_with_isolation(RepeatableRead)`
   for backward compat.

2. Add `transaction_isolation` + `next_txn_isolation` to `SessionContext`.
   Add `effective_isolation()` that consumes `next_txn_isolation`.
   Wire into executor's BEGIN dispatch.

3. Parse `SET TRANSACTION ISOLATION LEVEL ...` and `SET SESSION transaction_isolation`.
   Handle in executor's SET handler. Add `@@transaction_isolation` to wire session variables.

4. Write formal isolation tests in a dedicated `integration_isolation.rs`:
   - RC: two reads see different data after concurrent commit
   - RR: two reads see same data despite concurrent commit
   - No dirty reads in either level
   - Autocommit always RC
   - Per-txn override consumed after one txn

5. Wire smoke in `wire-test.py` for SET/SELECT @@transaction_isolation.

## Tests to write

- unit: `IsolationLevel` parse/display roundtrip
- unit: `active_snapshot()` returns fresh snapshot for RC, frozen for RR
- integration: RC inside explicit txn sees committed changes between statements
- integration: RR inside explicit txn does not see committed changes between statements
- integration: no dirty reads (uncommitted data invisible in both levels)
- integration: autocommit always uses fresh snapshot regardless of session isolation
- integration: `SET TRANSACTION ISOLATION LEVEL` inside active txn returns error
- integration: per-txn override consumed after one txn, next txn uses session default
- integration: `READ UNCOMMITTED` silently upgraded to RC
- integration: `SERIALIZABLE` accepted, uses RR snapshot policy
- wire: `SELECT @@transaction_isolation` returns correct value
- wire: `SET SESSION transaction_isolation` persists across statements

## Anti-patterns to avoid

- Do NOT change `RowHeader::is_visible()` — the visibility algorithm is correct for all levels.
- Do NOT add active transaction ID arrays — AxiomDB's O(1) timestamp model is superior for single-writer.
- Do NOT create a new `TransactionSnapshot` variant — the existing struct already has both fields needed.
- Do NOT skip the `begin_with_isolation()` backward-compat wrapper — existing callers must not break.
- Do NOT let `SET TRANSACTION` work inside an active txn — MySQL rejects this and it's the right behavior.

## Risks

- Snapshot refresh for RC could expose timing bugs if `max_committed` advances between snapshot creation and heap read → mitigated by single-writer: `max_committed` only changes when the Mutex holder commits, and the Mutex holder is the one reading.
- `next_txn_isolation` could leak across sessions if not reset on `COM_RESET_CONNECTION` → mitigate by clearing it in the reset handler.
- Tests for non-repeatable reads require two "connections" sharing storage — mitigate by using a single `TxnManager` with explicit `begin`/`commit` cycles simulating two sessions.
