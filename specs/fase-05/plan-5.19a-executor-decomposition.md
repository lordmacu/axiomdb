# Plan: 5.19a — Executor decomposition

## Files to create/modify
- `crates/axiomdb-sql/src/executor.rs` — delete/replace with directory module
- `crates/axiomdb-sql/src/executor/mod.rs` — public facade: thread-local last-insert-id, `execute`, `execute_with_ctx`, `dispatch`, `dispatch_ctx`, `SET` ctx helper, narrow re-exports of shared executor helpers
- `crates/axiomdb-sql/src/executor/shared.rs` — generic helper functions used across multiple executor areas: cached table resolution, projection, order/limit helpers, type inference, common column-meta utilities
- `crates/axiomdb-sql/src/executor/select.rs` — `SELECT` entrypoints, derived-select execution, projection flow, ORDER BY/LIMIT application wiring
- `crates/axiomdb-sql/src/executor/joins.rs` — join execution and join-specific metadata/nullability/type helpers
- `crates/axiomdb-sql/src/executor/aggregate.rs` — aggregate expression collection, accumulators, grouped execution, DISTINCT/group-key helpers
- `crates/axiomdb-sql/src/executor/insert.rs` — `execute_insert` and `execute_insert_ctx`
- `crates/axiomdb-sql/src/executor/update.rs` — `execute_update` and `execute_update_ctx`
- `crates/axiomdb-sql/src/executor/bulk_empty.rs` — `BulkEmptyPlan`, heap/index root-rotation helpers, and page-collection/free helpers shared by DELETE/TRUNCATE
- `crates/axiomdb-sql/src/executor/delete.rs` — `collect_delete_candidates`, `execute_delete`, and `execute_delete_ctx`; uses `bulk_empty.rs` instead of owning truncate helpers
- `crates/axiomdb-sql/src/executor/ddl.rs` — `CREATE/DROP/ALTER/SHOW/ANALYZE/TRUNCATE` execution, using `bulk_empty.rs` for truncate fast path
- `crates/axiomdb-sql/src/lib.rs` — keep `pub mod executor;` and `pub use executor::{execute, execute_with_ctx};` unchanged; adjust only if module-path comments/docs need refreshing
- `crates/axiomdb-sql/src/eval.rs` — no semantic change; only touch if needed to keep `crate::executor::last_insert_id_value()` path compiling via the new facade
- `crates/axiomdb-sql/tests/integration_executor.rs` — regression test target; only update imports/helpers if module-path changes require it

## Algorithm / Data structure
### 1. Replace the file module with a facade module
Rust module layout after the refactor:

```text
executor/
  mod.rs
  shared.rs
  select.rs
  joins.rs
  aggregate.rs
  insert.rs
  update.rs
  bulk_empty.rs
  delete.rs
  ddl.rs
```

Rules:
- `mod.rs` stays small and owns only:
  - the `thread_local!` last-insert-id state
  - `last_insert_id_value()`
  - `execute(...)`
  - `execute_with_ctx(...)`
  - `dispatch(...)`
  - `dispatch_ctx(...)`
  - `is_ddl(...)`
  - `execute_set_ctx(...)`
  - minimal `pub(crate)` re-exports needed across sibling executor modules
- All large statement-family implementations move out of the facade.

### 2. Split by responsibility, not by line count
Move code according to semantic ownership:

```text
shared.rs
  resolve_table_cached
  build_column_mask / collect_column_refs
  type/column-name inference helpers
  project_row / project_row_with
  order-by helpers
  LIMIT/OFFSET helpers

joins.rs
  apply_join
  eval_join_cond
  concat_rows
  build_join_column_meta
  join nullability helpers

aggregate.rs
  AggExpr / AggAccumulator / GroupState
  aggregate-expression collection
  group-by strategy selection
  grouped SELECT execution
  session-aware distinct/group-key helpers

select.rs
  execute_select(_ctx)
  execute_select_derived
  execute_select_with_joins(_ctx)
  wiring to joins.rs + aggregate.rs + shared.rs

insert.rs
  execute_insert(_ctx)

update.rs
  execute_update(_ctx)

bulk_empty.rs
  BulkEmptyPlan
  alloc_empty_heap_root / alloc_empty_index_root
  collect_heap_chain_pages / collect_btree_pages
  plan_bulk_empty_table / apply_bulk_empty_table / free_btree_pages

delete.rs
  collect_delete_candidates
  execute_delete(_ctx)
  uses bulk_empty.rs for no-WHERE fast path

ddl.rs
  execute_create_table / execute_drop_table
  execute_create_index / execute_drop_index / execute_drop_index_by_id
  execute_analyze
  execute_truncate
  execute_show_tables / execute_show_columns
  execute_alter_table and alter helpers
  uses bulk_empty.rs for TRUNCATE
```

