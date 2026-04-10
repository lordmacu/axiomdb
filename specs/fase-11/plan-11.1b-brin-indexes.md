# Plan: 11.1b — BRIN Indexes

## Files to create/modify

### New files
| File | Purpose |
|------|---------|
| `crates/axiomdb-storage/src/brin.rs` | Metapage, summary page read/write, range lookup |

### Modified files
| File | What changes |
|------|-------------|
| `crates/axiomdb-storage/src/lib.rs` | Export brin module |
| `crates/axiomdb-sql/src/executor/ddl_create_index.rs` | Build BRIN index from existing rows |
| `crates/axiomdb-sql/src/index_maintenance.rs` | INSERT: update BRIN summary for affected range |
| `crates/axiomdb-sql/src/planner.rs` or `planner_zone.rs` | Extend planner to consider BRIN indexes |
| `crates/axiomdb-sql/src/executor/select_ctx.rs` | Use BRIN range bitmap during scan |
| `crates/axiomdb-sql/src/table_scan.rs` | Filter scan by BRIN-qualifying page ranges |

## Algorithm / Data structure

### BRIN metapage format
```
Page 0 (metapage):
  magic: u32 = 0x4252494E
  version: u32 = 1
  pages_per_range: u32
  col_idx: u16        // which heap column is indexed
  num_ranges: u32     // total ranges stored
  // Rest of page: summary entries inline (no separate summary pages for small indexes)
```

### Summary entries (inline in metapage, overflow to additional pages)
```rust
struct BrinSummary {
    has_values: bool,
    min_val: i64,    // encoded: Int→i64, BigInt→i64, Real→f64_to_bits, Date→i32, Timestamp→i64
    max_val: i64,
    has_null: bool,
}
```

Encoding: 18 bytes per range. One 16KB page holds ~900 ranges.
For 1M rows with 128 pages/range and ~100 rows/page ≈ ~78 ranges → fits in metapage.

### CREATE INDEX build
1. Scan all heap pages in order
2. Group by range: `page_id / pages_per_range`
3. For each range: compute min/max from all visible rows
4. Write summaries to metapage (+ overflow pages if needed)
5. Store `root_page_id = metapage_id` in IndexDef

### INSERT maintenance
1. After heap insert returns `(page_id, slot_id)`:
2. `range_id = page_id / pages_per_range`
3. Read summary for range_id from BRIN metapage
4. Update: `min = min(old_min, new_val)`, `max = max(old_max, new_val)`
5. Write back (single byte-range update in metapage)

### Planner integration
1. When WHERE has `col op literal` on a BRIN-indexed column:
2. For each BRIN range:
   - Check `min <= literal <= max` (for EQ)
   - Check `min <= upper_bound` and `max >= lower_bound` (for BETWEEN/range)
3. Build set of qualifying range_ids
4. During scan: only visit heap pages in qualifying ranges

## Implementation phases

### Phase 1: Storage module (brin.rs)
1. Define `BrinSummary` struct
2. `brin_init_metapage()` — allocate + write empty metapage
3. `brin_read_summaries()` — read all range summaries from metapage
4. `brin_write_summary()` — update single range summary
5. `brin_encode_value()` — convert Value to i64 for comparison

**Verifiable:** unit tests for read/write round-trip.

### Phase 2: CREATE INDEX build
1. In `execute_create_index()`: detect BRIN → call `build_brin_index()`
2. Scan heap, collect min/max per range
3. Write summaries to metapage
4. Store root_page_id in catalog

**Verifiable:** `CREATE INDEX idx ON t USING brin (col)` + `SHOW INDEX` shows BRIN.

### Phase 3: INSERT maintenance
1. In `insert_into_indexes()`: detect BRIN index → update summary
2. After heap insert: compute range_id, update min/max

**Verifiable:** INSERT new row → BRIN summary reflects new min/max.

### Phase 4: Planner + scan integration
1. Extract BRIN predicates from WHERE (extend `planner_zone.rs`)
2. Build qualifying range set
3. In `scan_table()`: skip pages not in qualifying ranges
4. Pass range bitmap through to the scan function

**Verifiable:** SELECT with WHERE on BRIN column skips non-qualifying ranges.

## Tests to write
- `test_create_brin_index` — CREATE + verify metapage exists
- `test_brin_insert_updates_summary` — INSERT → min/max updated
- `test_brin_scan_skip_ranges` — SELECT with WHERE skips ranges
- `test_brin_correct_results` — no false negatives
- `test_brin_null_handling` — NULL values tracked but don't affect min/max
- `test_brin_drop_index` — DROP INDEX frees BRIN pages

## Anti-patterns to avoid
- DO NOT modify zone map infrastructure — BRIN is separate and complementary
- DO NOT build a revmap (PostgreSQL's revmap is for variable-size tuples — our fixed-size summaries don't need it)
- DO NOT try to make BRIN work for clustered tables (heap only for now)
- DO NOT add opclass framework — minmax-only is sufficient for Phase 11

## Risks
| Risk | Mitigation |
|------|-----------|
| Metapage overflow for large tables | Overflow to additional pages (allocate on demand) |
| False positives too high | Default pages_per_range=128 balances granularity vs overhead |
| INSERT overhead for BRIN maintenance | Single i64 comparison + conditional page write — negligible |
