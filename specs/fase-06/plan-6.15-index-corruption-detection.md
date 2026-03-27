# Plan: 6.15 — Index corruption detection

## Files to create / modify

### Create
- `crates/axiomdb-sql/src/index_integrity.rs` — startup verifier and repair orchestration for index-vs-heap divergence
- `crates/axiomdb-sql/tests/integration_index_integrity.rs` — end-to-end divergence detection / rebuild tests

### Modify
- `crates/axiomdb-sql/src/lib.rs` — export the new index integrity module
- `crates/axiomdb-sql/src/executor/ddl.rs` — extract reusable “build index root from heap rows” helper from CREATE INDEX path
- `crates/axiomdb-network/src/mysql/database.rs` — run startup integrity verification after WAL recovery and before serving traffic
- `crates/axiomdb-embedded/src/lib.rs` — run the same startup integrity verification in embedded open
- `crates/axiomdb-core/src/error.rs` — add a dedicated startup integrity/rebuild failure error if current errors are too generic
- `crates/axiomdb-sql/tests/integration_indexes.rs` or sibling test files — reuse existing index fixtures if helpful

## Algorithm / Data structure

### 1. Startup verification entrypoint

Add one SQL-layer helper:

```rust
verify_and_repair_indexes_on_open(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<IndexIntegrityReport, DbError>
```

Rules:
- call after `MmapStorage::open(...)` checksum verification
- call after `TxnManager::open_with_recovery(...)`
- run before the server/embedded API exposes the database to callers
- use `txn.snapshot()` for a fully committed visibility view

### 2. Per-index verification model

For every visible table:
1. load `TableDef`, `ColumnDef`, and `IndexDef`s from the catalog
2. scan all heap-visible rows once with `TableEngine::scan_table(...)`
3. for each index, derive the **expected** entry list from those rows
4. enumerate the **actual** entry list with `BTree::range_in(storage, root, None, None)`
5. compare `expected == actual`

If the lists match, the index is clean.
If the lists differ, mark the index divergent and schedule rebuild.
If enumerating actual entries fails with `BTreeCorrupted` or equivalent
structural read failure, abort open with an integrity error.

### 3. Expected-entry encoding

The verifier must reuse exactly the same key semantics as runtime maintenance:

```text
primary/unique index:
  key = encode_index_key(indexed_columns)
  value/rid = row RecordId

non-unique secondary index:
  key = encode_index_key(indexed_columns) || encode_rid(rid)
  value/rid = row RecordId

FK auto-index:
  key = encode_index_key(indexed_columns) || encode_rid(rid)
  value/rid = row RecordId

partial index:
  include only rows whose compiled predicate is truthy

NULL handling:
  if any indexed key column is NULL, skip the row
```

Expected entries should be represented as:

```rust
struct ExpectedEntry {
    key: Vec<u8>,
    rid: RecordId,
}
```

Sort `expected` by `(key, rid)` before comparison.
Build `actual` from `range_in(...)` as the same shape and compare vectors
directly.

### 4. Rebuild primitive

Do not build repair by emitting SQL `CREATE INDEX`.
Instead extract a reusable helper from `execute_create_index(...)`:

```rust
build_index_root_from_heap(
    storage: &mut dyn StorageEngine,
    table_def: &TableDef,
    col_defs: &[ColumnDef],
    index_def: &IndexDef,
    snapshot: TransactionSnapshot,
) -> Result<u64, DbError>
```

Behavior:
- allocate a fresh empty B+Tree root
- scan heap-visible rows
- compute index entries with the same rules as normal maintenance
- insert them into the new tree
- return the final root page ID

This helper must support:
- PK indexes
- unique secondary indexes
- non-unique secondary indexes
- FK auto-indexes
- partial indexes
- configured fillfactor

### 5. Safe root swap + old page reclamation

For each divergent index:
1. build a fresh root with `build_index_root_from_heap(...)`
2. collect the old B+Tree pages with the existing page-collection logic used by bulk-empty/root-rotation flows
3. inside a startup transaction:
   - `CatalogWriter::update_index_root(index_id, new_root)`
   - `txn.defer_free_pages(old_pages)`
