# Spec: 5.19 — B+Tree batch delete (sorted single-pass)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-sql/src/executor.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-index/src/page_layout.rs`
- `crates/axiomdb-index/src/lib.rs`
- `crates/axiomdb-index/tests/integration_btree.rs`
- `crates/axiomdb-sql/tests/integration_executor.rs`
- `benches/comparison/local_bench.py`
- `specs/fase-05/spec-5.17-in-place-btree-write-path.md`
- `specs/fase-05/spec-5.18-heap-insert-tail-pointer-cache.md`
- `specs/fase-06/spec-6.1-6.3-secondary-indexes.md`
- `specs/fase-06/spec-6.3b-indexed-delete-where.md`

These research files were reviewed before writing this spec:

- `research/sqlite/src/delete.c`
- `research/postgres/src/backend/access/nbtree/nbtree.c`
- `research/mariadb-server/storage/innobase/include/btr0bulk.h`
- `research/mariadb-server/storage/innobase/include/row0merge.h`
- `research/mariadb-server/storage/innobase/include/ibuf0ibuf.h`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`

## Research synthesis

### AxiomDB-first constraints

- After `6.3b`, `DELETE ... WHERE` already stops doing full heap scans when the
  predicate is indexable. The remaining bottleneck is index maintenance, not
  candidate discovery.
- `execute_delete_ctx(...)` and `execute_delete(...)` already batch heap
  deletion with `TableEngine::delete_rows_batch(...)`, but they still call
  `delete_from_indexes(...)` once per row, and that helper still calls
  `BTree::delete_in(...)` once per index key.
- `execute_update_ctx(...)` and `execute_update(...)` still delete old index
  keys and insert new ones row by row. Even when only a PRIMARY KEY exists, the
  index entry must be rewritten because the heap `RecordId` changes.
- `5.17` already removed part of the B+Tree page churn for individual writes,
  but it did not change the fact that a statement touching `N` rows still
  performs `N` separate root descents and `N` root-sync opportunities per
  affected index.
- Phase 5 is still effectively single-writer. `5.19` does not need to solve
  concurrent index mutation, snapshot-visible secondary-index versions, or
  background merge workers.

### What we borrow

- `research/sqlite/src/delete.c`
  - borrow: first decide exactly which row identities will be deleted, then run
    the physical mutation path
  - adapt: AxiomDB already does this after `6.3b`; `5.19` applies the same
    staging idea one layer lower, inside index maintenance
- `research/postgres/src/backend/access/nbtree/nbtree.c`
  - borrow: accumulate page-local deletions and apply them in one operation per
    page; PostgreSQL VACUUM explicitly prefers one delete call per page to
    minimize WAL traffic and page churn
  - adapt: AxiomDB will use one batch delete pass per affected index and one
    page rewrite per touched node whenever possible
- `research/mariadb-server/storage/innobase/include/btr0bulk.h`
  - borrow: ordered page-local work should be handled by a dedicated bulk path,
    not by reusing the point-mutation path in a loop
  - adapt: `5.19` adds a dedicated sorted delete path instead of looping
    `delete_in(...)`
- `research/mariadb-server/storage/innobase/include/row0merge.h`
  - borrow: sort/merge style index work is its own algorithmic mode
  - adapt: AxiomDB will sort exact encoded keys per index before mutating the
    tree
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
  - borrow: separate delete candidate production from the physical delete sink,
    and carry the columns/keys the sink really needs
  - adapt: AxiomDB will group exact encoded index keys per index before calling
    the B+Tree layer
- `research/oceanbase/src/sql/engine/dml/ob_table_delete_op.cpp`
  - borrow: keep DML staging explicit; production of delete intents is separate
    from writing to storage
  - adapt: AxiomDB will keep executor candidate collection unchanged and only
    change the index-maintenance stage
- `research/datafusion/datafusion/core/tests/custom_sources_cases/dml_planning.rs`
  - borrow: optimization intent should be explicit and testable
  - adapt: AxiomDB will add tests that prove DELETE/UPDATE use batch delete when
    multiple old index keys are removed in one statement

### What we reject

- folding `5.19` into `5.18`; heap tail caching and batched B+Tree delete are
  different bottlenecks in different layers
- reopening `6.3b`; candidate discovery is already a separate solved concern
- jumping directly to an InnoDB-style change buffer in this subphase
- keeping per-row catalog root updates when a statement is already mutating the
  same index in bulk
- weakening partial-index, FK-index, or UNIQUE semantics just to gain speed

### How AxiomDB adapts it

- `5.19` adds a new B+Tree primitive that removes many already-encoded keys from
  one index in ascending-key order.
- SQL index maintenance groups deletes per index, sorts exact encoded keys, and
  invokes the batch primitive once per affected index.
- `DELETE ... WHERE` uses that batch helper after candidate rows are already
  materialized.
- `UPDATE` uses the same batch helper only for the old-key delete side; new-key
  inserts stay on the existing `insert_in(...)` path in this subphase.
- For `UPDATE`, each index root is persisted once with its final post-delete
  and post-insert root page ID, not once per row.

## What to build (not how)

Implement batched sorted index-key deletion so AxiomDB stops doing one
`BTree::delete_in(...)` traversal per deleted row during `DELETE ... WHERE` and
the old-key removal half of `UPDATE`.

### Surface 1: new B+Tree batch delete primitive

The B+Tree layer must gain a new batch delete primitive that:

- accepts exact encoded keys in ascending order
- removes all matching keys from one index in one batch mutation
- updates the root atomically once at the end of the batch
- preserves current lookup, range, and delete correctness

This primitive is about exact key deletion, not predicate evaluation.

### Surface 2: per-index key grouping in SQL index maintenance

Index maintenance must stop deleting keys row by row.

Instead, for one statement:

- collect the exact encoded delete keys per affected index
- preserve all current rules when deciding whether a row contributes a key:
  - partial index predicate gating
  - `NULL` values not indexed
  - unique vs non-unique vs FK key encoding rules
- sort the collected keys ascending for that index
- call the new B+Tree batch delete primitive once for that index

### Surface 3: DELETE integration

For `DELETE ... WHERE` and any other DELETE path that already has an explicit
`to_delete: Vec<(RecordId, Vec<Value>)>`:

- heap deletion stays batched exactly as today
- index maintenance changes from per-row delete calls to one grouped batch per
  index
- bloom/filter dirtying and root updates remain correct

### Surface 4: UPDATE integration

For `UPDATE`, this subphase only optimizes the delete side of index
maintenance.

That means:

- old index keys are grouped and batch-deleted per index
- new index keys are still inserted with the existing per-row insert path
- when the same unique key is logically preserved across an UPDATE, the old key
  must be removed before the new key is inserted so reinsertion of the same key
  remains valid

### Surface 5: root persistence contract

When a statement removes many keys from one index:

- the index root must no longer be persisted row by row
- the final root page ID must be persisted once per affected index

For UPDATE:

- the final persisted root is the root after the batch delete phase and the
  subsequent insert phase

## Inputs / Outputs

- Input:
  - `DELETE` or `UPDATE` statement execution paths that already have resolved
    rows and index metadata
  - `IndexDef` metadata including primary, unique, non-unique, FK, and partial
    indexes
  - exact encoded index keys derived from row values and `RecordId`
  - mutable storage, active transaction, and current index root page IDs
- Output:
  - unchanged SQL result types:
    - `QueryResult::Affected { count, last_insert_id: None }`
    - unchanged row counts and statement visibility semantics
  - fewer B+Tree traversals and fewer per-row root updates during index delete
    maintenance
- Errors:
  - unchanged storage/index/catalog errors from current paths
  - unchanged UNIQUE/FK/partial-index correctness
  - no new SQL syntax, wire behavior, or user-visible error classes

## Use cases

1. `DELETE FROM bench_users WHERE id > 2500`
   On a table with only `PRIMARY KEY (id)`, AxiomDB batch-deletes the old PK
   entries instead of doing one `delete_in(...)` call per row.

2. `UPDATE bench_users SET score = score + 1 WHERE active = TRUE`
   Even though `score` is not part of the PK, the old PK entries must be
   removed because the heap `RecordId`s change. `5.19` batch-deletes those old
   PK keys before reinserting the new ones.

3. `DELETE FROM orders WHERE user_id = 42`
   On a table with a non-unique FK auto-index `user_id||RecordId`, AxiomDB
   batches deletion of the exact composite keys, not just the logical FK value.

4. `UPDATE users SET deleted_at = NOW() WHERE deleted_at IS NULL`
   For a partial unique index on `(email) WHERE deleted_at IS NULL`, rows that
   previously matched the predicate contribute delete keys; rows that no longer
   match do not contribute insert keys.

5. `DELETE FROM sessions WHERE expires_at < NOW()`
   If no usable index exists, candidate discovery behavior stays unchanged; if
   candidate rows are already collected, `5.19` still batch-deletes index keys
   from whatever indexes exist on the table.

## Acceptance criteria

- [ ] A new B+Tree batch delete primitive exists for exact encoded keys and is
      used by SQL index maintenance when more than one key is removed from the
      same index in one statement.
- [ ] `DELETE ... WHERE` no longer performs one `BTree::delete_in(...)` call
      per deleted row for each affected index.
- [ ] `UPDATE` no longer performs one old-key `BTree::delete_in(...)` call per
      updated row for each affected index.
- [ ] The batch delete path preserves current key encoding semantics for:
      primary, unique, non-unique, and FK auto-indexes.
- [ ] Partial indexes contribute delete keys only for rows whose old values
      satisfied the stored predicate.
- [ ] `NULL` key values remain skipped exactly as today.
- [ ] For UPDATE, old keys are batch-deleted before new keys are reinserted, so
      same-key replacements on UNIQUE/PRIMARY indexes remain valid.
- [ ] The final root page ID of an affected index is persisted once per index
      batch, not once per deleted row.
- [ ] Bloom/filter dirtying remains correct after batched deletions.
- [ ] `DELETE FROM bench_users WHERE id > N/2` on the benchmark schema uses the
      new batch delete path on the PRIMARY KEY index.
- [ ] `UPDATE bench_users SET score = score + 1 WHERE active = TRUE` uses the
      batch delete path for old PK keys before reinserting the new PK entries.

## Out of scope

- batch insert / `insert_many_in(...)`
- InnoDB-style change buffer or deferred background index merge
- reworking DELETE candidate discovery (`6.3b` already covers that)
- full-table `DELETE FROM t` bulk-empty path (`5.16` already covers that)
- concurrent writer coordination or MVCC-visible secondary-index versions
- new SQL syntax or protocol behavior

## Dependencies

- `5.16` bulk-empty DELETE path
- `5.17` in-place B+Tree write path expansion
- `6.2b` current index maintenance on INSERT/UPDATE/DELETE
- `6.3b` indexed DELETE WHERE candidate discovery for the benchmarked path
- `6.7` partial index predicate gating
- `6.9` PK/FK index encoding and maintenance rules

## ⚠️ DEFERRED

- batch insert side for UPDATE and other DML → pending in a future index-write
  batching subphase
- change-buffer / deferred index maintenance → future design, not part of
  `5.19`
- predicate-to-range leaf pruning without explicit key collection → future
  optimizer/storage subphase