This layout is mandatory for the refactor. Do not invent a different split during implementation.

### 3. Dependency direction inside `executor/`
Keep dependency edges one-way:

```text
mod.rs -> {shared, select, insert, update, delete, ddl}
select.rs -> {shared, joins, aggregate}
aggregate.rs -> {shared}
delete.rs -> {shared, bulk_empty}
ddl.rs -> {shared, bulk_empty}
insert.rs/update.rs -> {shared}
joins.rs -> {shared}
bulk_empty.rs -> {}
```

Rules:
- `bulk_empty.rs` must not depend on `delete.rs` or `ddl.rs`.
- `shared.rs` must not call back into statement modules.
- `aggregate.rs` must not depend on `select.rs`.
- Avoid circular imports by moving only truly cross-cutting helpers into `shared.rs`.

### 4. Keep behavior stable with narrow internal re-exports
`mod.rs` should re-export only what sibling executor modules actually need, for example:

```text
pub(crate) use shared::{...}
pub(crate) use aggregate::{...}
pub(crate) use joins::{...}
pub(crate) use bulk_empty::{...}
```

Do not widen helpers to `pub` at crate root.
Do not change external callers to reach into new submodules.
All external code should continue to go through `crate::executor`.

### 5. Move code mechanically before refactoring internals
Implementation order matters:
1. create the new module files;
2. move code with minimal edits so everything compiles again;
3. only then tighten imports/visibility and remove dead helpers.

This avoids mixing structural movement with opportunistic cleanup.

## Implementation phases
1. Create `crates/axiomdb-sql/src/executor/` and `mod.rs`, keeping the current public facade (`execute`, `execute_with_ctx`, `last_insert_id_value`) intact.
2. Extract `shared.rs` with generic helper functions used by multiple statement families.
3. Extract `joins.rs` and `aggregate.rs`; update `select.rs` wiring to use them.
4. Extract `insert.rs`, `update.rs`, and `delete.rs` without changing runtime behavior.
5. Extract `bulk_empty.rs` and make both DELETE and TRUNCATE call into it.
6. Extract `ddl.rs` with CREATE/DROP/ALTER/SHOW/TRUNCATE/ANALYZE logic.
7. Delete the old monolithic `executor.rs` file and keep only the new directory module.
8. Tighten imports and visibility, ensuring no accidental new public API.
9. Run full regression validation on the SQL crate and workspace-level compile/test gates relevant to the refactor.

## Tests to write
- unit:
  - any existing executor-local tests that need relocation should move with their responsible module
  - add at least one compile-focused smoke test or module-level test if the split introduces new internal helper contracts
- integration:
  - run the existing executor integration suite unchanged to prove no semantic drift
  - run existing semantic-analyzer and FK/index-maintenance integration tests that exercise executor paths
- bench:
  - no new benchmark is required for the refactor itself
  - re-run the existing critical SQL/DML benchmarks after the split to confirm no >5% regression from structure-only changes

## Anti-patterns to avoid
- Do not mix this refactor with `5.19` batch-delete implementation.
- Do not split files by arbitrary 500/1000-line chunks.
- Do not “improve” algorithms or change planner/executor decisions while moving code.
- Do not widen helper visibility just to make compilation easier; prefer `pub(crate)` re-exports inside `executor/mod.rs`.
- Do not duplicate dispatch logic in multiple modules.
- Do not leave bulk-empty helpers half in DELETE and half in DDL; that recreates cross-cutting coupling immediately.
- Do not change public import paths for callers outside `executor/`.

## Risks
- Risk: the split accidentally changes behavior because helper functions move together with “small cleanups”.
  Mitigation: move code mechanically first, avoid semantic edits, and use existing integration tests as the behavioral oracle.

- Risk: circular dependencies appear between SELECT, aggregate, delete, and DDL helpers.
  Mitigation: enforce the fixed dependency graph above and isolate cross-cutting utilities in `shared.rs` and `bulk_empty.rs`.

- Risk: `last_insert_id_value()` path breaks sibling modules such as `eval.rs`.
  Mitigation: keep it defined or re-exported from `executor/mod.rs` under the same path.

- Risk: `ddl.rs` and `delete.rs` both need truncate/bulk-empty helpers and re-grow coupling.
  Mitigation: extract `bulk_empty.rs` as a dedicated shared internal module from the start.

- Risk: the refactor hides behavior changes behind test gaps.
  Mitigation: treat any changed SQL output, error text, warning count, or affected-row behavior as a blocker even if the compiler is happy.

## Assumptions
- This is a pure structural refactor done before more invasive DML work such as `5.19`.
- The current executor test suite is strong enough to act as the primary regression oracle for semantic stability.
- Future executor work will benefit more from a responsibility-based module tree than from further patching the monolithic file.
