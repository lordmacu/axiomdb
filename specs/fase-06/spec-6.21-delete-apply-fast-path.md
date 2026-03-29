# Spec: DELETE apply fast path (Phase 6.21)

## Reviewed first

These AxiomDB files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `docs/fase-05.md`
- `docs/fase-06.md`
- `benches/comparison/local_bench.py`
- `crates/axiomdb-sql/src/executor/delete.rs`
- `crates/axiomdb-sql/src/table.rs`
- `crates/axiomdb-sql/src/index_maintenance.rs`
- `crates/axiomdb-storage/src/heap_chain.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-index/tests/integration_btree.rs`
- `crates/axiomdb-sql/benches/sqlite_comparison.rs`
- `specs/fase-06/spec-6.3b-indexed-delete-where.md`
- `specs/fase-06/spec-6.20-update-apply-fast-path.md`

These research sources were reviewed for this spec:

- `research/sqlite/src/delete.c`
- `research/sqlite/src/update.c`
- `research/duckdb/src/planner/binder/statement/bind_delete.cpp`
- `research/duckdb/src/execution/operator/persistent/physical_delete.cpp`
- `research/mariadb-server/sql/sql_delete.cc`
- `research/postgres/src/backend/executor/nodeModifyTable.c`
- `research/postgres/src/backend/access/heap/heapam.c`

## Research synthesis

### SQLite

SQLite validates the core idea behind this subphase:

- it separates candidate discovery from delete apply when one-pass delete is
  not safe, storing rowids/primary keys first
- it computes an exact OLD-column mask for triggers/FK checks before populating
  OLD row images

The relevant lesson for AxiomDB is not the VDBE machinery; it is that DELETE
should only materialize the columns actually needed by its consumers.

### DuckDB

DuckDB validates sparse pass-through of only the columns needed by physical
delete:

- the binder pushes required columns into the scan
- the physical delete operator reuses those columns directly
- non-present columns are represented sparsely rather than re-fetched

The relevant lesson for AxiomDB is that masked candidate materialization is a
valid executor design, not an ad-hoc optimization.

### MariaDB

MariaDB validates the gating strategy:

- use a truncate-style fast path when side effects are absent
- delete while scanning only when it is safe
- otherwise, collect row identities first and apply deletes afterwards
- explicitly mark only the columns needed for delete

The relevant lesson for AxiomDB is to keep the current two-phase DELETE apply
shape and make the input to that shape smaller, rather than trying to force a
"delete while scanning" executor redesign in this subphase.

### PostgreSQL

PostgreSQL is useful mainly as a separation-of-concerns reference:

- tuple identification happens outside `heap_delete()`
- the actual delete primitive focuses on MVCC, locking, and WAL correctness

That confirms AxiomDB should keep the physical heap/B-Tree delete primitives
unchanged in this subphase and optimize the executor/materialization layer
instead.

## Problem statement

The old Phase 6 perf warnings mix together solved and unsolved costs.

What is already solved in the current codebase:

- no-split `insert_leaf` already rewrites the same page ID in place
- parent absorb-on-split already preserves the same internal/root page ID
- `DELETE FROM t` without `WHERE` already has the `5.16` bulk-empty path when
  there are no parent-FK references
- `DELETE ... WHERE` already has indexed candidate discovery (`6.3b`),
  batched heap delete, and batched B-Tree delete (`5.19`)

The remaining DELETE gap is therefore not "DELETE still deletes one row at a
time everywhere" and not "insert_leaf always allocates". The remaining gap is
the residual executor/apply overhead on DELETE paths that still materialize and
decode more row data than the delete operation actually needs.

This subphase targets that remaining DELETE overhead.

## What to build (not how)

Add a DELETE apply fast path that reduces residual DELETE overhead after `5.16`,
`5.19`, and `6.3b` by avoiding unnecessary full-row decode/materialization on
candidate rows while preserving all current SQL-visible behavior.

### Surface 1: delete-specific required-column set

For every single-table DELETE statement, AxiomDB must derive the minimal set of
columns needed to execute the statement correctly.

That set is the union of columns needed by:

- the original `WHERE` clause recheck
- FK parent enforcement on rows being deleted
- index key construction for every maintained index
- partial-index predicate evaluation for every maintained partial index

Columns outside that set must not be eagerly decoded for the DELETE path.

### Surface 2: masked candidate materialization

Candidate discovery and apply must stop treating DELETE as "decode full rows and
then throw most of the values away".

For both:

- full-scan DELETE paths
- indexed candidate DELETE paths

the executor must fetch only the required columns plus the `RecordId`, keeping
the existing logical candidate order and preserving the same `WHERE` recheck
semantics as today.

### Surface 3: no-WHERE delete with parent FK references

When `DELETE FROM t` cannot use the `5.16` bulk-empty path because child tables
reference `t` as a parent, AxiomDB must no longer fall back to a full decode of
every visible row if only parent-key/index columns are needed.

This path must still:

