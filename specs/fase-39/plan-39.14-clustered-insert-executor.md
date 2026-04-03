# Plan: Phase 39.14 Clustered INSERT Executor Path

## Files to create/modify
- `crates/axiomdb-sql/src/clustered_table.rs` — new executor-facing clustered
  row helpers for:
  - coercing/encoding one SQL row
  - extracting PK values/bytes from the primary index definition
  - validating clustered PK nullability
  - scanning current clustered rows for `AUTO_INCREMENT` initialization
- `crates/axiomdb-sql/src/lib.rs` — export the new clustered table helper module
- `crates/axiomdb-sql/src/executor/insert.rs` — route clustered tables through
  the new clustered insert pipeline in both ctx and non-ctx paths
- `crates/axiomdb-sql/src/executor/mod.rs` — only if needed to force staged heap
  inserts to flush before a clustered INSERT statement
- `crates/axiomdb-sql/src/clustered_secondary.rs` — extend helper surface if the
  executor needs explicit tracked insert/root-return APIs
- `crates/axiomdb-sql/src/executor/ddl.rs` — optional but preferred: mark PK
  columns `nullable = false` for newly-created clustered tables so catalog
  metadata matches SQL semantics
- `crates/axiomdb-sql/tests/integration_clustered_insert.rs` — new SQL-visible
  integration coverage for clustered insert semantics
- `docs/fase-39.md` — close out `39.14`
- `docs/progreso.md` — mark `39.14` complete and record any real deferred gaps
- `docs-site/src/user-guide/features/indexes.md` — document SQL-visible
  clustered insert behavior and its current limits
- `docs-site/src/user-guide/features/transactions.md` — document clustered
  insert WAL / rollback semantics from the user view
- `docs-site/src/internals/architecture.md` — update executor flow to show the
  clustered insert branch
- `docs-site/src/internals/storage.md` — explain SQL-visible writes into the
  clustered tree
- `docs-site/src/internals/wal.md` — document `ClusteredInsert` as now used by
  executor-visible SQL writes
- `docs-site/src/development/roadmap.md` — advance current Phase 39 status
- `memory/project_state.md` — record `39.14`
- `memory/architecture.md` — capture the executor/storage boundary
- `memory/lessons.md` — record any new clustered executor lessons

## Algorithm / Data structure
Chosen approach: a dedicated clustered executor insert pipeline, not a heap path
with branches.

Research anchors:
- `research/sqlite/src/insert.c`
  - WITHOUT ROWID writes target the primary-key b-tree directly instead of a
    heap + separate PK index.
- `research/sqlite/src/build.c`
  - secondary entries append the table key, not a physical location, when the
    table storage itself is keyed by the PK.
  - PRIMARY KEY columns are forced to `NOT NULL` when the table root becomes
    the PK b-tree.
- `research/mariadb-server/storage/innobase/row/row0ins.cc`
  - clustered inserts treat the clustered key as the storage identity and keep
    secondary maintenance separate from the clustered record write.
  - a delete-marked clustered record is not treated as an immutable duplicate;
    insert can reuse or rehabilitate that clustered key instead.

### Statement-scoped clustered insert controller
For a clustered target table:

```text
resolve clustered table + indexes
flush pending heap batch first if ctx has one
find clustered primary index metadata
prepare mutable current_table_root = txn.clustered_root(table_id) or table_def.root_page_id
prepare mutable secondary roots from resolved indexes

for each source row:
  evaluate expressions into full row values
  assign AUTO_INCREMENT if needed
  check CHECK constraints
  validate FK child references

  coerced_values = clustered_table::coerce_row(...)
  pk_values = clustered_table::collect_pk_values(coerced_values, primary_idx)
  reject NULL PK columns
  pk_key = encode_index_key(pk_values)
  row_bytes = encode_row(coerced_values)

  visible_existing = clustered_tree::lookup(storage, Some(current_table_root), pk_key, snapshot)
  if visible_existing.is_some():
    return UniqueViolation(primary_idx.name)

  row_header = RowHeader {
    txn_id_created: active_txn_id,
    txn_id_deleted: 0,
    row_version: 0,
    _flags: 0,
  }

  new_table_root =
    try clustered_tree::insert(...)
    if DuplicateKey and visible_existing == None:
      // only a physical invisible version remains
      // mirror InnoDB clustered-key reuse against delete-marked rows
      clustered_tree::restore_exact_row_image(...)

  if reused invisible physical duplicate:
    txn.record_clustered_update(
      table_id,
      pk_key,
      old_clustered_row_image,
      ClusteredRowImage::new(new_table_root, row_header, row_bytes)
    )
  else:
    txn.record_clustered_insert(
      table_id,
      pk_key,
      ClusteredRowImage::new(new_table_root, row_header, row_bytes)
    )
  current_table_root = new_table_root

  for each non-primary index:
    layout = ClusteredSecondaryLayout::derive(index, primary_idx)
    entry = layout.entry_from_row(coerced_values)
    if entry exists:
      insert via layout / BTree atomic root
      record UndoIndexInsert with the physical key and the post-insert root
      update mutable root if split happened

persist current_table_root once at end of statement if changed
persist each changed secondary root once at end of statement
return affected count / last_insert_id
```

