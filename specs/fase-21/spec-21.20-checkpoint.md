# Spec: 21.20 — SQL CHECKPOINT

Phase: 21 — Advanced SQL
Task: 21.20 CHECKPOINT
Status: draft

## Context

Phase 3 already implemented the storage/WAL checkpoint engine:
`Checkpointer::checkpoint()` flushes dirty pages, appends a durable
`Checkpoint` WAL entry, stores `checkpoint_lsn` in page 0, and crash recovery
replays from that LSN onward. The remaining gap in Phase 21 is the SQL/admin
surface: there is still no top-level statement that lets a user force that
checkpoint manually through the normal SQL path.

This task closes the SQL-visible administrative statement only. WAL rotation
already exists internally via `TxnManager::rotate_wal(...)` and remains a
separate concern.

## Goal

Implement a top-level SQL `CHECKPOINT` statement that forces a full durable
checkpoint and returns success as an empty/OK result.

## Non-goals

- WAL truncation / rotation after checkpoint.
- Background or size-based auto-checkpoint scheduling.
- New output format such as returning the checkpoint LSN as a resultset.
- Reworking fsync pipeline semantics or WAL durability policy.
- Reusing `FLUSH TABLES` as an alias for checkpoint.

## Behavior

### Public API

```rust
pub enum Stmt {
    // ...
    Checkpoint,
}

impl TxnManager {
    pub fn checkpoint(
        &self,
        storage: &dyn StorageEngine,
    ) -> Result<u64, DbError>;
}
```

### SQL surface

Accepted grammar in this subphase:

```sql
CHECKPOINT
```

### Semantics

- `CHECKPOINT` is a top-level administrative statement.
- Execution path:
  1. reject if any transaction is currently active in the process
  2. flush storage pages and record a durable checkpoint via the existing
     `Checkpointer`
  3. return `QueryResult::Empty`
- The existing five-step checkpoint ordering from Phase 3 must remain the
  source of truth; the SQL statement is only a wrapper around that engine.
- `CHECKPOINT` does **not** rotate or truncate the WAL file.
- `CHECKPOINT` counts as a mutating/admin statement for degraded-mode gating.
- `CHECKPOINT` is allowed both in autocommit mode and outside transactions,
  but it must reject while any explicit transaction is active, including a
  transaction owned by another session.

### Error cases

| Input | Expected error | Message shape |
|-------|----------------|---------------|
| `CHECKPOINT` while current session has an active transaction | `DbError::TransactionAlreadyActive` | existing transaction-already-active text |
| `CHECKPOINT` while another session has an active transaction | `DbError::TransactionAlreadyActive` | same error shape |
| I/O / fsync / meta-page failure during checkpoint | propagated existing `DbError::{Io, DiskFull, StorageFull, WalGroupCommitFailed}` | unchanged |
| `CHECKPOINT extra_tokens` | `DbError::ParseError` | points at trailing garbage |

## Edge cases

- [ ] Fresh database with `checkpoint_lsn = 0`: `CHECKPOINT` succeeds and advances it to a non-zero LSN.
- [ ] Repeated checkpoints monotonically increase `checkpoint_lsn`.
- [ ] Empty WAL still allows `CHECKPOINT` and writes the first checkpoint entry.
- [ ] `CHECKPOINT` while current explicit transaction is open is rejected.
- [ ] `CHECKPOINT` while another session's transaction is open is rejected.
- [ ] `CHECKPOINT` survives reopen: `last_checkpoint_lsn()` after reopen matches the last successful SQL checkpoint.
- [ ] Wire path returns an OK packet, not a resultset.

## On-disk format

No new on-disk format.

This statement must reuse the existing Phase 3 checkpoint format:

- WAL `EntryType::Checkpoint`
- meta page `checkpoint_lsn`

Compatibility rule: `21.20` adds only SQL/executor plumbing; it does not alter
the WAL header, meta page layout, or recovery format.

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| Manual `CHECKPOINT` on idle/small DB | within existing `Checkpointer::checkpoint()` cost | no extra full-database work beyond current checkpoint path |

## Dependencies

- Depends on: `crates/axiomdb-wal/src/checkpoint.rs`
- Depends on: `crates/axiomdb-wal/src/txn_inspect.rs` administrative hooks
- Depends on: SQL parser/executor administrative statement plumbing in `axiomdb-sql`
- Blocks: `21.23` advanced SQL tests from covering explicit checkpoint behavior

## Open questions

- [x] `CHECKPOINT` is SQL-only in this subphase; WAL rotation stays separate.
- [x] The statement returns OK/empty result, not the checkpoint LSN.
- [x] The statement rejects if **any** transaction is active, matching the
      existing safety rule already used by `rotate_wal`.

## Done criteria

- [ ] Parser accepts bare `CHECKPOINT`.
- [ ] AST/executor support a real `Stmt::Checkpoint`.
- [ ] `TxnManager` exposes a guarded checkpoint helper that rejects active transactions.
- [ ] SQL execution advances `last_checkpoint_lsn()` on success.
- [ ] `CHECKPOINT` does not rotate/truncate the WAL.
- [ ] `cargo test -p axiomdb-wal` passes.
- [ ] `cargo test -p axiomdb-sql --test integration_checkpoint` passes.
- [ ] `python3 tools/wire-test.py` covers one checkpoint smoke.
- [ ] `cargo clippy --workspace -- -D warnings` passes.

## References

- `db.md` — WAL / checkpoint architecture
- `docs/progreso.md`
- `memory/project_state.md`
- `crates/axiomdb-wal/src/checkpoint.rs`
- `crates/axiomdb-wal/src/txn_inspect.rs`
- `specs/fase-03/spec-3.6-checkpoint.md`
- `specs/fase-03/spec-3.7-wal-rotation.md`
