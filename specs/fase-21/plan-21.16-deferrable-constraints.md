# Plan: 21.16 — DEFERRABLE constraints

Phase: 21 — Advanced SQL
Task: 21.16 DEFERRABLE constraints
Spec: specs/fase-21/spec-21.16-deferrable-constraints.md
Status: done

## Summary

Implement `21.16` as a bounded foreign-key deferral MVP.

The work splits cleanly into:

1. parser/AST support for FK deferrability clauses
2. catalog persistence on `FkDef`
3. per-transaction deferred-FK tracking in `ConnectionTxn`
4. commit-time validation + rollback-on-failure
5. parser/SQL/wire regression coverage and closeout

This deliberately excludes deferred CHECK/exclusion and `SET CONSTRAINTS`.

## Dependencies

Must be done first:
- [ ] `specs/fase-21/spec-21.16-deferrable-constraints.md` approved.
- [ ] Existing FK suites remain green before refactor.

Blocks:
- [ ] Future `SET CONSTRAINTS` work can build on the persisted FK metadata and
      deferred queue shape from this subphase.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_deferrable_fk.rs`
- `specs/fase-21/spec-21.16-deferrable-constraints.md`
- `specs/fase-21/plan-21.16-deferrable-constraints.md`

Modified files:
- `crates/axiomdb-sql/src/lexer.rs` — add `DEFERRABLE`, `INITIALLY`,
  `DEFERRED`, `IMMEDIATE` tokens if missing.
- `crates/axiomdb-sql/src/ast.rs` — FK deferrability metadata.
- `crates/axiomdb-sql/src/parser/ddl.rs` — FK clause parsing.
- `crates/axiomdb-sql/tests/integration_ddl_parser.rs` — parser coverage.
- `crates/axiomdb-catalog/src/schema_constraints.rs` — `FkDef` persistence /
  backward-compatible decode.
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs` and
  `crates/axiomdb-sql/src/executor/ddl_alter_constraint.rs` — persist FK flags.
- `crates/axiomdb-wal/src/txn.rs` and savepoint helpers — carry deferred-FK
  tracking in `ConnectionTxn`.
- `crates/axiomdb-sql/src/fk_enforcement.rs` — immediate-vs-deferred branching
  and commit-time validation helpers.
- `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` /
  `crates/axiomdb-sql/src/executor/exec_entry.rs` — validate before commit and
  rollback on failure.
- DML paths that currently call FK enforcement directly:
  `insert_heap_ctx.rs`, `insert_clustered_ctx.rs`, `update_candidates.rs`,
  `update_clustered.rs`, `delete.rs`, `dml_join.rs`, `merge.rs`,
  `odku_helpers.rs`, `on_conflict_helpers.rs`, `replace_helpers.rs`.
- `tools/wire-test.py` — 21.16 smoke.
- `docs/progreso.md`, `memory/project_state.md`, `docs/fase-21.md`,
  `memory/architecture.md`, `memory/lessons.md` — closeout.

## Step 1 — Parser and AST support

**Goal:** parse FK deferrability declaratively without changing runtime yet.
**Files:** `lexer.rs`, `ast.rs`, `parser/ddl.rs`, parser tests.
**Approach:** add a small normalized FK-deferrability model and default omitted
clauses to non-deferrable immediate behavior.

### Tests to add

```rust
#[test]
fn parse_table_fk_deferrable_initially_deferred() { ... }

#[test]
fn parse_column_references_not_deferrable() { ... }

#[test]
fn reject_not_deferrable_initially_deferred() { ... }
```

### Implementation outline

- Add constraint-timing / deferrability enums or fields to FK AST nodes.
- Extend FK grammar after `ON DELETE` / `ON UPDATE` handling.
- Normalize clause order and defaults.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — Catalog persistence

**Goal:** preserve FK deferrability in the catalog with backward compatibility.
**Files:** `schema_constraints.rs`, FK create/alter executor paths, FK reader tests.
**Approach:** append a compact trailer or extension to `FkDef` so old rows still
decode cleanly as non-deferrable.

