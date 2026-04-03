# Spec: 39.13 clustered CREATE TABLE

## What to build (not how)

Build the first executor-visible entry point for the clustered storage rewrite:
`CREATE TABLE` must become clustered-aware.

This subphase must let the SQL DDL path create a real clustered table when the
table definition contains an explicit `PRIMARY KEY`.

The clustered `CREATE TABLE` contract must:

- persist in the catalog that a table is heap-backed or clustered-backed
- persist a generic table root page id instead of assuming every table root is
  a heap chain root
- allocate and initialize an empty clustered root page when a new clustered
  table is created
- create the logical primary-index catalog row for clustered tables so later
  clustered work can recover primary-key column order from catalog metadata
- keep `CREATE TABLE` without an explicit `PRIMARY KEY` on the existing heap
  path for now instead of inventing a hidden clustered row id
- reject accidental use of the current heap-only runtime paths on clustered
  tables with explicit, phase-scoped errors instead of silently treating a
  clustered root as a heap page

This subphase must stay aligned with the Phase 39 objective:

- do not invent a hidden row id / `GEN_CLUST_INDEX` equivalent now
- do not wire clustered `INSERT`, `SELECT`, `UPDATE`, or `DELETE` yet
- do not rewrite the generic executor around clustered rows yet
- do not remove heap-table support; clustered and heap tables coexist
  temporarily until `39.14`–`39.17`

## Inputs / Outputs

- Input:
  - `CreateTableStmt`
  - `&mut dyn StorageEngine`
  - `&mut TxnManager`
  - current catalog state
- Output:
  - `TableDef` rows carry:
    - storage layout (`heap` or `clustered`)
    - generic table root page id
  - `CREATE TABLE ... PRIMARY KEY ...` creates:
    - a table catalog row marked clustered
    - an empty clustered root page
    - the corresponding primary-index catalog metadata
  - `CREATE TABLE` without explicit `PRIMARY KEY` continues to create:
    - a heap table catalog row
    - an empty heap root page
  - heap-only runtime paths detect clustered tables and fail explicitly until
    later subphases implement them
- Errors:
  - existing DDL errors such as duplicate table, unknown referenced table, and
    invalid column references
  - explicit `DbError::NotImplemented` when a heap-only runtime path is invoked
    on a clustered table before `39.14`–`39.17`
  - catalog/storage errors while allocating or persisting heap/clustered roots

## Use cases

1. `CREATE TABLE users (id INT PRIMARY KEY, name TEXT)` creates a clustered
   table whose root page is an empty clustered leaf and whose catalog metadata
   says the table layout is clustered.
2. `CREATE TABLE logs (ts INT, msg TEXT)` continues to create a heap table
   because there is no explicit primary key and hidden clustered keys are still
   deferred.
3. `CREATE TABLE orders (id INT PRIMARY KEY, customer_id INT UNIQUE)` creates a
   clustered table plus the logical primary-index metadata and the secondary
   unique-index metadata needed by later clustered DML work.
4. A later `INSERT INTO users ...` against the clustered table fails with a
   clear phase-scoped `NotImplemented` instead of trying to append into a heap
   chain rooted at a clustered leaf page.
5. Legacy catalog rows written before `39.13` still decode as heap tables.

## Acceptance criteria

- [ ] `TableDef` can represent both heap and clustered tables, and legacy
      catalog rows still decode as heap by default.
- [ ] The table catalog stores a generic root page id instead of assuming the
      root always points at `PageType::Data`.
- [ ] `CatalogWriter` can create either a heap table root or a clustered table
      root.
- [ ] `CREATE TABLE` chooses the clustered path when the table definition has
      an explicit `PRIMARY KEY`.
- [ ] `CREATE TABLE` without explicit `PRIMARY KEY` stays on the heap path.
- [ ] Clustered table creation allocates and initializes an empty clustered leaf
      root page.
- [ ] Clustered table creation persists primary-index catalog metadata so later
      subphases can recover PK column order without heap assumptions.
- [ ] Heap-only runtime paths fail explicitly on clustered tables instead of
      reading or writing clustered roots as heap pages.
- [ ] Targeted unit/integration coverage proves clustered table creation,
      legacy heap compatibility, and guard-rail errors on pre-`39.14` clustered
      DML attempts.

## Out of scope

- executor-visible clustered `INSERT`
- executor-visible clustered `SELECT`
- executor-visible clustered `UPDATE`
- executor-visible clustered `DELETE`
- hidden clustered keys for tables without explicit `PRIMARY KEY`
- heap-to-clustered table rebuild/migration
- clustered VACUUM / purge
- clustered root persistence beyond the current catalog/WAL boundary introduced
  by later DML work
- CREATE INDEX over populated clustered tables

## Dependencies

- `specs/fase-39/spec-39.3-clustered-btree-insert.md`
- `specs/fase-39/spec-39.4-clustered-btree-point-lookup.md`
- `specs/fase-39/spec-39.5-clustered-btree-range-scan.md`
- `specs/fase-39/spec-39.9-secondary-index-pk-bookmarks.md`
- `specs/fase-39/spec-39.11-clustered-wal-support.md`
- `specs/fase-39/spec-39.12-clustered-crash-recovery.md`
- `crates/axiomdb-catalog/src/schema.rs`
- `crates/axiomdb-catalog/src/writer.rs`
- `crates/axiomdb-sql/src/executor/ddl.rs`
- `crates/axiomdb-sql/src/table.rs`

## Research citations

- `research/sqlite/src/build.c`
  - `WITHOUT ROWID` converts the table to primary-key physical identity and
    rejects the mode when no explicit primary key exists.
- `research/sqlite/src/build.c`
  - secondary uniqueness metadata is rewritten to hang off the true primary key
    instead of a hidden rowid tail.
- `research/mariadb-server/storage/innobase/row/row0mysql.cc`
  - when InnoDB auto-generates the clustered key, MySQL itself does not know
    the hidden row id well enough to reconstruct row references cleanly.
- `research/mariadb-server/storage/innobase/include/dict0dict.inl`
  - InnoDB internally appends row-id state to clustered ordering fields when it
    must synthesize uniqueness, which is exactly the hidden-key path we do not
    want to adopt prematurely in `39.13`.

## ⚠️ DEFERRED

- clustered executor-visible `INSERT` / `SELECT` / `UPDATE` / `DELETE`
  integration → pending in `39.14` through `39.17`
- hidden clustered key support for tables without explicit `PRIMARY KEY`
  → revisit after the explicit-PK clustered path is stable
- CREATE INDEX / rebuild flows over populated clustered tables
  → revisit with later clustered executor/index integration
- snapshot-visible older-version reconstruction on clustered reads
  → revisit in later clustered MVCC work