4. `txn.commit()`
5. `txn.release_immediate_committed_frees(...)`

This preserves crash safety: old pages are not freed before the catalog root
swap is durable.

### 6. Structural corruption policy

Policy for this subphase:
- **Logical divergence** (heap and index disagree, but index is readable):
  rebuild automatically
- **Unreadable/corrupted tree** (`range_in`/page walk cannot enumerate entries):
  fail open

Reason:
- page checksum verification already catches torn/partial physical corruption
- unreadable trees cannot be safely reclaimed or trusted for page enumeration
- automatic rebuild is reserved for the “heap is sane, index contents drifted”
  class of problems

## Implementation phases

1. Extract reusable index-build helper from `execute_create_index(...)`.
2. Add `index_integrity.rs` with:
   - table scan orchestration
   - expected-entry derivation
   - actual-entry enumeration
   - divergence comparison
   - rebuild scheduling
3. Reuse / expose B+Tree page-collection helper for deferred reclamation of old index pages.
4. Wire `verify_and_repair_indexes_on_open(...)` into:
   - `Database::open(...)`
   - `Db::open(...)`
5. Add startup regression tests for:
   - clean open
   - missing index entry
   - extra stale index entry
   - partial index divergence
   - unreadable/corrupted tree fails open

## Tests to write

- unit:
  - expected-entry encoding matches runtime maintenance for PK, unique, non-unique, and FK indexes
  - partial-index predicate filtering during verification
  - direct vector comparison catches missing/extra entries deterministically

- integration:
  - create table + index + rows, manually remove one B+Tree entry, reopen DB, verify startup rebuild restores indexed lookups
  - create table + partial index, mutate data/index to create divergence, reopen DB, verify rebuild honors predicate
  - create table + FK/PK indexes, reopen DB after divergence, verify FK/PK behavior still works after rebuild
  - corrupt index structure so enumeration fails, reopen DB, assert open returns integrity error
  - verify both server-side `Database::open(...)` and embedded `Db::open(...)` run the same startup checker

- bench:
  - optional targeted startup benchmark: small/medium catalog with multiple indexes, measure verification time on open
  - no full performance benchmark required for normal query paths in this subphase

## Anti-patterns to avoid

- Do not add SQL `REINDEX` syntax in this subphase.
- Do not “repair” indexes opportunistically during normal SELECT/INSERT/UPDATE/DELETE.
- Do not compare only key counts; compare exact `(key, rid)` entry sets.
- Do not free old index pages eagerly before the root swap commit is durable.
- Do not trust runtime bloom filters for verification; rebuild from heap and B+Tree only.
- Do not treat the index as source of truth when it disagrees with the heap.

## Risks

- **Risk:** rebuild logic diverges from CREATE INDEX logic and produces subtly different keys.
  **Mitigation:** extract one shared helper instead of duplicating build rules.

- **Risk:** partial indexes or FK auto-indexes are rebuilt with the wrong predicate/key encoding.
  **Mitigation:** route verification through the same predicate compilation and key builders used by runtime maintenance.

- **Risk:** rebuilding an index and freeing old pages in the same startup path can break crash safety.
  **Mitigation:** reuse `txn.defer_free_pages(...)` + post-commit release, exactly like bulk root-rotation flows.

- **Risk:** structural corruption in the old index prevents safe enumeration of old pages for cleanup.
  **Mitigation:** fail open for unreadable trees in `6.15`; only auto-rebuild readable divergent indexes.

- **Risk:** startup time grows with table size because verification is full and exact.
  **Mitigation:** accept the O(table + index) cost in this phase; optimize later only if real workloads demand it.

## Assumptions

- Startup has exclusive writer ownership, so rebuild does not need concurrency control.
- Heap pages that pass existing page checksum and row decode checks are trusted as source of truth for this phase.
- There is no persisted bloom filter state to repair during open.
