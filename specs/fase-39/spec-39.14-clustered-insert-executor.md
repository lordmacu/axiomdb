# Spec: Phase 39.14 Clustered INSERT Executor Path

## What to build (not how)
Build the first executor-visible `INSERT` path for clustered tables.

For any table whose `TableDef.storage_layout` is `Clustered`, `INSERT` must no
longer fail with the Phase 39.14 guard rail. Instead, the executor must:

- coerce and encode the SQL row into the clustered row codec payload
- derive the clustered primary-key bytes from the primary index metadata order
- reject `NULL` in any primary-key column
- assign `AUTO_INCREMENT` values using the current table contents
- write the row through the clustered B-tree, not the heap chain
- treat a physically-present but snapshot-invisible clustered PK as a reusable
  deleted version, not as a SQL-visible duplicate
- record clustered WAL/undo via:
  - `TxnManager::record_clustered_insert(...)` for fresh clustered keys
  - `TxnManager::record_clustered_update(...)` when reusing a
    snapshot-invisible delete-marked physical key so rollback can restore the
    old tombstone image exactly
- maintain every non-primary index using clustered PK bookmarks, not heap `RecordId`
- persist any changed clustered table root and secondary index roots in the
  catalog inside the statement transaction

This subphase must support the SQL shapes that the current heap insert path
already supports:

- `INSERT ... VALUES (...)`
- multi-row `INSERT ... VALUES (...), (...)`
- `INSERT ... SELECT ...`
- autocommit execution
- explicit transactions through `execute_with_ctx(...)`

The implementation must remain clustered-first. It must not reintroduce heap
storage, heap `RecordId`, or the old staged heap batch as the source of truth
for clustered rows.

## Inputs / Outputs
- Input:
  - analyzed `InsertStmt`
  - resolved clustered `TableDef`
  - resolved clustered primary `IndexDef`
  - resolved non-primary `IndexDef`s for the same table
  - active `TxnManager`
  - `SessionContext` when running through the ctx executor
- Output:
  - `QueryResult::Affected { count, last_insert_id }` with the same public
    semantics as heap insert
  - durable clustered row state visible to later clustered executor phases via:
    - latest table `root_page_id`
    - clustered WAL entry
    - clustered secondary bookmark entries
- Errors:
  - `DbError::NotNullViolation` when any clustered PK column is `NULL`
  - `DbError::UniqueViolation` when a visible clustered PK already exists
  - `DbError::NoActiveTransaction` if the write path is called outside an
    active transaction
  - row coercion / encoding / FK / CHECK / storage / WAL errors from existing
    executor components

## Use cases
1. `INSERT INTO users VALUES (1, 'alice')` on
   `CREATE TABLE users(id INT PRIMARY KEY, name TEXT)` writes one clustered row
   into the table root and returns `Affected { count: 1, ... }`.
2. Inserting many rows into a clustered table causes clustered leaf/internal
   splits; the final root is persisted back to `axiom_tables`.
3. `INSERT` into a clustered table with a non-primary secondary index writes the
   secondary entry using the `secondary_key ++ pk_suffix` bookmark format from
   Phase 39.9.
4. `INSERT` into a clustered table inside an explicit transaction succeeds
   without using `SessionContext::pending_inserts`, and a statement rollback or
   savepoint rollback removes both the clustered row and secondary entries.
5. `INSERT INTO users VALUES (NULL, 'alice')` on a clustered PK returns
   `DbError::NotNullViolation`.
6. `INSERT INTO users VALUES (1, 'alice')` followed by another visible insert
   for PK `1` returns `DbError::UniqueViolation` on the primary index.
7. `INSERT INTO dst SELECT ... FROM src` inserts each output row into clustered
   storage and preserves `LAST_INSERT_ID()` semantics for auto-generated values.
8. If clustered storage already contains the same PK only as a snapshot-invisible
   delete-marked physical record, `INSERT` reuses/restores that physical key
   instead of surfacing `DbError::DuplicateKey` as a SQL-visible duplicate.

## Acceptance criteria
- [ ] `INSERT` into a clustered table no longer returns the Phase 39.14
      `NotImplemented` guard rail in either ctx or non-ctx executor paths.
- [ ] Clustered insert derives primary-key bytes from the clustered primary
      index metadata order using `encode_index_key(...)`.
- [ ] `NULL` in any clustered PK column is rejected with
      `DbError::NotNullViolation`.
- [ ] `AUTO_INCREMENT` on clustered targets works with the same user-visible
      semantics as the heap path, including `LAST_INSERT_ID()`.
- [ ] Clustered rows are stored through `axiomdb_storage::clustered_tree`,
      never through `HeapChain`.
- [ ] A snapshot-invisible physical duplicate in clustered storage is resolved
      through exact-row restore/reuse rather than surfacing as a visible PK
      conflict.
- [ ] Successful clustered inserts append clustered WAL/undo records:
      `ClusteredInsert` for fresh keys and `ClusteredUpdate` when a
      snapshot-invisible delete-marked physical key is being reused.
- [ ] Every non-primary index on a clustered table is maintained through
      `clustered_secondary`, not through heap `RecordId` payloads.
- [ ] Any clustered table root change is persisted through
      `CatalogWriter::update_table_root(...)`.
- [ ] Any secondary root change is persisted through
      `CatalogWriter::update_index_root(...)`.
- [ ] Explicit-transaction clustered inserts are executed immediately and remain
      statement-savepoint-safe without entering `pending_inserts`.
- [ ] Integration tests cover:
      - clustered PK insert
      - duplicate clustered PK rejection
      - clustered `AUTO_INCREMENT`
      - clustered secondary maintenance
      - clustered root persistence after splits
      - rollback / savepoint rollback of clustered insert

## Out of scope
- executor-visible clustered `SELECT` / `UPDATE` / `DELETE` (`39.15`–`39.17`)
- `CREATE INDEX`, `ANALYZE`, or `VACUUM` on clustered tables
- heap-to-clustered table rebuild (`39.19`)
- clustered bulk-load optimization or cross-statement staged batching
- planner cost changes for clustered access paths

## Dependencies
- `specs/fase-39/spec-39.3-clustered-btree-insert.md`
- `specs/fase-39/spec-39.9-secondary-index-pk-bookmarks.md`
- `specs/fase-39/spec-39.11-clustered-wal-support.md`
- `specs/fase-39/spec-39.12-clustered-crash-recovery.md`
- `specs/fase-39/spec-39.13-clustered-create-table.md`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-sql/src/clustered_secondary.rs`
- `crates/axiomdb-wal/src/txn.rs`

## ⚠️ DEFERRED
- executor-visible clustered `SELECT` and planner access-path selection →
  pending in `39.15`
- executor-visible clustered `UPDATE` / `DELETE` →
  pending in `39.16` / `39.17`
- public clustered purge / VACUUM of delete-marked rows →
  pending in `39.18`
- older-snapshot reconstruction when a delete-marked clustered PK is reused by
  `INSERT` → pending in later clustered MVCC / version-chain work
- final SQL-path insert benchmarks against MySQL/PostgreSQL →
  report when `39.14` implementation is closed and the benchmark surface exists
