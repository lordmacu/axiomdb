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

### Rule 0 — Composite equality (Phase 6.9)

```
WHERE col1 = v1 AND col2 = v2    (N ≥ 2 leading columns of index)
```

Condition: `col1, col2, ...` match the **N leading columns** of a composite index
`(col1, col2, ...)` as equality conditions in the AND-tree.

Result: `AccessMethod::IndexRange { lo: encode(v1, v2), hi: encode(v1, v2) }`

Both unique and non-unique composite indexes return all matching rows via range
scan with `lo = hi = composite_key`.

### Rule 1 — Equality lookup

```
WHERE col = literal    OR    literal = col
```

Condition: `col` is the **first column** of a non-primary secondary index.

- Single-column index → `AccessMethod::IndexLookup { key: encode(literal) }`
- Composite index → `AccessMethod::IndexRange { lo: prefix, hi: prefix + [0xFF…] }`
  (prefix scan returns all rows matching the leading column regardless of trailing columns)

### Rule 2 — Range scan

```
WHERE col > lo AND col < hi
WHERE col >= lo AND col <= hi
(any combination of > >= < <=)
```

Condition: both sides reference the same column, which is the first column of a
non-primary secondary index.

Result: `AccessMethod::IndexRange { index_def, lo: encode(lo_val), hi: encode(hi_val) }`

### Statistics cost gate (Phase 6.10)

After any Rule 0–2 match, the planner applies a statistics-based cost gate before
returning the index access method:

```
ndv       = stats.ndv > 0 ? stats.ndv : DEFAULT_NUM_DISTINCT (= 200)
selectivity = 1.0 / ndv            // equality predicate: ~1/ndv rows match
if row_count < 1,000: return Scan  // tiny table — index overhead not worth it
if selectivity > 0.20: return Scan // low-cardinality — full scan is cheaper
→ return IndexLookup / IndexRange  // selective enough for an index scan
```

`DEFAULT_NUM_DISTINCT = 200` is used when no statistics exist (pre-Phase 6.10
databases, or ANALYZE has not been run). This is conservative — always uses the
index when uncertain, which is never wrong.

### Fallback

All other patterns → `AccessMethod::Scan`.

Patterns **not** recognized:
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

## Bloom Filter — Skip B-Tree on Definite Absence

Each secondary index has a per-index Bloom filter managed by `BloomRegistry`.
Before performing an `IndexLookup` or `IndexRange`, the planner checks whether
the key is _definitely absent_ from the index:

```
1. Hash the key with the index's Bloom filter.
2. If the filter says ABSENT → skip the B-Tree lookup entirely.
3. If the filter says POSSIBLY PRESENT → proceed with the B-Tree lookup.
```

The filter is populated at `CREATE INDEX` time (bulk-load from the existing table)
and maintained incrementally:

| Operation | Filter update |
|---|---|
| `INSERT` | `bloom.add(index_id, key)` — key added |
| `DELETE` | `bloom.mark_dirty(index_id)` — filter rebuilt lazily |
| `UPDATE` | marks dirty if indexed columns changed |

**False positive rate:** ≈1% (configurable at build time via the number of hash
functions and bit array size). False positives cause unnecessary B-Tree lookups but
never produce incorrect results.

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
For workloads with many point-lookups for keys that do not exist (e.g. cache-miss
patterns, existence checks before INSERT), the Bloom filter eliminates the B-Tree
traversal entirely — reducing a 3–5 page I/O operation to a single in-memory hash
check. This matches the optimization used in RocksDB's SST files and DuckDB's ART
index absent-key path.
</div>
</div>

---

## Deferred Items

| Feature | Status | Phase |
|---------|--------|-------|
| Composite multi-column planner (> 1 column WHERE) | ✅ Done | 6.9 |
| Statistics cost gate (NDV, selectivity threshold) | ✅ Done | 6.10 |
| `EXPLAIN` statement | ⏳ Planned | 8.x |
| Covering indexes (index-only scan, no heap read) | ⏳ Planned | 6.13 |
| Partial index predicate matching (complex implication) | ⏳ Planned | 6.15 |
| Join selectivity estimation | ⏳ Planned | 6.15 |