- collect all visible rows to delete
- enforce parent-FK behavior before heap mutation
- preserve RESTRICT / CASCADE / SET NULL behavior unchanged

### Surface 4: single materialization pass into delete apply

Once candidate rows are fetched for DELETE:

- FK parent enforcement
- heap delete batching
- index delete-key collection

must consume that same materialized candidate set.

The delete executor must not re-read or re-decode the same row payloads again
for later DELETE stages inside the same statement.

### Surface 5: preserve current physical mutation primitives

This subphase does not replace the current physical DELETE primitives.

The source of truth for physical mutation remains:

- `TableEngine::delete_rows_batch(...)` for heap mutation
- `collect_delete_keys_by_index(...)` for per-index key derivation
- `delete_many_from_indexes(...)` for batched B-Tree removal

This subphase only changes how candidate rows are prepared and fed into those
primitives.

### Surface 6: preserve current SQL behavior

This subphase must preserve:

- `QueryResult::Affected`
- full original `WHERE` recheck semantics
- FK parent enforcement behavior
- partial-index correctness
- savepoint / rollback semantics
- bloom dirtying behavior
- root refresh / cache invalidation behavior

## Inputs / Outputs

- Input:
  - `DeleteStmt`
  - resolved table metadata (`TableDef`, `ColumnDef`, `IndexDef`, FK metadata)
  - mutable storage / txn / bloom / session context in the ctx path
  - active transaction snapshot
- Output:
  - unchanged `QueryResult::Affected { count, last_insert_id: None }`
- Errors:
  - unchanged SQL/execution/FK/index/storage errors from current DELETE paths
  - no new SQL syntax
  - no new wire-protocol-visible result shape

## Use cases

1. `DELETE FROM bench_users WHERE id >= 25000`
   On the benchmark schema with only `PRIMARY KEY (id)`, AxiomDB uses the PK
   candidate path and decodes only the columns needed for the `WHERE` recheck
   and index/FK maintenance instead of materializing full rows.

2. `DELETE FROM parent`
   When child tables reference `parent`, AxiomDB cannot use bulk-empty, but it
   still avoids decoding unrelated columns while collecting rows for FK parent
   enforcement and batched delete.

3. `DELETE FROM users WHERE email = 'a@b.com'`
   With a UNIQUE secondary index on `email`, AxiomDB uses indexed candidate
   discovery and carries only the columns needed for correctness through the
   delete pipeline.

4. `DELETE FROM sessions WHERE expires_at < '2026-01-01'`
   If no usable index exists, AxiomDB falls back to a scan, but the scan still
   decodes only the DELETE-required column set instead of the full row.

5. `DELETE FROM orders WHERE status = 'cancelled'`
   With a partial index on `status`, AxiomDB still evaluates the partial-index
   predicate correctly before deciding whether an index key must be removed.

## Acceptance criteria

- [ ] DELETE no longer eagerly decodes all table columns for every candidate row
      when only a strict subset is needed for `WHERE`, FK, and index maintenance.
- [ ] The DELETE executor computes a delete-specific required-column set before
      candidate row materialization.
- [ ] Full-scan DELETE paths honor that required-column set.
- [ ] Indexed candidate DELETE paths honor that required-column set.
- [ ] `DELETE FROM t` with parent-FK references but without `WHERE` no longer
      requires full-row decode when only parent-key/index columns are needed.
- [ ] The executor does not re-read or re-decode the same candidate rows in a
      later DELETE stage inside the same statement.
- [ ] Existing physical mutation primitives remain unchanged as the source of
      truth for heap delete and batched B-Tree delete.
- [ ] FK parent enforcement semantics remain unchanged.
- [ ] Partial-index correctness remains unchanged.
- [ ] Session collation safeguards remain unchanged for any text-index-based
      candidate path that depends on binary ordering.
- [ ] The local DELETE benchmarks improve relative to the pre-6.21 baseline on
      the same machine, and any remaining gap is documented explicitly.

## Out of scope

- New DELETE syntax
- Multi-table DELETE
- MVCC redesign
- New B-Tree delete algorithm
- VACUUM / physical purge of dead tuples
- Replacing `5.16` bulk-empty DELETE
- Replacing `5.19` batched B-Tree delete
- Fixing the stale `insert_leaf` tracker wording as part of the executor code
  change itself

## Dependencies

- `5.16` bulk-empty DELETE path
- `5.19` batched B-Tree delete path
- `6.3b` indexed DELETE candidate discovery
- `6.7` partial-index predicate implication guard
- `6.9` FK helper/index behavior
- `6.20` masked/batched DML apply ideas for executor structure

## ⚠️ DEFERRED

- Tracker cleanup for stale Phase 6 perf warnings (`insert_leaf` wording and
  pre-`5.16` DELETE wording) → must be reconciled when `6.21` is reviewed and
  closed
- Any further DELETE gap caused by wire protocol serialization instead of the
  executor/apply path → pending in later wire/performance subphases
- Any redesign of B-Tree page merge/rebalance cost → separate index subphase
