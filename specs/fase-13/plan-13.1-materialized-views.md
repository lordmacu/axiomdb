# Plan: 13.1 — Materialized views

## Status

In progress.

## Goal

Implement the first bounded materialized-view slice as a catalog-owned
materialized relation with:

- `CREATE MATERIALIZED VIEW ... AS SELECT`
- `REFRESH MATERIALIZED VIEW ...`
- `DROP MATERIALIZED VIEW ...`

using full rebuild semantics and existing physical table storage.

## Sprint

Estimate: 1 focused session

├── Task 1: Define the catalog shape
│   Description: extend table metadata so a relation can be identified as a
│   materialized view and can persist its defining SQL text.
│   Dependencies: none
│   Done criterion: catalog encode/decode roundtrips the new metadata and old
│   rows remain backward-compatible.
│
├── Task 2: Add parser / AST surface
│   Description: parse `CREATE MATERIALIZED VIEW`, `REFRESH MATERIALIZED VIEW`,
│   and `DROP MATERIALIZED VIEW`.
│   Dependencies: Task 1 can run in parallel conceptually, but parser work does
│   not require it to land first.
│   Done criterion: new statements roundtrip through AST and targeted parser
│   tests pass.
│
├── Task 3: Implement create / refresh / drop execution
│   Description: create initial rows via CTAS-style materialization, persist
│   defining query text, rebuild rows on refresh, and cleanly drop both
│   relation data and metadata.
│   Dependencies: Task 1, Task 2
│   Done criterion: SQL integration tests prove create, select, refresh, and
│   drop behavior.
│
├── Task 4: Expose relation type in metadata
│   Description: update `SHOW FULL TABLES` and/or related metadata paths so
│   materialized views are not reported as ordinary base tables.
│   Dependencies: Task 1
│   Done criterion: metadata tests pin the relation type reported for a
│   materialized view.
│
└── Task 5: Wire smoke and closeout
    Description: extend wire smoke, update docs/state/memory, and run final
    gates for the subphase close.
    Dependencies: Task 3, Task 4
    Done criterion: `13.1` is closed locally with green targeted and global
    validation.

## Affected areas

New files:

- `specs/fase-13/spec-13.1-materialized-views.md` — behavioral contract.
- `specs/fase-13/plan-13.1-materialized-views.md` — execution plan.
- `crates/axiomdb-sql/tests/integration_materialized_views.rs` — targeted SQL
  coverage.

Modified files:

- `crates/axiomdb-catalog/src/schema_table.rs` — relation type / defining-query
  persistence.
- `crates/axiomdb-catalog/src/schema.rs` — roundtrip coverage for new table
  metadata.
- `crates/axiomdb-catalog/src/writer.rs` / `reader.rs` — create/read helpers.
- `crates/axiomdb-sql/src/ast.rs` — new statement variants.
- `crates/axiomdb-sql/src/parser/ddl.rs` / `parser/mod.rs` — grammar.
- `crates/axiomdb-sql/src/executor/ddl_create_table.rs` or sibling DDL modules
  — create / refresh / drop execution.
- `crates/axiomdb-sql/src/executor/ddl_show.rs` and related metadata paths —
  relation type reporting.
- `tools/wire-test.py` — bounded `13.1` smoke.
- `docs/progreso.md`, `memory/project_state.md`, `docs/fase-13.md`,
  `memory/architecture.md`, `memory/lessons.md` — closeout.

## Risks

- `TableDef` is on-disk metadata, so the new trailer must remain append-only
  and preserve decoding of older rows.
- Refresh rebuild needs a safe replacement strategy; a naive in-place mutation
  can leave partial state visible if execution fails mid-refresh.
- Without regular views, naming/metadata semantics must stay explicit to avoid
  pretending the engine already has a generic relation-kind system.