### Key decisions
1. Do not enqueue clustered rows in `SessionContext::pending_inserts`.
   That staging layer is heap-batch-specific and keyed by `RecordId` outcomes.
   Clustered INSERTs should execute immediately inside the statement savepoint.
2. Use the primary index metadata as the source of truth for PK column order.
   Never infer clustered PK order from table column order alone.
3. Keep clustered roots statement-local while rows are being inserted, then
   persist the final roots once per statement.
4. Record index undo against the root that exists **after** the insert, not the
   pre-split root, so rollback can start from a valid search root.
5. Runtime PK null rejection is mandatory even if older catalog rows still say
   the PK column is nullable.
6. If clustered storage reports `DuplicateKey` but snapshot lookup found no
   visible row, prefer exact-row restore/reuse over rejecting the SQL insert.
   This matches clustered-key semantics better than treating tombstones as
   permanent duplicates, but rollback must then use `record_clustered_update(...)`
   so the old tombstone image can be restored exactly.

## Implementation phases
1. Add `clustered_table` helpers for row coercion/encoding, PK extraction, PK
   null validation, and clustered `AUTO_INCREMENT` scanning.
2. Extend `executor/insert.rs` with a clustered branch shared by ctx and
   non-ctx insert flows.
3. Flush any pending heap batch before a clustered INSERT executes in ctx mode.
4. Insert clustered base rows through `clustered_tree::insert(...)`, using
   `record_clustered_insert(...)` for fresh keys and
   `record_clustered_update(...)` when a delete-marked physical duplicate is
   being reused, while tracking the latest root locally.
5. Maintain non-primary indexes through `ClusteredSecondaryLayout`, recording
   index undo with post-insert roots and persisting changed roots once.
6. Update clustered CREATE TABLE metadata so PK columns are stored as
   `nullable = false` for new tables.
7. Add integration coverage for clustered `VALUES`, multi-row `VALUES`,
   `INSERT ... SELECT`, auto-increment, duplicate PK, root splits, and
   statement rollback/savepoint rollback.
8. Close docs/memory/progress only after the targeted validation passes.

## Tests to write
- unit:
  - PK extraction follows primary index column order, not raw table order
  - clustered PK null validation returns `NotNullViolation`
  - clustered `AUTO_INCREMENT` bootstrap over existing clustered rows
- integration:
  - `INSERT INTO clustered_table VALUES (...)` inserts rows visible through
    direct clustered storage inspection
  - duplicate clustered PK fails with `UniqueViolation`
  - clustered insert updates `axiom_tables.root_page_id` after enough inserts to split
  - clustered insert maintains secondary bookmark indexes correctly
  - clustered insert reuses a delete-marked physical PK instead of surfacing a
    false duplicate
  - explicit transaction + statement failure rolls back clustered base row and
    clustered secondary entries
  - `ROLLBACK TO SAVEPOINT` removes clustered rows inserted after the savepoint
  - `INSERT ... SELECT` into a clustered table succeeds
  - new `CREATE TABLE ... PRIMARY KEY` metadata reports PK columns as non-null
- bench:
  - defer formal `cargo bench` comparison table to the `review-task` closeout,
    but keep `39.14` insert loops structured so a clustered insert benchmark can
    be added without refactoring the API again

## Anti-patterns to avoid
- Do not call `TableEngine::insert_row*` from the clustered branch.
- Do not reuse `index_maintenance::insert_into_indexes_with_undo(...)` for
  clustered secondaries; it encodes `RecordId`-based payloads and assumes heap semantics.
- Do not put clustered rows into `SessionContext::pending_inserts`.
- Do not infer clustered roots only from catalog state once a transaction is active;
  always consult `txn.clustered_root(table_id)` first.
- Do not rely on `clustered_tree::insert(...)` duplicate detection alone; it sees
  physical duplicates, while SQL uniqueness is snapshot-visible.
- Do not persist table/index roots after every row if a statement-local final
  root is enough.

## Risks
- Root tracking mismatch between executor locals, `TxnManager`, and catalog.
  Mitigation: one helper owns the statement-local root map and persists only the final values.
- `AUTO_INCREMENT` still needs a full clustered scan to bootstrap `MAX(...)`.
  Mitigation: keep behavior consistent with the current heap path now; optimize later.
- Existing catalog rows created before the PK-nullability fix may still mark PK
  columns nullable.
  Mitigation: clustered insert validates PK nullability from primary index metadata at runtime.
- `clustered_tree::insert(...)` currently surfaces physical duplicates before it
  knows whether they are logically visible.
  Mitigation: executor pre-checks visibility with `lookup(...)` and falls back to
  `restore_exact_row_image(...)` when only an invisible deleted version remains.
- Reusing a delete-marked clustered PK rewrites the current physical version and
  therefore still depends on later clustered version-chain work for older
  snapshots to reconstruct the superseded tombstone image.
  Mitigation: document the MVCC limit explicitly in `39.14` instead of pretending
  the executor solved clustered historical reads.
- Reusing heap staging logic by accident could leave clustered inserts invisible
  to savepoint rollback or dependent statements.
  Mitigation: clustered branch flushes staged heap rows first and then bypasses staging completely.
