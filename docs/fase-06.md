# Phase 6 — Secondary Indexes + Query Planner

## Subfases completed in this session: 6.1, 6.1b, 6.2, 6.2b, 6.3, 6.15

## What was built

### 6.1 — Columns in IndexDef

`IndexDef` in `axiomdb-catalog` now stores `columns: Vec<IndexColumnDef>`, recording
which column positions (`col_idx: u16`) and sort directions (`SortOrder`) each index
covers. The on-disk format is extended backward-compatibly: old rows that end before
the `ncols` byte are read as `columns: []` (treated as unusable by the planner).

New types in `axiomdb-catalog/src/schema.rs`:
- `SortOrder` — `Asc`/`Desc`, repr `u8`
- `IndexColumnDef { col_idx: u16, order: SortOrder }` — one column entry (3 bytes on disk)

### 6.1b — Order-preserving key encoding

`crates/axiomdb-sql/src/key_encoding.rs`:
- `encode_index_key(&[Value]) -> Result<Vec<u8>, DbError>` — encodes a multi-value
  key into bytes such that `encode(a) < encode(b)` iff `a < b` under SQL comparison.
- Handles all Value variants: NULL (sorts first), Bool, Int, BigInt, Real, Decimal,
  Date, Timestamp, Text (NUL-escaped), Bytes (NUL-escaped), Uuid.
- Keys exceeding 768 bytes return `DbError::IndexKeyTooLong`.

### 6.2 — CREATE INDEX executor

`execute_create_index` in the executor was rewritten to:
1. Check for duplicate index name on the table.
2. Build `IndexColumnDef` list from the `CreateIndexStmt.columns`.
3. Allocate and initialize a fresh B-Tree leaf root page.
4. Scan the entire table heap and insert existing rows into the B-Tree (skipping NULLs
   and rows with keys > 768 bytes with a warning).
5. Persist `IndexDef` with the final `root_page_id` (which may change after root splits).

`execute_drop_index` now calls `free_btree_pages` to walk and free all B-Tree pages,
preventing page leaks on DROP INDEX.

New B-Tree static API in `axiomdb-index`:
- `BTree::lookup_in(storage, root_pid, key)` — point lookup without owning storage
- `BTree::insert_in(storage, root_pid, key, rid)` — insert without owning storage
- `BTree::delete_in(storage, root_pid, key)` — delete without owning storage
- `BTree::range_in(storage, root_pid, lo, hi)` — range scan, returns `Vec<(RecordId, Vec<u8>)>`

New `CatalogWriter::update_index_root(index_id, new_root)` persists updated
`root_page_id` when a B-Tree root splits during DML.

### 6.2b — Index maintenance on DML

`crates/axiomdb-sql/src/index_maintenance.rs`:
- `indexes_for_table(table_id, storage, snapshot)` — reads catalog
- `insert_into_indexes(indexes, row, rid, storage)` — inserts key into all secondary
  non-primary indexes; checks UNIQUE constraint (skips NULL keys — NULL ≠ NULL in SQL)
- `delete_from_indexes(indexes, row, storage)` — deletes key from indexes (ignores
  missing keys); skips NULLs and over-length keys

Integrated into `execute_insert`, `execute_update`, `execute_delete`:
- Pre-load secondary indexes once before the row loop.
- After each heap mutation, call the appropriate maintenance function.
- Persist updated `root_page_id` values after B-Tree splits.
- For UPDATE: update in-memory `root_page_id` after delete before insert to avoid
  reading freed pages.

### 6.3 — Query planner

`crates/axiomdb-sql/src/planner.rs`:
- `AccessMethod` enum: `Scan`, `IndexLookup { index_def, key }`, `IndexRange { index_def, lo, hi }`
- `plan_select(where_clause, indexes, columns)` — matches:
  - Rule 1: `col = literal` or `literal = col` → `IndexLookup`
  - Rule 2: `col > lo AND col < hi` (or `>=`/`<=`) → `IndexRange`
  - Otherwise: `Scan`

