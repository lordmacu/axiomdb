# Query Planner

The query planner is a lightweight **pattern-matching rewrite** that runs before the
executor for every `SELECT`. It detects whether the `WHERE` clause matches a predicate
on a secondary indexed column and substitutes a B-Tree lookup for a full table scan.

---

## Access Methods

The planner returns an `AccessMethod` that drives the executor's row-fetching strategy:

```rust
pub enum AccessMethod {
    /// Full sequential scan — read every row from the heap.
    Scan,

    /// Point lookup: B-Tree lookup → single heap row read.
    IndexLookup {
        index_def: IndexDef,
        key: Vec<u8>,     // pre-encoded via encode_index_key
    },

    /// Range scan: iterate B-Tree entries in [lo, hi] → heap reads.
    IndexRange {
        index_def: IndexDef,
        lo: Option<Vec<u8>>,
        hi: Option<Vec<u8>>,
    },
}
```

---

## Pattern Matching Rules

`plan_select(where_clause, indexes, columns)` applies rules in order:

### Rule 1 — Equality lookup

```
WHERE col = literal    OR    literal = col
```

Condition: `col` is the **first column** of a non-primary secondary index.

Result: `AccessMethod::IndexLookup { index_def, key: encode(literal) }`

### Rule 2 — Range scan

```
WHERE col > lo AND col < hi
WHERE col >= lo AND col <= hi
(any combination of > >= < <=)
```

Condition: both sides reference the same column, which is the first column of a
non-primary secondary index.

Result: `AccessMethod::IndexRange { index_def, lo: encode(lo_val), hi: encode(hi_val) }`

### Fallback

All other patterns → `AccessMethod::Scan`.

Patterns **not** recognized (Phase 6.3):
- OR predicates
- Functions (`WHERE lower(name) = 'alice'`)
- Non-leading index column (`WHERE col2 = x` when index is `(col1, col2)`)
- JOIN conditions

---

## Executor Integration

In `execute_select` (single-table path), after resolving the table and before reading
rows:

```
1. Load indexes from catalog: CatalogReader::list_indexes(table_id)
2. plan_select(where_clause, indexes, columns) → access_method
3. Fetch rows:
   Scan       → TableEngine::scan_table (full heap scan)
   IndexLookup → BTree::lookup_in → TableEngine::read_row (at most 1 row)
   IndexRange  → BTree::range_in → TableEngine::read_row (per B-Tree hit)
4. Apply residual WHERE filter on fetched rows (for conditions not consumed by index)
5. Continue with ORDER BY / LIMIT / projection as before
```

The residual WHERE filter handles cases where the index returned a row that later
fails a non-index condition (e.g., `WHERE indexed_col = 5 AND other_col = 'x'`). This
is safe and correct — the index just reduces the candidate set.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
A point lookup via B-Tree on a 1M-row table requires O(log 1M) ≈ 20 page reads,
versus O(1M) for a full scan. The planner applies this automatically with no EXPLAIN
or HINT syntax required — every `WHERE col = ?` on an indexed column is
accelerated transparently.
</div>
</div>

---

## Index Maintenance

Secondary indexes are kept consistent with the heap on every DML operation. This is
implemented in `axiomdb-sql/src/index_maintenance.rs`.

### INSERT

```
1. heap INSERT → RecordId
2. For each secondary non-primary index on this table:
   a. Skip if key has any NULL component (NULLs are not indexed)
   b. Encode key from row values
   c. If UNIQUE: check B-Tree for existing key → UniqueViolation if found
   d. BTree::insert_in(storage, root_pid, key, rid)
   e. Persist updated root_page_id if root split occurred
```

### DELETE

```
For each deleted row:
1. Capture (rid, row_values) before deletion
2. heap DELETE
3. For each secondary non-primary index:
   a. Skip NULL key components
   b. Encode key from old row_values
   c. BTree::delete_in(storage, root_pid, key)
   d. Persist updated root_page_id if root collapsed
```

### UPDATE

```
For each matched row:
1. Capture old_values
2. Apply SET assignments → new_values
3. TableEngine::update_row → new_rid (heap delete + insert)
4. delete_from_indexes(old_values)   — removes old key
5. Update in-memory root_page_ids (root may have changed after delete)
6. insert_into_indexes(new_values, new_rid) — inserts new key
7. Persist root_page_id updates for both operations
```

The in-memory root_page_id update between steps 5 and 6 is critical: if the B-Tree
root changed during delete (page freed and replaced), the insert must use the new
root, not the stale catalog value.

---

## NULL Handling in Indexes

NULL values are **not stored** in secondary B-Trees. Rationale:

- SQL defines `NULL ≠ NULL` — two NULLs should never trigger a UNIQUE violation.
- Storing NULLs would require composite keys with RIDs to avoid DuplicateKey errors.
- MySQL InnoDB, PostgreSQL, and SQLite all omit NULLs from secondary index B-Trees
  (they are tracked separately or not at all).

Consequence: `WHERE col IS NULL` always falls back to a full table scan.

---

## Deferred Items

| Feature | Phase |
|---------|-------|
| Composite multi-column planner (> 1 column WHERE) | 6.8 |
| Cost-based optimizer (NDV, row counts) | 6.10 |
| `EXPLAIN` statement | 8.x |
| Covering indexes (no heap read) | 9.x |
| Partial index predicate matching | 10.x |
