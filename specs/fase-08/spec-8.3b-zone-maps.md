# Spec: 8.3b — Zone Maps (Per-Page Min/Max)

## Context

Full table scans evaluate every row even when the predicate eliminates most
pages. For `WHERE age = 30` on 5000 rows across 25 pages, if age values are
sorted/clustered, ~24 pages contain no matching rows but are still scanned.

**Research findings:**
- DuckDB: per-segment min/max, skips 2048-row vectors at a time
- PostgreSQL BRIN: per-128-pages range, min/max per column
- OceanBase: per-micro-block min/max + null_count, 1-5% overhead
- InnoDB/SQLite: no zone maps

## What to build

Store min/max values for selected columns in each heap page's header
(`_reserved` field, 28 bytes available). During full scan, check if the
page's min/max range overlaps with the WHERE predicate before decoding
any rows on that page.

### Format (in PageHeader._reserved[0..28])

```
Byte 0:     zone_map_version (u8) — 0 = no zone map, 1 = v1
Byte 1:     num_entries (u8) — number of column min/max pairs (0-2)
Byte 2..3:  col_idx_0 (u16 LE) — first tracked column index
Byte 4..11: min_0 (8 bytes) — minimum value (type-dependent encoding)
Byte 12..19: max_0 (8 bytes) — maximum value
Byte 20..21: col_idx_1 (u16 LE) — second tracked column (if num_entries=2)
Byte 22..25: min_1 (4 bytes, truncated) — second column min
Byte 26..27: reserved
```

This supports 1 full-precision column (8-byte min/max for Int/BigInt/Real)
or 2 columns with reduced precision.

### Scan skip logic

```rust
// In scan_table_filtered, before entering the slot loop:
if let Some(zm) = read_zone_map(&page) {
    if !zone_map_might_match(&zm, &where_col_idx, &predicate_bounds) {
        current = next;  // Skip entire page — 0 row decodes
        continue;
    }
}
```

### Maintenance

- INSERT: after writing row data, update page zone map if new value extends
  min or max for any tracked column
- UPDATE: invalidate zone map (set version=0) — recalculate lazily
- DELETE: do NOT update zone map (stale min/max is conservative — may scan
  a page unnecessarily, but never skips a page incorrectly)

## Acceptance Criteria

- [ ] Zone map stored in PageHeader._reserved for configured columns
- [ ] scan_table_filtered skips pages where predicate is outside min/max
- [ ] INSERT updates zone map incrementally
- [ ] DELETE does not invalidate zone map (conservative)
- [ ] UPDATE invalidates zone map (version=0)
- [ ] Identical query results with and without zone maps
- [ ] `cargo test --workspace` passes
- [ ] Benchmark improvement on select_where without secondary index

## Out of Scope
- Multi-column composite zone maps
- String/Text column zone maps (only numeric types for v1)
- Automatic column selection (caller specifies which columns to track)
- BRIN-style separate index structure
