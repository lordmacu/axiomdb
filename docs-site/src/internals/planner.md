# Query Planner

The query planner is a lightweight **pattern-matching rewrite** that runs before the
executor for every `SELECT`. It detects whether the `WHERE` clause matches a predicate
on a usable index key and substitutes a B-Tree lookup for a full table scan.

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

    /// Index-only scan: all SELECT columns are covered by index key columns.
    /// No heap read is needed — values are decoded directly from B-Tree key bytes.
    IndexOnlyScan {
        index_def: IndexDef,
        lo: Option<Vec<u8>>,
        hi: Option<Vec<u8>>,
        n_key_cols: usize,              // number of encoded columns in the key
        needed_key_positions: Vec<usize>, // position of each SELECT column in the decoded key
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

Condition: `col` is the **first column** of a usable index.

- `PRIMARY KEY = literal` is forced to an indexed path even on tiny tables
  or when NDV stats would normally prefer `Scan`
- secondary-index equality still goes through the normal statistics gate

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
usable index.

Result: `AccessMethod::IndexRange { index_def, lo: encode(lo_val), hi: encode(hi_val) }`

### Statistics cost gate (Phase 6.10)

After any Rule 0–2 match, the planner usually applies a statistics-based cost gate before
returning the index access method:

```
ndv       = stats.ndv > 0 ? stats.ndv : DEFAULT_NUM_DISTINCT (= 200)
selectivity = 1.0 / ndv            // equality predicate: ~1/ndv rows match
if row_count == 0: return Index    // stats bootstrapped on empty table — conservative
if row_count < 1,000: return Scan  // tiny table — index overhead not worth it
if selectivity > 0.20: return Scan // low-cardinality — full scan is cheaper
→ return IndexLookup / IndexRange  // selective enough for an index scan
```

`PRIMARY KEY = literal` is the deliberate exception: it bypasses this gate and
returns `IndexLookup` directly.

`DEFAULT_NUM_DISTINCT = 200` is used when no statistics exist (pre-Phase 6.10
databases, or ANALYZE has not been run). This is conservative — always uses the
index when uncertain, which is never wrong.

`row_count == 0` is treated as "no reliable statistics" (not "empty table") because
stats are bootstrapped at `CREATE INDEX` time. If `CREATE INDEX` runs before any
`INSERT`s, the stats reflect an empty table but become stale immediately after the
first insert. The conservative default is to use the index.

### Fallback

All other patterns → `AccessMethod::Scan`.

Patterns **not** recognized:
- OR predicates
- Functions (`WHERE lower(name) = 'alice'`)
- Non-leading index column (`WHERE col2 = x` when index is `(col1, col2)`)
- JOIN conditions

### Rule 3 — Index-only scan (Phase 6.13)

After any Rule 0–2 match selects an index, `plan_select` checks whether the
query's projected columns are all covered by the index key. If so, it upgrades
the access method to `IndexOnlyScan`.

**Coverage check — `index_covers_query`:**

```
index_covers_query(index_def, select_col_idxs) -> bool:
  if select_col_idxs is empty:
    return false          // SELECT * → Scan/Lookup, never IndexOnlyScan
  for each col_idx in select_col_idxs:
    if col_idx not in index_def.key_col_idxs:
      return false        // at least one column is not in the key
  return true
```

**Key position mapping — `build_key_positions`:**

```
build_key_positions(index_def, select_col_idxs) -> Vec<usize>:
  for each col_idx in select_col_idxs:
    position = index_def.key_col_idxs.iter().position(|&k| k == col_idx)
    needed_key_positions.push(position)
  return needed_key_positions
```

Coverage only triggers when `select_col_idxs` is **non-empty**. A bare
`SELECT *` always produces an empty `select_col_idxs` (all columns requested)
and falls back to `Scan` or a regular `IndexLookup`/`IndexRange`.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Coverage Without Visibility Map</span>
PostgreSQL's <code>check_index_only</code> function in <code>indxpath.c</code>
uses the all-visible map bit to decide whether a heap fetch can be skipped per
page. AxiomDB uses the same coverage check (all SELECT columns must be index key
columns) but without the visibility map: a 24-byte <code>RowHeader</code> read
performs the MVCC check instead. This eliminates the VACUUM dependency at the
cost of one 24-byte I/O per visible row. A full all-visible bitmap is planned
for Phase 6.14.
</div>
</div>

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — PK Equality Bypasses Cost Gate</span>
SQLite's `where.c` and PostgreSQL's index-scan planning both treat primary-key equality as a first-class access path. AxiomDB now does the same: `WHERE pk = literal` ignores the small-table / NDV scan bias because the benchmarked point-lookup debt came from never reaching the PK B+Tree at all, not from a bad executor path.
</div>
</div>

---

## Executor Integration

In `execute_select` (single-table path), after resolving the table and before reading
rows:

```
1. Load indexes from catalog: CatalogReader::list_indexes(table_id)
2. plan_select(where_clause, indexes, columns) → access_method
3. Fetch rows:
   Scan            → TableEngine::scan_table (full heap scan)
   IndexLookup     → BTree::lookup_in → TableEngine::read_row (at most 1 row)
   IndexRange      → BTree::range_in → TableEngine::read_row (per B-Tree hit)
   IndexOnlyScan   → BTree::range_in → HeapChain::is_slot_visible (24-byte header read)
                     → decode_index_key(key_bytes, n_key_cols)
                     → project needed_key_positions from decoded key (no heap decode)
4. Apply residual WHERE filter on fetched rows (for conditions not consumed by index)
5. Continue with ORDER BY / LIMIT / projection as before
```

The residual WHERE filter handles cases where the index returned a row that later
fails a non-index condition (e.g., `WHERE indexed_col = 5 AND other_col = 'x'`). This
is safe and correct — the index just reduces the candidate set.

---

## UPDATE / DELETE Candidate Planning

The same planner module also feeds DML candidate discovery.

### DELETE candidates (`6.3b`)

- uses `plan_delete_candidates(...)`
- never applies `stats_cost_gate`
- never returns `IndexOnlyScan`
- materializes candidate `RecordId`s before heap/index mutation

### UPDATE candidates (`6.17`)

- uses `plan_update_candidates(...)`
- mirrors DELETE candidate eligibility:
  - PK, UNIQUE, secondary, and eligible partial indexes are allowed
  - no `stats_cost_gate`
  - no `IndexOnlyScan`
- `execute_update[_ctx]` fetches rows by RID, rechecks the full `WHERE`, and
  only then hands them to the `5.20` stable-RID / fallback write path

This separation matters: row discovery and physical row rewrite are different
problems. PostgreSQL's `ModifyTable` and SQLite's `where.c`/`update.c` split
them too; AxiomDB adopts the same boundary without copying those executor
architectures wholesale.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Discover Then Mutate</span>
PostgreSQL's <code>nodeModifyTable.c</code> and SQLite's <code>update.c</code> both rely on row identity produced by a planning step before mutation starts. AxiomDB now does the same for indexed UPDATE: it materializes candidate <code>RecordId</code>s first, then mutates later, so updating the same indexed column used by the predicate cannot interfere with discovery mid-statement.
</div>
</div>

<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
A point lookup via B-Tree on a 1M-row table requires O(log 1M) ≈ 20 page reads,
versus O(1M) for a full scan. The planner applies this automatically with no EXPLAIN
or HINT syntax required — every `WHERE col = ?` on a qualifying index, including
the primary key, is
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
3. Try TableEngine::update_rows_preserve_rid[...] → either:
   a. stable-RID same-slot rewrite (row still fits in the old slot), or
   b. fallback delete + insert (RID changes)
4. If RID changed:
   a. batch-delete old keys per affected index
   b. sync post-delete roots in memory
   c. insert new keys with the new RID
5. If RID stayed stable:
   a. skip indexes whose key bytes and predicate membership did not change
   b. only delete + reinsert the indexes whose logical membership changed
6. Persist root_page_id updates after the batch delete phase and after any reinsertion phase
```

The root synchronization between the delete and insert halves is still critical: if
the B-Tree root changed during old-key deletion (root collapse or same-page rebalance),
reinsertion must start from the new root, not the stale catalog value captured before
the batch.

<div class="callout callout-design">
<span class="callout-icon">⚙️</span>
<div class="callout-body">
<span class="callout-label">Design Decision — Stable RID Before Index Skips</span>
PostgreSQL HOT can skip index maintenance only because the tuple keeps a stable row
identity. AxiomDB now follows the same rule in a Phase 5-friendly form: unchanged
indexes are skipped only when the heap update preserves the same <code>RecordId</code>.
If the RID changes, every index that stores the RID must still be treated as changed.
</div>
</div>

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
| Covering indexes (index-only scan, no heap read) | ✅ Done | 6.13 |
| Partial index predicate matching (complex implication) | ⏳ Planned | future planner phases |
| Join selectivity estimation | ⏳ Planned | future planner phases |
