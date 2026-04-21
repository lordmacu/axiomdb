# Plan: 21.5f — Generated Columns

Phase: 21 — Advanced SQL
Task: 21.5f GENERATED ALWAYS AS columns
Spec: specs/fase-21/spec-21.5f-generated-columns.md
Status: in-progress

## Summary

Implement generated columns in the lowest-risk order: AST/parser first,
catalog persistence second, then write-time materialization for STORED columns
across INSERT and UPDATE-like paths. VIRTUAL stays represented but rejected at
DDL execution so future work can add logical-column evaluation without changing
the accepted syntax again.

## Dependencies

Must be done first:
- [x] `specs/fase-21/spec-21.5f-generated-columns.md` approved.
- [x] Phase 21.5 MERGE / UPSERT closed.
- [x] Existing catalog expression persistence for DEFAULT and ON UPDATE.

Blocks:
- [ ] Closing Phase 21.5f.
- [ ] Future true VIRTUAL generated-column support.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_generated_columns.rs` — parser and
  executor coverage.

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — add generated-column AST metadata.
- `crates/axiomdb-sql/src/lexer.rs` — add keywords if local parser style needs
  dedicated tokens.
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse generated column clauses.
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs` — validate and persist
  generated metadata.
- `crates/axiomdb-sql/src/executor/ddl_alter_column.rs` — reject ALTER generated
  columns explicitly.
- `crates/axiomdb-catalog/src/schema_table.rs` — serialize generated metadata.
- `crates/axiomdb-sql/src/executor/insert_helpers.rs` — central generated
  materialization and write-protection helpers.
- `crates/axiomdb-sql/src/executor/insert_heap.rs` and
  `crates/axiomdb-sql/src/executor/insert_heap_ctx.rs` — materialize generated
  values on heap INSERT paths.
- `crates/axiomdb-sql/src/executor/insert_clustered.rs` and
  `crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs` — materialize
  generated values on clustered INSERT paths or reject unsupported branch-local
  paths explicitly if needed.
- `crates/axiomdb-sql/src/executor/update_ctx.rs`,
  `crates/axiomdb-sql/src/executor/update_candidates.rs`,
  `crates/axiomdb-sql/src/executor/update_entry.rs`, and clustered update
  helpers — reject direct assignments and recompute STORED values.
- `crates/axiomdb-sql/src/executor/merge.rs`,
  `crates/axiomdb-sql/src/executor/odku_helpers.rs`,
  `crates/axiomdb-sql/src/executor/replace_helpers.rs`, and
  `crates/axiomdb-sql/src/executor/on_conflict_helpers.rs` — ensure conflict
  insert/update rows use the same helpers.
- `tools/wire-test.py` — optional smoke assertion if the final SQL surface is
  stable enough for wire coverage.
- `docs/progreso.md` and `memory/project_state.md` — close subphase after gates.

## Step 1 — AST and parser

Status: pending.

**Goal:** parse generated-column clauses without executor behavior.
**Files:** `ast.rs`, `lexer.rs`, `parser/ddl.rs`,
`tests/integration_generated_columns.rs`.
**Approach:** TDD parser tests first.

### Tests to add

```rust
#[test]
fn parses_stored_generated_column() {
    parse("CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) STORED)", None)
        .unwrap();
}

#[test]
fn parses_virtual_generated_column_for_explicit_runtime_rejection() {
    parse("CREATE TABLE t (a INT, b INT GENERATED ALWAYS AS (a + 1) VIRTUAL)", None)
        .unwrap();
}
```

### Implementation outline

- Add `GeneratedColumnKind` and `GeneratedColumn` to the SQL AST.
- Add `ColumnDef.generated: Option<GeneratedColumn>`.
- Parse `GENERATED ALWAYS AS (<expr>) [STORED|VIRTUAL]` in column definitions.
- Treat omitted kind as `Virtual` only if the local grammar needs MySQL
  tolerance; DDL still rejects non-STORED for this subphase.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_generated_columns
```

## Step 2 — Catalog persistence and DDL validation

Status: pending.

**Goal:** persist generated metadata for STORED columns and reject invalid DDL.
**Files:** `schema_table.rs`, `ddl_create_table.rs`, `ddl_alter_column.rs`,
`tests/integration_generated_columns.rs`.
**Approach:** catalog round-trip and DDL error tests first.

### Tests to add

```rust
#[test]
fn create_table_persists_stored_generated_metadata() { ... }

#[test]
fn virtual_generated_columns_are_not_implemented_yet() { ... }

