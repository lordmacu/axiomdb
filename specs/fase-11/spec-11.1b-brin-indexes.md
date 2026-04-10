# Spec: 11.1b — BRIN Indexes (Block Range INdex)

## What to build (not how)

Complete the BRIN index implementation: storage module, CREATE INDEX build,
INSERT maintenance, and planner/executor integration. BRIN indexes store
per-range (N heap pages) min/max summaries, enabling the planner to skip
entire ranges that cannot match a WHERE predicate.

## Current state

- Step 1 ✅ AST+parser: `CREATE INDEX ... USING brin`, `WITH (pages_per_range=N)`
- Step 2 ✅ Catalog: `IndexDef.index_type: u8` (0=BTree, 1=BRIN), `pages_per_range: u32`

## Research findings

### PostgreSQL BRIN (research/postgresql/src/backend/access/brin/)
- **Metapage**: magic, version, pagesPerRange, lastRevmapPage
- **Revmap pages**: `ItemPointerData[]` — maps heap range → summary tuple location
- **Data pages**: store summary tuples (minmax, bloom, etc.)
- **Opclass callbacks**: ADDVALUE, UNION, CONSISTENT — generic framework
- **BRIN scan**: build TIDBitmap of qualifying ranges, executor scans those

### AxiomDB zone maps (complementary, NOT replacement)
- Zone maps: per-page min/max in PageHeader (18 bytes, automatic, single column, i64 only)
- BRIN: per-range (128 pages), separate index structure, explicit, multi-column capable
- Both can coexist on the same table

### Design decision: minmax-only, no opclass
Start with minmax summaries for numeric columns (Int, BigInt, Real, Date, Timestamp).
Covers 90% of time-series use cases without needing an opclass plugin system.

## Inputs / Outputs
- Input: `CREATE INDEX idx ON t USING brin (col)` or `CREATE INDEX idx ON t USING brin (col) WITH (pages_per_range=64)`
- Output: BRIN index structure stored in allocated pages, usable by planner for range skip
- Maintenance: INSERT updates range summary; DELETE/UPDATE do nothing (summary is a superset)

## On-disk format

### Metapage (1 page, allocated at CREATE INDEX)
```
[magic: u32 = 0x4252494E "BRIN"]
[version: u32 = 1]
[pages_per_range: u32]
[num_indexed_cols: u16]
[col_idx: u16]×num_indexed_cols
[num_ranges: u32]
[summary_start_page: u64]  // first page storing summary tuples
[padding to page size]
```

### Summary pages (sequential, one entry per range)
Each summary entry (fixed 40 bytes per indexed column):
```
[range_id: u32]           // heap_block / pages_per_range
[has_values: u8]          // 0=empty range, 1=has values
[min_value: i64]          // min of all values in range (encoded as i64)
[max_value: i64]          // max of all values in range (encoded as i64)
[null_present: u8]        // 1 if any NULL in range
[padding: 6 bytes]        // align to 40 bytes
```
With 16KB pages and 40 bytes per entry: ~400 ranges per page.
For a 1M row table with 128 pages_per_range: ~50 ranges → fits in 1 page.

## Use cases
1. Time-series: `WHERE created_at > '2024-01-01'` on naturally-ordered data → skip 90%+ of ranges
2. Sequential IDs: `WHERE id BETWEEN 1000 AND 2000` → skip all ranges outside [1000,2000]
3. IoT sensors: `WHERE device_id = 42 AND ts > NOW() - INTERVAL '1h'` → BRIN on ts

## Acceptance criteria
- [ ] `CREATE INDEX idx ON t USING brin (col)` builds BRIN index from existing rows
- [ ] INSERT into table updates BRIN summary for affected range
- [ ] SELECT with WHERE on BRIN-indexed column uses range skip in planner
- [ ] Correct results: no false negatives (may have false positives)
- [ ] Works for Int, BigInt, Real, Date, Timestamp columns
- [ ] `pages_per_range` configurable (default 128)
- [ ] DROP INDEX removes BRIN pages
- [ ] Existing zone maps unaffected (complementary)

## Out of scope
- Multi-column BRIN (single column for now)
- Opclass framework (minmax only)
- BRIN for Text/Bytes columns
- BRIN on clustered tables (heap only for now)
- Concurrent BRIN maintenance under multi-writer (Phase 40 covers page-level locking)

## Dependencies
- Step 1+2 (parser+catalog): ✅ done
- HeapChain scan infrastructure: ✅ exists
- Zone map planner infrastructure: ✅ exists (extend for BRIN)
