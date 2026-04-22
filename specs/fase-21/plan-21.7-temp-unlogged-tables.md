# Plan: 21.7 — TEMP and UNLOGGED tables

Phase: 21 — Advanced SQL
Task: 21.7 TEMP and UNLOGGED tables
Spec: specs/fase-21/spec-21.7-temp-unlogged-tables.md
Status: completed

## Summary

Implement 21.7 in four bounded layers. First add persistence syntax and
catalog/session metadata so the rest of the engine can distinguish permanent,
temporary, and unlogged tables. Then land TEMP tables through a hidden
session schema plus lifecycle cleanup. After that wire UNLOGGED reopen
semantics using a clean-shutdown flag and the existing bulk-empty helpers.
Finish by patching metadata visibility and `SHOW CREATE TABLE`, then close the
subphase with network + SQL integration coverage.

## Dependencies

Must be done first:
- [x] `specs/fase-21/spec-21.7-temp-unlogged-tables.md` approved.
- [x] Existing CREATE TABLE / CTAS / LIKE paths are green after 21.6b.
- [x] Existing TRUNCATE bulk-empty helpers are available for root rotation.

Blocks:
- [x] Closing Phase 21.7 in `docs/progreso.md`.
- [ ] Later SQL-standard temporal-table work that needs stable table
      persistence metadata.

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_temp_unlogged_tables.rs` — end-to-end
  TEMP/UNLOGGED SQL coverage.

Modified files:
- `crates/axiomdb-sql/src/ast.rs` — add `TablePersistence` and persistence
  fields on CREATE TABLE statements.
- `crates/axiomdb-sql/src/lexer.rs` — add `TEMP`, `TEMPORARY`, `UNLOGGED`.
- `crates/axiomdb-sql/src/parser/mod.rs` — accept CREATE TEMP/UNLOGGED.
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse persistence across normal,
  LIKE, and AS SELECT CREATE forms.
- `crates/axiomdb-sql/tests/integration_ddl_parser.rs` — parser coverage.
- `crates/axiomdb-catalog/src/schema_table.rs` — persist table persistence.
- `crates/axiomdb-catalog/src/schema.rs` — round-trip tests.
- `crates/axiomdb-catalog/src/lib.rs` — re-export `TablePersistence`.
- `crates/axiomdb-catalog/src/writer.rs` — create tables with persistence.
- `crates/axiomdb-storage/src/meta.rs` — clean-shutdown flag helpers.
- `crates/axiomdb-sql/src/session.rs` — add temp-schema field + search-path helpers.
- `crates/axiomdb-sql/src/analyzer_bind.rs` — default-schema temp fallback path.
- `crates/axiomdb-sql/src/executor/shared.rs` — resolution cache must honor temp schema.
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs` — TEMP/UNLOGGED create paths.
- `crates/axiomdb-sql/src/executor/ddl_drop_table.rs` — session-temp drop path.
- `crates/axiomdb-sql/src/executor/ddl_show.rs` — SHOW CREATE prefixes.
- `crates/axiomdb-sql/src/executor/information_schema_exec.rs` — temp visibility filtering.
- `crates/axiomdb-network/src/mysql/connection.rs` — preserve/reset temp schema state.
- `crates/axiomdb-network/src/mysql/handler.rs` — disconnect/reset/change-user cleanup.
- `crates/axiomdb-network/src/mysql/shared_db.rs` — dirty-open unlogged truncation.
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs` — temp cleanup assertions.
- `docs/progreso.md`, `memory/project_state.md`, `memory/architecture.md`,
  `memory/lessons.md`, `docs/fase-21.md` — close subphase after gates.

## Step 1 — Syntax and metadata shape

Status: completed.

**Goal:** teach the parser/AST/catalog row format to distinguish permanent,
temporary, and unlogged tables.
**Files:** `ast.rs`, `lexer.rs`, `parser/mod.rs`, `parser/ddl.rs`,
`integration_ddl_parser.rs`, `schema_table.rs`, `schema.rs`, `writer.rs`,
`lib.rs`.
**Approach:** TDD — parser tests and `TableDef` round-trip tests first.

### Tests to add

```rust
#[test]
fn parse_create_temp_table() { ... }

#[test]
fn parse_create_unlogged_table_as_select() { ... }

#[test]
fn table_def_roundtrips_unlogged_persistence() { ... }
```

### Implementation outline

- Add `TablePersistence`.
- Extend CREATE parsing to consume `TEMP`, `TEMPORARY`, `UNLOGGED` before `TABLE`.
- Carry persistence through standard/LIKE/AS SELECT CREATE statements.
- Extend `TableDef` v4 row format and catalog writer options.

### Verification

```bash
cargo test -p axiomdb-catalog
cargo test -p axiomdb-sql --test integration_ddl_parser
```

## Step 2 — TEMP tables via hidden session schema

Status: completed.

**Goal:** make TEMP tables session-scoped, shadow permanent tables, and clean
themselves up on connection lifecycle boundaries.
**Files:** `session.rs`, `analyzer_bind.rs`, `executor/shared.rs`,
`ddl_create_table.rs`, `ddl_drop_table.rs`, `shared_db.rs`, `handler.rs`,
`connection.rs`, `integration_temp_unlogged_tables.rs`,
`integration_connection_lifecycle.rs`.
**Approach:** start with same-session SQL tests, then add reset/disconnect tests.

### Tests to add

```rust
#[test]
fn temp_table_shadows_public_table() { ... }

#[test]
fn temp_table_requires_unqualified_create_name() { ... }

