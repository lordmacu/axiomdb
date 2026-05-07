# Plan: 20.3 — ENUMs

Phase: 20 — Types + import/export
Task: 20.3 ENUMs
Spec: `specs/fase-20/spec-20.3-enums.md`
Status: in-progress

## Summary

Implement named enum types as catalog objects whose values are stored physically
as text. The plan starts with durable enum metadata, then parses `CREATE TYPE
... AS ENUM`, preserves enum identity on table columns, validates writes through
shared coercion helpers, adds declaration-order `ORDER BY`, and closes with
wire/docs/regression gates. This order keeps the row codec stable while making
every user-visible behavior testable.

## Dependencies

Must be done first:

- [x] Phase 20.2 sequences closed.
- [x] Spec 20.3 approved.

Blocks:

- [ ] Phase 20.4 enum arrays.
- [ ] Later compact physical enum storage (`u32` ordinal row encoding).
- [ ] `ALTER TYPE` / dependency-aware enum evolution.

## Affected Files

New files:

- `crates/axiomdb-catalog/src/schema_enum.rs` — serialized enum type
  definition.
- `crates/axiomdb-sql/src/executor/ddl_enum.rs` — `CREATE TYPE ... AS ENUM`
  execution and optional `DROP TYPE` rejection/defer path.
- `crates/axiomdb-sql/tests/integration_enums.rs` — end-to-end enum coverage.

Modified files:

- `crates/axiomdb-storage/src/meta.rs` and `src/lib.rs` — enum catalog root
  meta-page slot if no existing spare root is available.
- `crates/axiomdb-catalog/src/{bootstrap,lib,reader,schema,writer}.rs` —
  enum catalog root, read/write APIs, lazy legacy initialization.
- `crates/axiomdb-catalog/src/schema_table.rs` — persist declared enum type
  identity on `ColumnDef` while keeping physical `ColumnType::Text`.
- `crates/axiomdb-sql/src/{ast,lexer}.rs` — enum DDL AST and type-name-bearing
  column definitions.
- `crates/axiomdb-sql/src/parser/{mod,ddl}.rs` — parse `CREATE TYPE ... AS ENUM`
  and custom enum column type names.
- `crates/axiomdb-sql/src/executor/{exec_dispatch,exec_explain,exec_with_ctx,mod}.rs`
  — DDL dispatch and plan invalidation.
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs` and
  `ddl_alter_column.rs` — resolve enum column type names, validate defaults,
  and persist enum metadata.
- `crates/axiomdb-sql/src/table*.rs`, `clustered_table.rs`, and shared insert /
  update helpers as needed — validate enum values through common coercion paths.
- `crates/axiomdb-sql/src/executor/ddl_show.rs`,
  `information_schema*.rs` — report declared enum type names.
- `crates/axiomdb-sql/src/executor/select_ctx.rs` / order helpers as needed —
  order enum columns by declared ordinal.
- `tools/wire-test.py` — wire smoke block.
- `docs/fase-20.md`, `docs/progreso.md`, `docs-site/src/user-guide/sql-reference/ddl.md`,
  `docs-site/src/internals/catalog.md`, `memory/project_state.md`, and
  optionally `memory/architecture.md` / `memory/lessons.md`.

## Step 1 — Catalog Shape

**Goal:** make enum definitions durable and readable.

Tests to add:

```rust
#[test]
fn enum_type_def_roundtrips_binary_format() {
    // EnumTypeDef -> bytes -> EnumTypeDef preserves schema/name/ordered labels.
}

#[test]
fn enum_type_persistence_across_reopen() {
    // Create enum catalog row, reopen, read ordered labels unchanged.
}
```

Implementation outline:

- Add `EnumTypeDef { schema_name, name, labels }` with compact serialization.
- Add an enum catalog root and lazy initialization for legacy databases.
- Add `CatalogReader::get_enum_type` / `list_enum_types_in_schema`.
- Add `CatalogWriter::create_enum_type` / optional delete helper.
- Reject empty labels and duplicate labels at write time as a defensive layer.

Verification:

```bash
cargo test -p axiomdb-catalog enum
```

## Step 2 — Parser And AST

**Goal:** parse enum type creation and custom enum column references.

Tests to add:

```rust
#[test]
fn parses_create_type_as_enum() {
    // CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')
}

#[test]
fn parses_enum_column_type_name() {
    // CREATE TABLE tasks (state mood NOT NULL)
}
```

Implementation outline:

- Add `CreateEnumTypeStmt { enum_type: TableRef, labels: Vec<String> }`.
- Add `Stmt::CreateEnumType`.
- Add parser dispatch for `CREATE TYPE`.
- Parse only string literals in the label list.
- Extend AST `ColumnDef` with `declared_type_name: Option<TableRef>` or an
  equivalent custom-type descriptor while keeping `data_type = DataType::Text`.
- Preserve existing built-in type parsing behavior before falling back to a
  custom identifier as an enum type candidate.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_ddl_parser enum
```

