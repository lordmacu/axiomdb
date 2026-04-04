# Spec: 39.19 — Table Rebuild: Heap → Clustered Migration

## What to build (not how)

An `ALTER TABLE t REBUILD` command that migrates a legacy heap table with an
existing PRIMARY KEY index into clustered storage. The rebuild must:

- preserve all statement-visible rows,
- switch the table metadata from `Heap` to `Clustered`,
- make the PRIMARY KEY root point at the clustered tree root,
- rebuild non-primary indexes so their entries use clustered PK bookmarks
  instead of heap `RecordId`s,
- and reclaim the old heap/index pages only through deferred free after the
  rebuild transaction commits.

This subphase is for legacy heap tables that predate `39.13` or for catalog
fixtures that still represent `Heap + PK index`. It is not a new creation path
for modern tables with explicit PRIMARY KEY, because those already start as
clustered.

## Inputs / Outputs

- Input:
  - `ALTER TABLE <table> REBUILD`
  - resolved table metadata (`TableDef`, columns, indexes)
  - active DDL transaction managed by the executor
- Output:
  - `QueryResult::Affected { count, last_insert_id: None }`
    - `count` = number of visible rows migrated into the new clustered tree
- Errors:
  - table already clustered
  - table has no PRIMARY KEY
  - table metadata is inconsistent (missing PK root / unreadable old pages)
  - rebuild fails before the catalog swap

## Use cases

1. Legacy heap table with PRIMARY KEY and no secondary indexes is rebuilt into
   clustered layout; all rows remain readable by PK lookup and range scan.
2. Legacy heap table with PRIMARY KEY plus secondary indexes is rebuilt; the
   secondaries still answer lookups after the migration, but now point at
   clustered PK bookmarks.
3. Empty legacy heap table with PRIMARY KEY is rebuilt into an empty clustered
   root without panics or `unwrap()`.
4. `ALTER TABLE ... REBUILD` on a table without PRIMARY KEY returns a clear
   error instead of creating an invalid clustered table.
5. `ALTER TABLE ... REBUILD` on an already clustered table returns a clear
   error and leaves metadata unchanged.

## Acceptance criteria

- [ ] `ALTER TABLE t REBUILD` converts a legacy heap table with PRIMARY KEY into
      `TableStorageLayout::Clustered`
- [ ] The rebuilt table root is a clustered root and the PRIMARY KEY catalog
      root points at that same clustered root
- [ ] Every visible heap row is preserved exactly once in the rebuilt clustered
      table
- [ ] The rebuild consumes rows in PRIMARY KEY logical order so the resulting
      clustered tree is ordered by PK
- [ ] Secondary indexes are rebuilt with clustered PK bookmarks, not heap
      `RecordId`s
- [ ] Old heap/index pages are reclaimed via deferred free after commit, not
      immediate `free_page()` during the catalog swap
- [ ] On rebuild failure before success, the old table layout/root and old index
      roots remain authoritative
- [ ] `SELECT`, `UPDATE`, `DELETE`, and `VACUUM` work on the rebuilt table
- [ ] Integration test: seed a legacy heap+PK table, rebuild it, then verify
      clustered reads and writes
- [ ] Integration test: rebuild a legacy heap+PK+secondary table and verify the
      secondary path still works
- [ ] Integration test: rebuild on a heap table without PRIMARY KEY returns an
      error
- [ ] Integration test: rebuild on an already clustered table returns an error

## Out of scope

- Online rebuild with concurrent DML
- A dedicated bottom-up bulk builder (`BtrBulk`-style) for clustered trees
- Clustered → heap reverse migration
- Hidden rowid / implicit clustered PK creation
- Rebuilding tables while simultaneously altering columns or constraints

## ⚠️ DEFERRED

- General rollback tracking for newly allocated rebuild pages if a failure
  happens after the executor hands control back to the outer DDL commit path.
  This depends on broader transactional page-allocation accounting and remains a
  cross-cutting concern beyond `39.19`.

## Dependencies

- 39.3–39.10 clustered storage, lookup, range, and overflow rows ✅
- 39.9 clustered secondary PK bookmarks ✅
- 39.13 clustered table metadata in the catalog ✅
- 39.14–39.18 executor-visible clustered DML + VACUUM ✅