#[test]
fn generated_column_cannot_use_default_or_auto_increment() { ... }
```

### Implementation outline

- Add generated metadata to catalog `ColumnDef`.
- Extend `to_bytes` / `from_bytes` with flags bit6/bit7.
- Convert AST generated expressions to SQL text via existing `expr_to_sql`.
- Validate generated expressions:
  - known columns only;
  - non-generated dependencies only;
  - no self-reference;
  - no DEFAULT / ON UPDATE / AUTO_INCREMENT on generated column.
- Return `NotImplemented` from ALTER generated-column paths.

### Verification

```bash
cargo test -p axiomdb-catalog
cargo test -p axiomdb-sql --test integration_generated_columns
```

## Step 3 — INSERT materialization

Status: pending.

**Goal:** compute STORED generated values before insert constraints and indexes.
**Files:** `insert_helpers.rs`, `insert_heap.rs`, `insert_heap_ctx.rs`,
`insert_clustered.rs`, `insert_clustered_ctx.rs`,
`tests/integration_generated_columns.rs`.
**Approach:** executor tests for VALUES, DEFAULT, and SELECT-source inserts first.

### Tests to add

```rust
#[test]
fn insert_values_computes_stored_generated_column() { ... }

#[test]
fn insert_default_for_generated_column_computes_value() { ... }

#[test]
fn insert_literal_for_generated_column_errors() { ... }

#[test]
fn insert_select_computes_generated_column() { ... }
```

### Implementation outline

- Add helper to detect generated columns and reject explicit non-DEFAULT writes.
- Add helper to evaluate all STORED generated expressions against the full row.
- Run generated materialization after defaults/auto-increment and before text
  constraints, CHECK, FK, index maintenance, and RETURNING.
- Ensure proposed conflict rows already contain computed generated values.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_generated_columns
cargo test -p axiomdb-sql --test integration_on_conflict
cargo test -p axiomdb-sql --test integration_insert_on_dup
```

## Step 4 — UPDATE and conflict-update recomputation

Status: pending.

**Goal:** recompute STORED generated values after every update-like mutation.
**Files:** update executors, `merge.rs`, `odku_helpers.rs`,
`on_conflict_helpers.rs`, `tests/integration_generated_columns.rs`.
**Approach:** update tests first, then wire helpers into shared mutation paths.

### Tests to add

```rust
#[test]
fn update_base_column_recomputes_generated_column() { ... }

#[test]
fn update_generated_column_literal_errors() { ... }

#[test]
fn on_conflict_do_update_recomputes_generated_column() { ... }

#[test]
fn merge_update_recomputes_generated_column() { ... }
```

### Implementation outline

- Reject update assignments targeting generated columns unless the value is
  `Expr::Default`.
- After normal assignments and ON UPDATE expressions, recompute all STORED
  generated columns.
- Keep recomputation central so heap, clustered, ODKU, ON CONFLICT, and MERGE
  use the same rule.
- Confirm RETURNING sees post-recompute values.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_generated_columns
cargo test -p axiomdb-sql --test integration_merge
cargo test -p axiomdb-sql --test integration_returning
```

## Step 5 — Final integration and closure

Status: pending.

**Goal:** run gates, update project docs, and close 21.5f.
**Files:** `docs/progreso.md`, `memory/project_state.md`,
`specs/fase-21/plan-21.5f-generated-columns.md`.

### Verification against spec

- [ ] AST can represent generated-column metadata.
- [ ] Parser accepts STORED generated column syntax.
- [ ] `CREATE TABLE` persists generated metadata and rejects out-of-scope forms.
- [ ] Catalog serialization remains backward-compatible.
- [ ] INSERT paths materialize STORED generated columns.
- [ ] UPDATE / conflict-update paths recompute STORED generated columns.
- [ ] Direct writes to generated columns are rejected except `DEFAULT`.
- [ ] `VIRTUAL` and `ALTER TABLE ... GENERATED` return explicit
      `NotImplemented`.
- [ ] Integration tests cover parser, catalog, INSERT, UPDATE, conflict paths,
      RETURNING, and error cases.
- [ ] `cargo fmt --check`
- [ ] `cargo test -p axiomdb-catalog`
- [ ] `cargo test -p axiomdb-sql`
- [ ] `cargo clippy -p axiomdb-sql -- -D warnings`
- [ ] `python3 tools/wire-test.py` if updated.

## Risk register

| Risk | Likelihood | Mitigation |
|---|---:|---|
| Updating catalog `ColumnDef` requires many test fixtures to add new fields | high | Prefer helper/default constructor where local style allows; otherwise update mechanically. |
| INSERT without column list has ambiguous generated-column arity | medium | Accept trailing omitted generated columns and reject explicit literal writes. |
| Conflict paths bypass central insert/update helpers | medium | Add targeted ON CONFLICT, ODKU, and MERGE tests. |
| VIRTUAL scope creeps into SELECT/executor redesign | high | Keep VIRTUAL as explicit `NotImplemented` in DDL. |

## Rollback plan

If implementation exposes an architectural blocker, leave spec approved, mark
this plan as blocked with the exact failing step, and revert only this
subphase's code/docs changes. Do not touch unrelated `.claude` working files.

## Estimated effort

Total: high, roughly 1-2 focused days.

- Step 1: 1 hour.
- Step 2: 2-3 hours.
- Step 3: 3-4 hours.
- Step 4: 3-5 hours.
- Step 5: 1-2 hours depending on gate failures.