## Step 3 — DDL Execution And Column Metadata

**Goal:** create enum types and persist enum identity on columns.

Tests to add:

```rust
#[test]
fn create_enum_type_persists_catalog_entry() {
    // CREATE TYPE mood AS ENUM (...); catalog has labels in order.
}

#[test]
fn create_table_with_enum_column_persists_type_identity() {
    // state mood stored as physical Text plus declared enum type mood.
}
```

Implementation outline:

- Implement `execute_create_enum_type`.
- Reject duplicate enum type names in the same schema.
- Resolve enum type references during `CREATE TABLE` / `ALTER TABLE ADD COLUMN`
  / `ALTER TABLE MODIFY COLUMN`.
- Persist declared enum type identity on `CatalogColumnDef` / `ColumnDef` using
  a backward-compatible extension.
- Keep physical `ColumnType::Text` for enum columns.
- Validate enum defaults at DDL time where default expressions are literal or
  evaluable through existing default evaluation.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_enums create
cargo test -p axiomdb-catalog enum
```

## Step 4 — Write-Path Validation

**Goal:** reject invalid enum values on every supported table write path.

Tests to add:

```rust
#[test]
fn insert_invalid_enum_value_errors_without_partial_write() {
    // Multi-row INSERT with one bad value leaves table unchanged.
}

#[test]
fn update_invalid_enum_value_errors() {
    // UPDATE state='bad' fails.
}
```

Implementation outline:

- Add a helper that builds per-column enum label maps from resolved columns.
- Integrate validation after normal type coercion in `coerce_values` /
  `coerce_values_with_ctx` or a nearby shared helper so heap, clustered,
  INSERT SELECT, ODKU, ON CONFLICT, MERGE, and generated values share behavior.
- Treat `NULL` as valid for nullable columns; existing NOT NULL validation
  remains responsible for rejecting nulls.
- Reject non-text values after coercion with deterministic `DbError`.
- Preserve statement atomicity by validating rows before physical writes in
  batch/multi-row paths where possible.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_enums write
```

## Step 5 — Ordering And Metadata Surfaces

**Goal:** expose enum identity and order enum columns by declaration order.

Tests to add:

```rust
#[test]
fn order_by_enum_uses_declaration_order() {
    // labels declared low, medium, high sort in that order, not lexical order.
}

#[test]
fn show_create_table_reports_enum_type_name() {
    // SHOW CREATE TABLE tasks contains "state mood".
}
```

Implementation outline:

- Teach `ORDER BY` on direct enum column references to map labels to ordinals.
- Keep equality predicates and indexes text-backed; no index-key format changes.
- Update `SHOW CREATE TABLE`, `DESCRIBE`, `SHOW FULL COLUMNS`, and
  `information_schema.COLUMNS` to display declared enum type name.
- Document any unsupported non-direct enum ordering expressions explicitly.

Verification:

```bash
cargo test -p axiomdb-sql --test integration_enums order
cargo test -p axiomdb-sql --test integration_enums metadata
```

## Step 6 — Wire, Docs, And Closeout

**Goal:** close the subphase with user-visible behavior and regression gates.

Tasks:

- Add wire smoke checks for create type, create table, valid insert, invalid
  insert, order by, metadata, and cleanup.
- Document enum DDL and text-backed physical contract.
- Update internals catalog documentation.
- Update `docs/fase-20.md`, `docs/progreso.md`, and memory.
- Mark this plan `done` only after all closeout gates pass.

Verification:

```bash
cargo test -p axiomdb-catalog enum
cargo test -p axiomdb-sql --test integration_ddl_parser enum
cargo test -p axiomdb-sql --test integration_enums
python3 tools/wire-test.py
cargo test --workspace
cargo clippy --workspace -- -D warnings
cargo fmt --check
```

## Risks

| Risk | Likelihood | Mitigation |
|------|------------|------------|
| column metadata extension breaks old catalog rows | medium | add roundtrip and legacy decode tests before executor wiring |
| validation misses a write path | high | integrate in shared coercion helper and cover INSERT/UPDATE/ODKU/ON CONFLICT/MERGE |
| enum ORDER BY requires planner/analyzer type context not present today | medium | implement direct-column ORDER BY first and document/test fallback limits |
| duplicate label handling with Unicode/case | low | exact string set, no case folding |
| compact storage deferred causes future migration work | medium | document text-backed contract and preserve enum type identity for future migration |

## Estimated Effort

Total: high

- Step 1: 1-2 h
- Step 2: 1-2 h
- Step 3: 2-3 h
- Step 4: 3-5 h
- Step 5: 2-4 h
- Step 6: 1-2 h
