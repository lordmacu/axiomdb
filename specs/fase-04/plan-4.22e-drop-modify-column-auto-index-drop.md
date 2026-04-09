# Plan: 4.22e — ALTER TABLE DROP/MODIFY COLUMN auto-index-drop

## Files to create/modify
- `crates/axiomdb-sql/src/executor/ddl_alter_column.rs` — detect index / constraint dependencies, classify affected secondary indexes by layout, run row rewrite, then drop or rebuild roots with deferred free
- `crates/axiomdb-sql/src/executor/ddl_create_index.rs` — extract or expose a reusable helper that rebuilds a secondary index root from an existing `IndexDef` on heap or clustered tables
- `crates/axiomdb-sql/tests/integration_executor_ddl.rs` — heap-table coverage for auto-drop, auto-rebuild, unique enforcement, partial/include metadata preservation, and rejection of FK/CHECK/PRIMARY cases
- `crates/axiomdb-sql/tests/integration_clustered_alter_table.rs` — clustered-table coverage replacing current “must error” cases with successful rebuild/drop behavior where allowed
- `tools/wire-test.py` — add one wire-visible `DROP COLUMN` auto-drop case and one `MODIFY COLUMN` auto-rebuild case
- `docs-site/src/user-guide/sql-reference/ddl.md` — document automatic secondary-index drop/rebuild behavior and explicit rejections for PK/FK/CHECK dependencies
- `docs-site/src/internals/executor.md` — describe heap-vs-clustered ALTER rewrite semantics and secondary-index repair strategy
- `docs/progreso.md` — mark `4.22e` when closed and keep any residual heap-ADD-COLUMN gap visible if still pending
- `memory/project_state.md` — record the new ALTER TABLE capability and remaining limits

## Algorithm / Data structure
1. Resolve current table metadata once:
   - current columns
   - current indexes
   - current CHECK constraints
   - current FK constraints (child + parent references)
2. Classify the target column:
   - reject if it belongs to the PRIMARY KEY
   - reject if any FK references it as child or parent column
   - reject if any CHECK expression references it
3. Classify secondary indexes:
   - `definition_depends_on_column(idx)` if:
     - `idx.columns` contains `col_idx`
     - `idx.include_columns` contains `col_idx`
     - parsed partial-index predicate references `col_idx`
4. Compute layout-specific repair sets:
   - heap `DROP COLUMN`:
     - `drop_indexes = affected secondary indexes`
     - `rebuild_indexes = surviving secondary indexes` because heap rewrite may change RID
   - heap `MODIFY COLUMN`:
     - `drop_indexes = []`
     - `rebuild_indexes = all secondary indexes`
   - clustered `DROP COLUMN`:
     - `drop_indexes = affected secondary indexes`
     - `rebuild_indexes = []`
   - clustered `MODIFY COLUMN`:
     - `drop_indexes = []`
     - `rebuild_indexes = affected secondary indexes`
5. Execute row rewrite + catalog column change first.
   - Old secondary roots stay untouched during rewrite.
   - If rewrite fails, statement rollback restores rows and old indexes remain valid.
6. Apply index repair after the row rewrite succeeds:
   - For every `drop_index`:
     - collect old B-tree pages
     - delete the index catalog row
     - defer free of old pages through `TxnManager::defer_free_pages`
   - For every `rebuild_index`:
     - build a fresh root from the current table contents using the preserved `IndexDef`
     - update `root_page_id` in catalog with `update_index_root`
     - defer free of old pages
7. Preserve metadata exactly:
   - `index_id`
   - name
   - uniqueness
   - predicate SQL
   - fillfactor
   - include columns
   - index type / BRIN settings

## Implementation phases
1. Add dependency helpers in `ddl_alter_column.rs`:
   - CHECK-reference detection
   - FK-reference detection
   - secondary-index dependency detection via key/include/predicate
2. Add reusable “rebuild root from existing `IndexDef`” helper for heap + clustered layouts.
3. Refactor `alter_drop_column` to:
   - reject PK/FK/CHECK cases
   - rewrite rows
   - delete affected secondary indexes
   - rebuild surviving heap secondaries when needed
4. Refactor `alter_modify_column` to:
   - reject PK/FK/CHECK cases
   - keep current coercion/nullability validation
   - rewrite rows
   - rebuild the correct secondary index set for heap vs clustered
5. Add targeted SQL tests for heap + clustered behavior.
6. Extend wire smoke with client-visible assertions.
7. Update docs and progress tracking.

## Tests to write
- unit:
  - helper that detects index dependency through key/include/predicate
  - helper that detects CHECK-expression column dependency
- integration:
  - heap `DROP COLUMN` auto-drops unique / partial / include-dependent secondary indexes
  - heap `DROP COLUMN` rebuilds surviving unrelated secondary indexes so they remain queryable
  - heap `MODIFY COLUMN` rebuilds secondary indexes and preserves uniqueness enforcement
  - clustered `DROP COLUMN` on indexed secondary column succeeds and removes the index
  - clustered `MODIFY COLUMN` on indexed secondary column succeeds and keeps the index usable
  - FK / CHECK / PRIMARY dependency cases reject cleanly
- bench:
  - reuse existing DDL / parser smoke if available
  - if a dedicated ALTER benchmark exists later, compare heap indexed rewrite before/after root rebuild overhead

## Anti-patterns to avoid
- Do not free old index pages immediately; use deferred-free pages so rollback/savepoint semantics stay intact
- Do not recreate surviving indexes with lossy metadata (dropping predicate/include/fillfactor/BRIN settings)
- Do not special-case only key columns and forget INCLUDE / partial-predicate dependencies
- Do not silently allow PK/FK/CHECK-dependent column changes
- Do not patch clustered-only behavior while leaving heap secondary RIDs stale after rewrite

## Risks
- Catalog/index repair ordering: update roots or delete rows only after the row rewrite succeeds; mitigation: keep old roots untouched until repair phase
- Partial-index predicate parsing may fail on stored SQL; mitigation: surface a clear ALTER error and leave old metadata intact
- Heap rewrite changes RIDs; mitigation: rebuild every surviving heap secondary index after DROP/MODIFY
- Clustered rebuild helper drift from `CREATE INDEX`; mitigation: extract a shared root-build path instead of duplicating clustered-secondary encoding logic
