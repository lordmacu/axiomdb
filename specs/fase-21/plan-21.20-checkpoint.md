# Plan: 21.20 — SQL CHECKPOINT

Phase: 21 — Advanced SQL
Task: 21.20 CHECKPOINT
Spec: specs/fase-21/spec-21.20-checkpoint.md
Status: draft

## Summary

Implement `21.20` as a small administrative SQL feature layered on top of the
existing Phase 3 checkpoint engine. First add the statement shape in the SQL
AST/parser. Then expose a guarded `TxnManager::checkpoint(...)` API that reuses
`Checkpointer::checkpoint(...)` but refuses to run while any transaction is
active. After that wire the executor to run the checkpoint and return OK.
Finish with targeted SQL/WAL/wire tests and subphase closeout docs.

## Dependencies

Must be done first:
- [ ] `specs/fase-21/spec-21.20-checkpoint.md` approved.
- [ ] Existing checkpoint/WAL rotation behavior remains green in `axiomdb-wal`.

Blocks:
- [ ] `21.23` advanced SQL tests for explicit checkpoint coverage.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_checkpoint.rs` — SQL-level checkpoint behavior.
- `specs/fase-21/spec-21.20-checkpoint.md`
- `specs/fase-21/plan-21.20-checkpoint.md`

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — add `Stmt::Checkpoint`.
- `crates/axiomdb-sql/src/parser/mod.rs` — parse top-level `CHECKPOINT`.
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — dispatch checkpoint statement.
- `crates/axiomdb-sql/src/executor/exec_explain.rs` — `CHECKPOINT` explain/admin handling.
- `crates/axiomdb-sql/src/plan_deps.rs` — no-dependency admin statement arm.
- `crates/axiomdb-network/src/mysql/shared_db.rs` — classify `CHECKPOINT` as mutating/admin SQL if needed.
- `crates/axiomdb-wal/src/txn_inspect.rs` — add guarded `TxnManager::checkpoint(storage)`.
- `crates/axiomdb-wal/src/lib.rs` or re-export sites if needed.
- `tools/wire-test.py` — 21.20 checkpoint smoke.
- `docs/progreso.md`, `memory/project_state.md`, `docs/fase-21.md`,
  `memory/architecture.md`, `memory/lessons.md` — subphase closeout.

## Step 1 — Grammar and statement plumbing

**Goal:** parse and represent `CHECKPOINT` as a real statement.
**Files:** `ast.rs`, `parser/mod.rs`, parser tests.
**Approach:** TDD — parser coverage first, then the minimal AST additions.

### Tests to add

```rust
#[test]
fn parse_checkpoint_statement() { ... }

#[test]
fn parse_checkpoint_trailing_garbage_errors() { ... }
```

### Implementation outline

- Add `Stmt::Checkpoint`.
- Extend `parse_stmt()` to accept `CHECKPOINT` as a top-level statement.
- Keep it as a dedicated admin statement, not `Stmt::Noop`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — Guarded checkpoint API in WAL

**Goal:** expose a safe admin hook that reuses the existing checkpoint engine.
**Files:** `txn_inspect.rs`, `checkpoint.rs` tests if needed.
**Approach:** add one small `TxnManager` helper mirroring the safety gate used
by `rotate_wal(...)`.

### Tests to add

```rust
#[test]
fn checkpoint_rejects_active_transaction() { ... }

#[test]
fn checkpoint_advances_last_checkpoint_lsn_without_rotation() { ... }
```

### Implementation outline

- Add `TxnManager::checkpoint(&self, storage) -> Result<u64, DbError>`.
- Reuse the `active_set` guard already used by `rotate_wal(...)`.
- Delegate the actual work to `Checkpointer::checkpoint(storage, &self.wal)`.
- Keep WAL rotation/truncation out of this path entirely.

### Verification

```bash
cargo test -p axiomdb-wal checkpoint
```

## Step 3 — Executor/admin wiring

**Goal:** make `CHECKPOINT` executable through SQL.
**Files:** `exec_dispatch.rs`, `exec_explain.rs`, `plan_deps.rs`, possibly `shared_db.rs`.
**Approach:** treat it like `VACUUM`/`ANALYZE`: a top-level admin statement
that returns `QueryResult::Empty`.

### Tests to add

```rust
#[test]
fn checkpoint_statement_updates_checkpoint_lsn() { ... }

#[test]
fn checkpoint_statement_rejects_open_transaction() { ... }

#[test]
fn checkpoint_statement_rejects_other_session_active_transaction() { ... }
```

### Implementation outline

- Add a dispatch arm that calls `txn.checkpoint(storage)?`.
- Return `QueryResult::Empty`.
- Mark `CHECKPOINT` as dependency-free in `plan_deps`.
- Ensure read-only/mutating classification in `SharedDatabase` is correct so
  degraded-mode gates and read-only fast paths do not misroute it.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_checkpoint
```

## Step 4 — Wire smoke and closeout

**Goal:** verify the statement through the MySQL wire path and close the subphase.
**Files:** `tools/wire-test.py`, docs/memory closeout files.

### Tests to add

```python
cur.execute("CHECKPOINT")
cur.execute("CREATE TABLE ckpt_probe (id INT PRIMARY KEY)")
cur.execute("CHECKPOINT")
# assert success path only; storage-level assertions stay in Rust tests
```

### Verification

```bash
cargo fmt --check
cargo test -p axiomdb-wal
cargo test -p axiomdb-sql --test integration_checkpoint
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

## Risk register

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| Checkpoint races with active transactions and records an inconsistent boundary | medium | Reuse the `active_set` safety gate already enforced by WAL rotation |
| SQL path accidentally rotates WAL and widens scope | low | Keep `TxnManager::checkpoint(...)` separate from `rotate_wal(...)` |
| `CHECKPOINT` is misclassified as read-only and bypasses admin gates | medium | Update parsed-statement and raw-SQL mutation classification explicitly |
| Wire smoke gives false confidence without asserting durable state | low | Keep durability/`checkpoint_lsn` assertions in Rust integration tests, use wire only for protocol surface |

## Estimated effort

Total: medium-high

- Step 1: 30-45 min
- Step 2: 45-60 min
- Step 3: 45-75 min
- Step 4: 30-45 min
