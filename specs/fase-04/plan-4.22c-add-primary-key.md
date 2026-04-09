# Plan: 4.22c — ALTER TABLE ADD PRIMARY KEY

## Files to create/modify
- `crates/axiomdb-sql/src/executor/ddl_alter_constraint.rs` — implement `TableConstraint::PrimaryKey` for ALTER TABLE: validate rows, build provisional PK metadata, force PK columns to `NOT NULL`, invoke rebuild, and clean up on failure
- `crates/axiomdb-sql/src/executor/ddl_alter_rebuild.rs` — reuse existing heap→clustered rebuild path; adjust only if a small helper extraction is needed for the new caller
- `crates/axiomdb-sql/tests/integration_executor_ddl.rs` — heap-table ALTER regression tests for populated/empty success and duplicate/NULL failure
- `crates/axiomdb-sql/tests/integration_clustered_rebuild.rs` or a new focused integration file — coverage for preserved secondary indexes and clustered metadata after installing the PK
- `crates/axiomdb-network/tests/integration_connection_lifecycle.rs` — no direct 4.22c work expected; keep existing regression suite green because DDL still exercises bootstrap/open paths
- `tools/wire-test.py` — add a wire-visible `ALTER TABLE ... ADD PRIMARY KEY (...)` smoke and regression assertions around the resulting table behavior
- `docs-site/src/user-guide/sql-reference/ddl.md` — document MySQL-style `ALTER TABLE ... ADD PRIMARY KEY (...)` behavior
- `docs-site/src/internals/executor.md` — document provisional PK build + clustered rewrite flow and cleanup invariants
- `docs/fase-04.md` — summarize implementation, known trade-offs, and validation
- `memory/project_state.md` — record the new ALTER TABLE capability and its constraints

## Algorithm / Data structure
```text
input: resolved heap table + requested PK columns

1. Resolve PK columns -> IndexColumnDef list + column positions.
2. Read current indexes:
   - reject if any existing index is_primary=true
3. Validate current heap rows:
   - scan visible rows under statement snapshot
   - for each row, extract PK tuple
   - if any component is NULL -> InvalidValue
4. Build a provisional unique heap B-tree root for the PK:
   - allocate empty index root
   - insert every visible row by encoded PK tuple
   - if B-tree insert sees duplicate -> map to UniqueViolation
5. Persist provisional PK metadata in the catalog:
   - name = explicit constraint name or `<table>_pkey`
   - is_unique=true, is_primary=true, root_page_id=provisional_root
6. Rewrite affected column metadata so PK columns become nullable=false.
7. Re-resolve the table so the rebuild path sees the new PK index.
8. Call existing heap->clustered rebuild flow:
   - scan rows in provisional PK order
   - bulk insert into clustered tree
   - rebuild existing secondary indexes with PK bookmarks
   - swap table root/layout and PK root in catalog
   - defer-free old heap pages + old secondary roots + provisional PK B-tree pages
9. On error before success:
   - free provisional PK B-tree pages explicitly
   - let statement rollback undo provisional catalog row / column metadata
10. Return rewritten-row count.
```

## Implementation phases
1. Add a focused helper in `ddl_alter_constraint.rs` to resolve/validate primary-key columns and reject existing `NULL` values before any catalog change.
2. Build the provisional unique heap index root using existing index-build primitives, with explicit duplicate-to-`UniqueViolation` mapping.
3. Persist the provisional primary index row and flip PK columns to `NOT NULL` in catalog metadata inside the same ALTER statement transaction.
4. Re-resolve the table and reuse `alter_rebuild_to_clustered()` to perform the physical rewrite.
5. Add explicit cleanup for provisional PK B-tree pages when any step after allocation fails.
6. Add executor integration tests for populated success, empty success, duplicate failure, NULL failure, and preserved secondary-index lookups.
7. Extend `tools/wire-test.py` with a client-visible `ALTER TABLE ... ADD PRIMARY KEY (...)` scenario.
8. Update docs and memory after implementation is green.

## Tests to write
- unit: none preferred; behavior is integration-heavy and tied to catalog/storage interaction
- integration:
  - `ALTER TABLE t ADD PRIMARY KEY (id)` succeeds on a populated heap table
  - `ALTER TABLE t ADD PRIMARY KEY (id)` succeeds on an empty heap table
  - duplicate existing key -> error and no partial metadata left behind
  - existing `NULL` in PK column -> error and no rewrite
  - composite primary key works
  - existing secondary index still answers correctly after the rewrite
  - key columns become `NOT NULL` after the ALTER
- wire:
  - smoke via `tools/wire-test.py` showing a heap table, later `ADD PRIMARY KEY`, then successful reads through the new clustered layout
- bench:
  - no new dedicated benchmark required for the subphase; reuse rebuild-path validation and mention that it shares the existing clustered rebuild cost profile

## Anti-patterns to avoid
- Do not duplicate the clustered rewrite algorithm in the `ADD PRIMARY KEY` branch; reuse the existing rebuild path
- Do not silently skip rows with `NULL` PK components the way secondary-index builders do; PK validation must fail hard
- Do not update table root/layout before the new clustered tree and rebuilt secondaries are fully ready
- Do not leave provisional PK B-tree pages allocated on failure
- Do not mutate existing secondary index metadata in place before the clustered swap is ready

## Risks
- Provisional PK root leaks pages if rebuild fails after allocation
  Mitigation: explicit `free_btree_pages()` cleanup on every error path after root creation
- Catalog reflects `NOT NULL` PK columns before physical rewrite completes
  Mitigation: perform catalog writes inside the statement transaction so rollback restores previous metadata
- Duplicate-key detection surfaces as a low-level B-tree error instead of SQL-level uniqueness
  Mitigation: map duplicate insert failures to `UniqueViolation` with the PK index name
- Existing helper paths may assume a PK is already fully installed once `is_primary=true` exists in catalog
  Mitigation: keep the provisional PK root valid before re-resolving and invoke rebuild immediately in the same statement
