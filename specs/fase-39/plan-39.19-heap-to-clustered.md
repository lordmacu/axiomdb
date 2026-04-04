# Plan: 39.19 — Table Rebuild: Heap → Clustered Migration

## Files to create/modify

- `crates/axiomdb-sql/src/executor/ddl.rs` — make `ALTER TABLE ... REBUILD`
  perform a safe legacy heap→clustered migration
- `crates/axiomdb-sql/tests/integration_clustered_rebuild.rs` — replace the
  placeholder coverage with real migration tests seeded from legacy heap+PK
  fixtures
- `docs/fase-39.md` — close out the subphase and document the migration
  boundary
- `docs/progreso.md` — mark `39.19` complete and record any real deferred gap
- `docs-site/src/user-guide/sql-reference/ddl.md` — user-facing `ALTER TABLE
  ... REBUILD` behavior and error cases
- `docs-site/src/user-guide/features/indexes.md` — explain legacy rebuild into
  clustered storage
- `docs-site/src/internals/architecture.md` — DDL swap + deferred free flow
- `docs-site/src/internals/storage.md` — clustered migration / page lifecycle
- `docs-site/src/development/roadmap.md` — phase status
- `memory/project_state.md` — current phase status
- `memory/architecture.md` — rebuild flow and catalog/root semantics
- `memory/lessons.md` — only if the implementation exposes a new design lesson

## Algorithm / Data structure

Borrow the sorted rebuild idea from InnoDB / PostgreSQL CLUSTER, but keep the
swap semantics aligned with AxiomDB's existing `index_integrity` /
`bulk_empty` patterns.

```text
execute_alter_table_rebuild(table):
  resolve table + columns + indexes
  require table.storage_layout == Heap
  require a PRIMARY KEY index exists

  source_rows = scan visible rows in PK logical order
    preferred: walk the old PK B-tree, then batch-read heap rows by RecordId

  new_clustered_root = build clustered tree from source_rows
    for each row in PK order:
      pk_key = encode PK columns
      row_bytes = encode logical row
      clustered_tree::insert(...)

  new_secondary_roots = rebuild every non-primary index
    derive ClusteredSecondaryLayout(sec_idx, pk_idx)
    insert entries using clustered PK bookmarks

  old_pages = collect old heap-chain pages + old PK tree pages + old secondary tree pages

  flush storage so the newly built roots are durable before catalog metadata points at them

  catalog swap inside the active DDL txn:
    update table root -> new clustered root
    update table storage_layout -> Clustered
    update PK index root -> new clustered root
    update each secondary index root -> rebuilt root
    txn.defer_free_pages(old_pages)

  return affected row count
```

Error path:

```text
if rebuild fails before success:
  free newly built clustered / secondary roots best-effort
  leave old table root, layout, and old index roots untouched
```

## Implementation phases

1. Rework the `ALTER TABLE ... REBUILD` contract in `ddl.rs` around legacy heap
   tables only, not already-clustered tables.
2. Replace immediate old-page reclamation with the safe `flush -> catalog swap
   -> defer_free_pages` pattern.
3. Remove `unwrap/expect` from the rebuild path, including empty-table root
   creation.
4. Seed realistic legacy heap+PK fixtures in integration tests and assert the
   post-rebuild table is actually clustered.
5. Update docs, progress tracking, and memory only after the migration path and
   its tests are consistent.

## Tests to write

- unit:
  - empty-table rebuild allocates an empty clustered root without panic
  - rebuild failure on already-clustered table returns a clear error
  - rebuild failure on heap table without PRIMARY KEY returns a clear error
- integration:
  - seed a legacy heap+PK table directly, rebuild it, verify `storage_layout`
    flips to `Clustered` and `SELECT` sees all rows
  - same fixture with a secondary index; verify post-rebuild secondary lookups
    still work through clustered PK bookmarks
  - rebuild preserves DML viability: `UPDATE`, `DELETE`, `VACUUM` after rebuild
  - empty legacy table rebuild works
  - rebuild through the ctx path so the normal DDL autocommit boundary is
    exercised
- bench:
  - not required in `39.19`; the phase-level benchmark closeout belongs to
    `39.20`

## Anti-patterns to avoid

- DO NOT free old heap/index pages immediately after the catalog swap
- DO NOT validate `39.19` using only tables created with `PRIMARY KEY`, because
  those are already clustered after `39.13`
- DO NOT rebuild secondaries with heap `RecordId` suffixes after the swap
- DO NOT leave `unwrap()` / `expect()` in production rebuild code
- DO NOT update the catalog before the new clustered and secondary roots are
  durable enough for the DDL commit path

## Risks

- Legacy-fixture setup in tests is more involved because user-visible `CREATE
  TABLE ... PRIMARY KEY` now produces clustered tables. Mitigation: seed the
  pre-39.13 heap+PK shape directly via catalog/storage helpers in the test.
- Rebuild allocates new pages outside physical WAL logging. Mitigation: flush
  before the metadata swap, clean up new roots on pre-commit errors, and keep
  the remaining post-dispatch commit-failure leak as an explicit deferred item.
- Very large tables would prefer a bottom-up bulk builder or external sort.
  Mitigation: keep `39.19` correct first; performance-oriented bulk build
  remains outside scope.