### Tests to add

```rust
#[test]
fn fkdef_roundtrip_deferrable_flags() { ... }

#[test]
fn legacy_fkdef_decodes_as_not_deferrable() { ... }
```

### Implementation outline

- Extend `FkDef` with two persisted booleans.
- Keep old fixed layout readable.
- Thread flags through CREATE/ALTER FK persistence.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_fk
```

## Step 3 — Per-transaction deferred-FK tracking

**Goal:** capture deferred FK work without yet changing commit behavior.
**Files:** `txn.rs`, savepoint helpers, `fk_enforcement.rs`, DML call sites.
**Approach:** store deferred-FK pending checks inside `ConnectionTxn` so
savepoints can snapshot/truncate them naturally.

### Tests to add

```rust
#[test]
fn deferred_fk_queue_truncates_on_savepoint_rollback() { ... }

#[test]
fn immediate_fk_paths_still_error_inline() { ... }
```

### Implementation outline

- Add deferred-FK queues to `ConnectionTxn`.
- Extend savepoint snapshot/truncation metadata if needed.
- At FK enforcement call sites:
  - non-deferrable or initially-immediate → current behavior
  - initially-deferred → enqueue validation work instead of erroring now

### Verification

```bash
cargo test -p axiomdb-sql --test integration_deferrable_fk
```

## Step 4 — Commit-time validation and rollback-on-failure

**Goal:** make `COMMIT` the point where deferred FK violations are decided.
**Files:** `fk_enforcement.rs`, `exec_with_ctx.rs`, `exec_entry.rs`.
**Approach:** validate pending deferred FKs before finalizing commit; on error,
roll back the whole transaction and return the violation.

### Tests to add

```rust
#[test]
fn deferred_fk_allows_child_before_parent_until_commit() { ... }

#[test]
fn commit_fails_and_rolls_back_on_unrepaired_deferred_fk() { ... }

#[test]
fn deferred_parent_delete_violation_surfaces_on_commit() { ... }
```

### Implementation outline

- Add `validate_deferred_fks(...)` helper(s).
- Run validation just before `txn.commit(...)`.
- On failure:
  - call rollback path
  - clear session deferred state
  - return FK violation from COMMIT

### Verification

```bash
cargo test -p axiomdb-sql --test integration_deferrable_fk
```

## Step 5 — Wire smoke and regression sweep

**Goal:** prove the feature through the MySQL wire and guard immediate FK behavior.
**Files:** `tools/wire-test.py`, SQL integration tests.
**Approach:** add one explicit-transaction smoke that inserts child before
parent under `DEFERRABLE INITIALLY DEFERRED`, then a failing `COMMIT` case.

### Tests to add

```python
def test_21_16_deferrable_fk_success(): ...
def test_21_16_deferrable_fk_commit_failure(): ...
```

### Verification

```bash
cargo test -p axiomdb-sql --test integration_deferrable_fk --test integration_fk --test integration_ddl_parser
python3 tools/wire-test.py
```

## Step 6 — Closeout

**Goal:** run final gates and record the subphase closure.

### Final verification

```bash
cargo fmt --check
cargo test -p axiomdb-sql --test integration_deferrable_fk --test integration_fk --test integration_ddl_parser
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
```

### Closeout checklist

- [ ] Mark `21.16` complete in `docs/progreso.md`.
- [ ] Update `memory/project_state.md` with the new active/next subphase.
- [ ] Append closure notes to `docs/fase-21.md`.
- [ ] Record implementation notes in `memory/architecture.md`.
- [ ] Record lessons in `memory/lessons.md`.
- [ ] Commit with `feat(fase-21): implement deferrable constraints`.

## Risks

- FK enforcement is called from many DML paths; missing one would create an
  inconsistent immediate/deferred split.
- Savepoint rollback must truncate deferred-FK bookkeeping exactly or commit
  may validate rows that no longer exist in the transaction state.
- Commit failure must not leave `SessionContext` or `ConnectionTxn` half-open.
