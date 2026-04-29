# Plan: 20.2 — Sequences

Phase: 20 — Types + import/export
Task: 20.2 Sequences
Spec: `specs/fase-20/spec-20.2-sequences.md`
Status: in-progress

## Summary

Implement standalone sequence objects in the same style as recent catalog-backed
DDL work: add durable catalog metadata, parse sequence DDL, wire executor
create/drop operations, then route `NEXTVAL` / `CURRVAL` through session-aware
expression evaluation. The highest-risk item is preserving sequence gaps across
transaction rollback, so that is tested before closeout.

## Dependencies

Must be done first:

- [x] Phase 20.1 regular views closed.
- [x] Spec 20.2 approved.

Blocks:

- [ ] `SERIAL` / identity-column ownership over standalone sequences.
- [ ] `ALTER SEQUENCE`, `SETVAL`, and `OWNED BY`.

## Affected Files

New files:

- `crates/axiomdb-catalog/src/schema_sequence.rs` — serialized sequence
  definition/state.
- `crates/axiomdb-sql/src/executor/ddl_sequence.rs` — create/drop sequence
  execution.
- `crates/axiomdb-sql/tests/integration_sequences.rs` — end-to-end coverage.

Modified files:

- `crates/axiomdb-catalog/src/{bootstrap,lib,reader,schema,writer}.rs` —
  sequence catalog root, read/write APIs, lazy legacy initialization.
- `crates/axiomdb-storage/src/meta.rs` — meta-page root offset if no existing
  spare catalog slot is available.
- `crates/axiomdb-sql/src/{ast,lexer}.rs` — sequence AST and keywords.
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse sequence DDL and options.
- `crates/axiomdb-sql/src/executor/{exec_dispatch,exec_explain,exec_with_ctx,mod}.rs`
  — DDL dispatch.
- `crates/axiomdb-sql/src/eval/functions/{mod,system}.rs` and eval context
  plumbing — `NEXTVAL` / `CURRVAL`.
- `crates/axiomdb-sql/src/plan_deps.rs` — invalidate plans on sequence DDL.
- `tools/wire-test.py` — wire smoke block.
- `docs/fase-20.md`, `docs/progreso.md`, `docs-site/src/user-guide/sql-reference/ddl.md`,
  `docs-site/src/user-guide/sql-reference/expressions.md`,
  `docs-site/src/internals/catalog.md`, and `memory/project_state.md`.

## Step 1 — Catalog Shape

**Goal:** make sequence definitions durable and readable.

Tests to add:

```rust
#[test]
fn sequence_def_roundtrips_binary_format() {
    // SequenceDef -> bytes -> SequenceDef preserves every field.
}
```

Implementation outline:

- Add `SequenceDef` with compact binary serialization.
- Add a new catalog root for `axiom_sequences`.
- Add `CatalogReader::get_sequence` / `list_sequences`.
- Add `CatalogWriter::create_sequence`, `replace_sequence_state`, and
  `delete_sequence`.
- Ensure legacy databases lazily allocate the root.

Verification:

```bash
cargo test -p axiomdb-catalog sequence
```

## Step 2 — DDL Parser And Executor

**Goal:** support `CREATE SEQUENCE` and `DROP SEQUENCE`.

Tests to add:

```rust
#[test]
fn parses_create_sequence_with_options() {
    // CREATE SEQUENCE s START WITH 10 INCREMENT BY 5 MINVALUE 1 MAXVALUE 99
}

#[test]
fn create_drop_sequence_persists_catalog_entry() {
    // CREATE SEQUENCE s; DROP SEQUENCE s; missing lookup fails.
}
```

Implementation outline:

- Add AST nodes `CreateSequenceStmt` and `DropSequenceStmt`.
- Parse defaults plus `IF NOT EXISTS`, `IF EXISTS`, `START WITH`,
  `INCREMENT BY`, `MINVALUE`, `MAXVALUE`, `NO MINVALUE`, `NO MAXVALUE`,
  `CACHE`, `CYCLE`, and `NO CYCLE`.
- Validate `INCREMENT BY 0`, invalid bounds, and invalid start values in the
  executor before writing catalog rows.
- Dispatch DDL through the write executor path.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser sequence
cargo test -p axiomdb-sql --test integration_sequences create_drop
```

## Step 3 — NEXTVAL Runtime

**Goal:** advance sequence state and return one value per expression evaluation.

Tests to add:

```rust
#[test]
fn nextval_advances_per_output_row() {
    // SELECT NEXTVAL('s') FROM t ORDER BY id returns 1, 2, 3.
}

#[test]
fn rollback_does_not_reuse_nextval_value() {
    // NEXTVAL in rolled-back txn consumes the value; next txn sees the next one.
}
```

Implementation outline:

- Add an execution path for `nextval(text)` that resolves `schema.name`.
- Serialize state advancement under the existing database write path or a
  sequence-specific lock so concurrent callers cannot duplicate values.
- Persist the advanced state immediately enough that rollback cannot reuse a
  consumed value.
- Return `DbError::InvalidValue` for missing sequences and exhaustion.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_sequences nextval
```

## Step 4 — CURRVAL Session State

**Goal:** expose session-local last sequence values.

Tests to add:

```rust
#[test]
fn currval_requires_session_nextval_first() {
    // CURRVAL('s') before NEXTVAL('s') errors.
}

#[test]
fn currval_tracks_sequence_name_per_session() {
    // NEXTVAL('a') does not define CURRVAL('b').
}
```

Implementation outline:

- Extend session/executor context with a `HashMap<qualified_sequence, i64>`.
- Set the entry after successful `NEXTVAL`.
- Route `CURRVAL` through context-aware evaluation; keep the current
  zero-context function dispatcher rejecting it if no session context exists.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_sequences currval
```

## Step 5 — Integration, Docs, And Closeout

**Goal:** close the subphase with user-visible behavior and regression gates.

Tasks:

- Add wire smoke checks for create/nextval/currval/drop.
- Document sequence DDL and expression functions.
- Update internals catalog documentation.
- Update `docs/fase-20.md`, `docs/progreso.md`, and memory.
- Run closeout gates.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_sequences
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| rollback accidentally reuses consumed values | high | write a failing rollback test before finalizing `NEXTVAL` |
| eval path lacks session context everywhere | medium | start with SELECT and wire paths; reject context-free calls clearly |
| catalog root expansion affects legacy databases | medium | mirror aggregate root lazy-init pattern |
| concurrent `NEXTVAL` duplicates values | medium | serialize sequence state replacement under a sequence/database write lock |

## Estimated Effort

Total: high

- Step 1: 1-2 h
- Step 2: 1-2 h
- Step 3: 2-4 h
- Step 4: 1-2 h
- Step 5: 1-2 h
