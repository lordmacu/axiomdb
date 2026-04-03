# Spec: 39.15 — Executor Integration: SELECT from Clustered Table

## What to build (not how)

Remove the `ensure_heap_runtime()` guard and route SELECT queries on clustered
tables through the clustered B-tree storage layer instead of the heap. The executor
must support three access paths on clustered tables:

1. **Full scan**: iterate all clustered B-tree leaves via `clustered_tree::range(Unbounded, Unbounded)`
2. **PK point lookup**: `clustered_tree::lookup(key)` when WHERE is `pk_col = literal`
3. **PK range scan**: `clustered_tree::range(lo, hi)` when WHERE is `pk_col BETWEEN lo AND hi`

Secondary index lookups on clustered tables follow the path:
secondary B-tree → extract PK bookmark → clustered B-tree lookup.

## Research findings

### InnoDB SELECT on clustered index
- `row_search_mvcc()` (row0sel.cc:4343): single function handles both clustered and secondary
- Decision: `if (index == clust_index)` → data is inline in leaf record, no heap fetch
- MVCC: `row_get_rec_trx_id(rec)` checked against read view directly on clustered record
- Range scan: cursor advances through clustered leaves, returns row data inline
- Full table scan: `rnd_next()` calls `index_first()` then `index_next()` on clustered index
- Prefetch: InnoDB buffers ~100 rows internally (`MYSQL_FETCH_CACHE_SIZE`)

### PostgreSQL (contrast — no clustered mode)
- Always two-phase: index → TID → heap fetch
- Sequential scan: iterate heap pages directly (no index)
- MVCC visibility in heap tuple headers (xmin/xmax), NOT in index
- AxiomDB's clustered approach eliminates this two-phase cost

### AxiomDB current executor (what changes)
- `ensure_heap_runtime()` at select.rs line 37 rejects clustered tables
- Planner produces `Scan`, `IndexLookup`, `IndexRange` regardless of storage layout
- Executor dispatches to `TableEngine::scan_table_filtered()` (heap) for all paths
- **Change**: dispatch to `clustered_tree::range()` / `clustered_tree::lookup()` when `is_clustered()`

## Inputs / Outputs

- Input: SQL SELECT on a clustered table (via `execute_select_ctx`)
- Output: `QueryResult::Rows` with correct data, MVCC-filtered
- Errors: same as heap path (column not found, parse errors, etc.)

## Three access paths

### Path 1: Full table scan (no WHERE or non-PK WHERE)

```
AccessMethod::Scan on clustered table:
  1. Call clustered_tree::range(storage, root_pid, Unbounded, Unbounded, snapshot)
  2. Iterate ClusteredRangeIter → each row yields ClusteredRow { key, row_header, row_data }
  3. Decode row_data via codec::decode_row()
  4. Apply WHERE filter on decoded values (if non-PK WHERE exists)
  5. Project selected columns
  6. Return rows
```

MVCC visibility handled inside `ClusteredRangeIter` (already filters invisible rows).

### Path 2: PK point lookup (WHERE pk = literal)

```
AccessMethod::IndexLookup on PK index of clustered table:
  1. Encode PK value as key bytes
  2. Call clustered_tree::lookup(storage, root_pid, key, snapshot)
  3. If Some(ClusteredRow) → decode row_data, apply remaining WHERE, project
  4. If None → empty result
```

Single B-tree descent. No heap fetch.

### Path 3: PK range scan (WHERE pk BETWEEN lo AND hi)

```
AccessMethod::IndexRange on PK index of clustered table:
  1. Encode lo/hi as key bytes with proper bounds (Included/Excluded)
  2. Call clustered_tree::range(storage, root_pid, lo_bound, hi_bound, snapshot)
  3. Iterate → decode each row → apply remaining WHERE → project
```

Leaf chain traversal with prefetch. No heap fetch.

### Path 4: Secondary index lookup on clustered table

```
AccessMethod::IndexLookup on secondary index of clustered table:
  1. BTree::lookup_in(secondary_root, sec_key) → get (RecordId, key_bytes)
  2. Decode key_bytes via ClusteredSecondaryLayout::decode_entry_key() → extract PK values
  3. Encode PK as key bytes
  4. clustered_tree::lookup(storage, clustered_root, pk_key, snapshot) → full row
  5. Decode, filter, project
```

Two B-tree lookups (secondary → clustered) instead of one B-tree + one heap.

## Row decoding for clustered rows

`ClusteredRow.row_data` is encoded with `axiomdb_types::codec::encode_row()` — same
format as heap rows. The decode path is identical:

```rust
let values = codec::decode_row(&clustered_row.row_data, &col_types)?;
```

No new codec needed. The row_data stored in clustered leaves uses the exact same
null_bitmap + field encoding as heap rows.

## Use cases

1. `SELECT * FROM users WHERE id = 5` — PK point lookup → 1 clustered B-tree search
2. `SELECT * FROM users WHERE id BETWEEN 10 AND 20` — PK range → leaf chain scan
3. `SELECT * FROM users` — full scan → leftmost leaf → chain traversal
4. `SELECT * FROM users WHERE name = 'Alice'` — secondary index → PK bookmark → clustered lookup
5. `SELECT * FROM users WHERE active = TRUE` — no index on `active` → full scan + filter
6. `SELECT COUNT(*) FROM users` — full scan, count visible rows
7. `SELECT id, name FROM users ORDER BY id` — clustered scan already returns PK order

## Acceptance criteria

- [ ] `ensure_heap_runtime()` guard removed for SELECT on clustered tables
- [ ] Full scan via `clustered_tree::range(Unbounded, Unbounded, snapshot)`
- [ ] PK point lookup via `clustered_tree::lookup(key, snapshot)`
- [ ] PK range scan via `clustered_tree::range(lo, hi, snapshot)`
- [ ] Secondary index → PK bookmark → clustered lookup path works
- [ ] MVCC visibility: only committed rows visible (tested)
- [ ] WHERE clause evaluated on decoded values (post-fetch filter)
- [ ] Projection: only selected columns returned
- [ ] GROUP BY, ORDER BY, LIMIT work on clustered scan results
- [ ] Existing heap SELECT paths unchanged (no regression)
- [ ] Integration test: INSERT rows then SELECT them back via all 3 paths
- [ ] Integration test: MVCC — concurrent snapshot doesn't see uncommitted rows
- [ ] Wire protocol smoke test: pymysql SELECT from clustered table

## Out of scope

- BatchPredicate on clustered leaves (optimization for Phase 39.20)
- Zone maps for clustered pages (different page structure)
- Parallel scan on clustered leaves (Phase 9.1 extension)
- SELECT FOR UPDATE/FOR SHARE (Phase 40.5 lock integration)

## Dependencies

- 39.1-39.5: Clustered leaf/internal/insert/lookup/range — all complete ✅
- 39.9: Secondary PK bookmarks — complete ✅
- 39.13-39.14: CREATE TABLE + INSERT clustered — complete ✅