Integrated into `execute_select` (single-table no-JOIN path):
- Loads indexes from catalog before the scan.
- Dispatches on `AccessMethod`:
  - `Scan` → existing `TableEngine::scan_table` (unchanged)
  - `IndexLookup` → `BTree::lookup_in` → `TableEngine::read_row`
  - `IndexRange` → `BTree::range_in` → `TableEngine::read_row` for each hit

New `TableEngine::read_row(storage, columns, rid)` reads a single heap row by RID.

## New error types

In `axiomdb-core/src/error.rs`:
- `IndexAlreadyExists { name, table }` — SQLSTATE 42P07
- `IndexKeyTooLong { key_len, max }` — SQLSTATE 54000

(UniqueViolation already existed)

## Tests

- 11 new integration tests in `crates/axiomdb-sql/tests/integration_indexes.rs`
- 4 new unit tests in `axiomdb-catalog/src/schema.rs` (roundtrip, old-format compat)
- 10 unit tests in `axiomdb-sql/src/key_encoding.rs`
- 6 unit tests in `axiomdb-sql/src/planner.rs`

Total: 1135 tests pass (was 1124 before this phase).

## Deferred items

- `⚠️` Composite index planner (> 1 column WHERE predicate) — encoding supported,
  planner deferred to subfase 6.8
- `⚠️` Bloom filter per index — deferred to 6.4
- `⚠️` MVCC on secondary indexes — deferred to 6.14
- `⚠️` Index statistics (NDV, row counts) — deferred to 6.10

## 6.15 — Index corruption detection

`6.15` adds a startup-time logical integrity pass for secondary and primary
indexes. The new verifier lives in
`crates/axiomdb-sql/src/index_integrity.rs` and runs immediately after WAL
recovery in both:

- `crates/axiomdb-network/src/mysql/database.rs`
- `crates/axiomdb-embedded/src/lib.rs`

### What it does

For every catalog-visible table and index:

1. scan heap-visible rows under the committed snapshot
2. derive the exact expected index entries using the same key encoding and
   partial-index predicate semantics as normal DML maintenance
3. enumerate the actual B+ Tree entries from the catalog root
4. compare expected vs actual

If the tree is readable but divergent, AxiomDB:

- rebuilds a fresh index root from heap contents
- flushes the rebuilt pages
- rotates the catalog root inside a WAL-protected transaction
- defers free of the old tree pages until commit durability is confirmed

If the tree cannot be traversed safely, open fails with
`DbError::IndexIntegrityFailure` and the database never starts serving traffic.

### Code changes

- `crates/axiomdb-sql/src/index_integrity.rs`
  - `verify_and_repair_indexes_on_open(...)`
  - `IndexIntegrityReport`
  - `RebuiltIndex`
- `crates/axiomdb-sql/src/executor/ddl.rs`
  - `build_index_root_from_heap(...)`
- `crates/axiomdb-sql/src/executor/bulk_empty.rs`
  - `collect_btree_pages(...)`
  - `free_btree_pages(...)`
- `crates/axiomdb-core/src/error.rs`
  - `DbError::IndexIntegrityFailure`
- `crates/axiomdb-sql/src/lib.rs`
  - exports the verifier API for server/embedded callers

### Tests

- `crates/axiomdb-sql/tests/integration_index_integrity.rs`
  - rebuild missing unique-index entries
  - rebuild partial-index divergence
  - fail open for unreadable roots
  - verify rebuilt indexes remain durable across reopen
- `crates/axiomdb-network/tests/integration_open_integrity.rs`
  - server open fails on unreadable index root
- `crates/axiomdb-embedded/tests/integration.rs`
  - embedded open fails on unreadable index root

### Important invariant discovered during implementation

The rebuilt B+ Tree pages are written directly into storage, not through WAL.
That means the correct ordering is:

1. build fresh tree pages
2. `storage.flush()`
3. commit catalog root swap

Without that ordering, WAL recovery could make the new root visible before the
rebuilt pages were durable.

### Deferred

- SQL `REINDEX` remains deferred to `19.15` / `37.44`
- broader user-facing integrity surfaces (`CHECK TABLE`, `PRAGMA integrity_check`
  style commands) remain deferred to later diagnostics phases