#[tokio::test]
async fn temp_tables_drop_on_reset_connection() { ... }
```

### Implementation outline

- Add `SessionContext.temp_schema: Option<String>`.
- On first TEMP create, allocate hidden schema token and switch search path to
  `[temp_schema, "public"]`.
- Store TEMP tables in catalog under that hidden schema with
  `TablePersistence::Temporary`.
- Reuse existing CREATE/DROP/DML once resolution finds the temp schema first.
- Add cleanup helper that drops all tables from `session.temp_schema` during
  disconnect, `COM_RESET_CONNECTION`, and `COM_CHANGE_USER`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_temp_unlogged_tables
cargo test -p axiomdb-network --test integration_connection_lifecycle
```

## Step 3 — UNLOGGED dirty-open truncation

Status: completed.

**Goal:** keep UNLOGGED runtime behavior simple while making dirty reopen lose
table contents.
**Files:** `meta.rs`, `schema_table.rs`, `ddl_create_table.rs`,
`shared_db.rs`, `integration_temp_unlogged_tables.rs`.
**Approach:** unit-test the meta flag helpers, then add reopen integration tests.

### Tests to add

```rust
#[test]
fn clean_shutdown_flag_roundtrips() { ... }

#[test]
fn unlogged_table_survives_clean_reopen() { ... }

#[test]
fn unlogged_table_truncates_on_dirty_reopen() { ... }
```

### Implementation outline

- Add `clean_shutdown` read/write helpers in `meta.rs`.
- Mark database dirty on open before queries run.
- On graceful `SharedDatabase` drop, best-effort flush and mark clean.
- On dirty open, enumerate unlogged tables and truncate them using the existing
  bulk-empty/root-rotation helpers before serving queries.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_temp_unlogged_tables
cargo test -p axiomdb-network
```

## Step 4 — Metadata visibility and polish

Status: completed.

**Goal:** make SHOW/info_schema reflect persistence correctly and hide foreign
TEMP tables.
**Files:** `ddl_show.rs`, `information_schema_exec.rs`,
`integration_temp_unlogged_tables.rs`, `integration_g11_information_schema.rs`.
**Approach:** metadata-query tests first.

### Tests to add

```rust
#[test]
fn show_create_table_reconstructs_temporary_prefix() { ... }

#[test]
fn info_schema_hides_other_sessions_temp_tables() { ... }

#[test]
fn show_create_table_reconstructs_unlogged_prefix() { ... }
```

### Implementation outline

- Prefix SHOW CREATE output based on `TableDef.persistence`.
- Filter information_schema / SHOW TABLE STATUS rows so each session sees only
  its own TEMP tables.
- Reject FK declarations involving TEMP/UNLOGGED tables with explicit
  `NotImplemented`.

### Verification

```bash
cargo test -p axiomdb-sql --test integration_g11_information_schema
cargo test -p axiomdb-sql --test integration_temp_unlogged_tables
```

## Step 5 — Final integration and closure

Status: completed.

**Goal:** run the closing gates, update docs/memory, and close 21.7.

### Verification against spec

- [x] AST and parser accept `CREATE TEMP[TORARY] TABLE` and `CREATE UNLOGGED TABLE`.
- [x] `CreateTableStmt`, `CreateTableLikeStmt`, and `CreateTableAsSelectStmt`
      carry table persistence explicitly.
- [x] Catalog `TableDef` persists table persistence backward-compatibly.
- [x] Session context tracks a hidden temp schema for the current connection.
- [x] TEMP table resolution shadows permanent tables for unqualified names and
      falls back to `public` when absent.
- [x] TEMP tables auto-drop on disconnect, `COM_RESET_CONNECTION`, and
      `COM_CHANGE_USER`.
- [x] Other sessions cannot see or resolve TEMP tables they do not own.
- [x] UNLOGGED tables survive clean reopen and truncate on dirty reopen.
- [x] `SHOW CREATE TABLE` reconstructs TEMPORARY / UNLOGGED prefixes.
- [x] Information schema / SHOW metadata include current-session TEMP tables
      only and all UNLOGGED tables.
- [x] FK attempts involving TEMP/UNLOGGED tables fail explicitly with
      `DbError::NotImplemented`.
- [x] `cargo test -p axiomdb-catalog`
- [x] `cargo test -p axiomdb-sql`
- [x] `cargo test -p axiomdb-network`
- [x] `cargo clippy -p axiomdb-sql -- -D warnings`
- [x] `cargo clippy -p axiomdb-network -- -D warnings`

## Risk register

| Risk | Likelihood | Mitigation |
|---|---:|---|
| TEMP tables leak after reset/disconnect | medium | centralize cleanup in handler lifecycle and cover reset/change-user/disconnect tests |
| Hidden temp schema breaks resolution unexpectedly | medium | keep fallback order `temp_schema -> public` and regression-test shadowing + qualified access |
| Dirty-open detection false-positives truncate UNLOGGED tables | medium | make clean flag conservative and document legacy first-open behavior |
| Metadata views leak foreign TEMP tables | medium | filter in one place using `SessionContext.temp_schema` and add cross-session tests |
| FK semantics across persistence classes become inconsistent | low | reject TEMP/UNLOGGED FK definitions explicitly in 21.7 |

## Rollback plan

If implementation stalls mid-way:

1. Revert AST/catalog/meta changes together so persistence decoding never lands
   half-upgraded.
2. Keep TEMP lifecycle and UNLOGGED recovery work on the same branch until both
   pass end-to-end tests.
3. If UNLOGGED recovery proves too invasive, keep the TEMP groundwork and
   revise the spec before any 21.7 closure update.

## Estimated effort

Total: high.

- Step 1: 45-60 min
- Step 2: 1.5-2.5 h
- Step 3: 1.5-2.5 h
- Step 4: 45-75 min
- Step 5: 30-45 min
