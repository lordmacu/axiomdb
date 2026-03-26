# Spec: 5.19a — Executor decomposition

## What to build
Split the monolithic SQL executor implementation in `crates/axiomdb-sql/src/executor.rs` into a directory module under `crates/axiomdb-sql/src/executor/` organized by responsibility, while preserving the current public API and all SQL-visible behavior.

This subphase is a structural refactor only. It must not change query semantics, error semantics, transaction behavior, FK behavior, index-maintenance behavior, planner choices, wire-visible results, or benchmark targets by itself.

The new executor layout must:
- keep `axiomdb_sql::execute(...)` and `axiomdb_sql::execute_with_ctx(...)` as the stable public entrypoints;
- keep `crate::executor::last_insert_id_value()` available for sibling modules such as `eval.rs`;
- replace the single 7K+ line implementation file with smaller modules grouped by execution responsibility;
- isolate DML, SELECT, aggregation, join, DDL, and shared helpers so future subphases can change one area without re-reading the entire executor;
- avoid widening visibility beyond what sibling executor modules need;
- preserve all existing tests and current behavior bit-for-bit from the SQL user's perspective.

## Research synthesis
### AxiomDB files that constrain the design
These files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/lib.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/eval.rs`
- `crates/axiomdb-sql/src/planner.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-sql/src/fk_enforcement.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `crates/axiomdb-sql/tests/integration_semantic_analyzer.rs`

### Research sources and how they inform this subphase
- `research/postgres/src/backend/executor/README`
  Borrow: organize execution code around distinct responsibilities instead of a single catch-all file.
  Reject: PostgreSQL's full plan-state tree and executor-state hierarchy.
  Adapt: AxiomDB keeps its current direct function-call executor, but mirrors the separation of concerns at the source-file level.

- `research/postgres/src/backend/executor/nodeModifyTable.c`
  Borrow: keep row-modification logic in its own unit instead of interleaving it with all SELECT/aggregate/DDL logic.
  Reject: `ModifyTable` as a runtime node/state abstraction.
  Adapt: AxiomDB will separate `insert.rs`, `update.rs`, and `delete.rs` modules while preserving the current `dispatch()` flow.

- `research/sqlite/src/insert.c`
  Borrow: statement-family-specific source files are easier to reason about than a single DML monolith.
  Reject: SQLite's VDBE opcode-generation architecture.
  Adapt: AxiomDB will split implementation by statement type, not by parser/codegen phases.

- `research/sqlite/src/update.c`
  Borrow: UPDATE-specific helpers should live near UPDATE execution, not be buried in a global executor file.
  Reject: coupling to SQLite's ephemeral-table and opcode internals.
  Adapt: UPDATE helpers and row-rewrite logic move into dedicated executor submodules without changing runtime semantics.

- `research/sqlite/src/delete.c`
  Borrow: DELETE and truncate-style logic deserve their own unit because they accumulate specialized fast paths and invariants.
  Reject: SQLite's VM-oriented control flow.
  Adapt: AxiomDB will isolate delete/truncate/bulk-empty logic in dedicated executor modules.

- `research/duckdb/src/execution/physical_plan/plan_aggregate.cpp`
  Borrow: aggregate-specific strategy code should be physically isolated from generic SELECT plumbing.
  Reject: DuckDB's vectorized physical-plan generator as an architectural template.
  Adapt: AxiomDB keeps its current row-based executor but moves aggregation/group-by code into `aggregate.rs`.

## Inputs / Outputs
- Input:
  - the current executor feature set implemented in `crates/axiomdb-sql/src/executor.rs`
  - stable public API:
    - `execute`
    - `execute_with_ctx`
    - `last_insert_id_value`
- Output:
  - `crates/axiomdb-sql/src/executor/` directory module with the same externally visible behavior
  - a small facade module that preserves existing import paths
- Errors:
  - no new user-visible SQL errors
  - no changed error messages or SQLSTATE mappings
  - build/test failures are blockers because this subphase is behavioral no-op refactoring

## Use cases
1. A contributor implementing `5.19` can work on DELETE/index-maintenance paths without navigating SELECT, aggregation, ALTER TABLE, and SHOW helpers in the same file.

2. A contributor debugging GROUP BY can read only aggregate and SELECT modules instead of a 7K-line executor file.

3. `eval.rs` and other sibling modules continue to call `crate::executor::last_insert_id_value()` without any import-path breakage.

4. Existing SQL integration tests continue to pass unchanged, proving that the split is structural only.

## Acceptance criteria
- [ ] `crates/axiomdb-sql/src/executor.rs` is replaced by a directory module rooted at `crates/axiomdb-sql/src/executor/mod.rs`.
- [ ] The public API remains stable: `axiomdb_sql::execute(...)` and `axiomdb_sql::execute_with_ctx(...)` continue to work without caller changes.
- [ ] `crate::executor::last_insert_id_value()` remains available to sibling modules after the split.
- [ ] Statement-dispatch logic remains centralized in the executor facade rather than being duplicated across submodules.
- [ ] SELECT/joins/aggregation code is split from DML and DDL code into separate files by responsibility, not arbitrary line-count chunks.
- [ ] Bulk-empty/truncate helper logic used by DELETE/TRUNCATE is isolated so `ddl.rs` and `delete.rs` do not re-grow into a new monolith.
- [ ] No SQL-visible behavior changes: result sets, warnings, transaction behavior, FK behavior, and error text remain unchanged.
- [ ] Existing executor integration tests continue to pass without being rewritten for different behavior.
- [ ] Visibility is kept as narrow as possible (`pub(crate)` or private inside `executor/`); the refactor does not expose new public crate API unless already required.
- [ ] No feature work from `5.19` or later subphases is folded into this refactor.

## Out of scope
- Any new SQL feature
- Any planner or cost-model change
- B+Tree/index-maintenance optimizations from `5.19`
- Session or wire protocol changes
- Error-message rewrites
- Benchmark-tuning changes beyond what falls out mechanically from code movement

## Dependencies
- Current executor feature set through the already implemented Phase 5 subphases
- Existing SQL integration tests in `crates/axiomdb-sql/tests/`
- Stable helper APIs already provided by:
  - `planner.rs`
  - `table.rs`
  - `fk_enforcement.rs`
  - `index_maintenance.rs`

## ⚠️ DEFERRED
- Any DML performance improvement that becomes easier after the split → pending in `5.19`
- Planner/API cleanup outside executor structure → pending in future executor/planner hygiene subphases
