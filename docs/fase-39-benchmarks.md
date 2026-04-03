# Phase 39 — Clustered Index: Benchmark Results

## Date: 2026-04-03
## Status: Functionally complete, performance optimization pending

---

## Architecture Change

AxiomDB now uses **clustered index storage** for all tables with PRIMARY KEY.

```
BEFORE (Phases 1-7):
  Data lives in heap pages (insertion order)
  PK B-tree stores key → RecordId (page_id, slot_id)
  Every PK operation = 2 I/O paths (index + heap)

AFTER (Phase 39):
  Data lives INSIDE B-tree leaf pages (PK order)
  No separate heap for clustered tables
  PK operation = 1 I/O path (clustered leaf)
  Secondary indexes store PK value as bookmark (not RecordId)
```

## Current Benchmark Results (50K rows, clustered)

| Scenario | MariaDB 12.1 | MySQL 8.0 | AxiomDB (Clustered) | vs best |
|---|---|---|---|---|
| insert | 26,970 r/s | 23,350 r/s | 15,263 r/s | 🔴 0.57x |
| insert_multi_values | 364,673 r/s | 226,718 r/s | 58,567 r/s | 🔴 0.16x |
| insert_autocommit | 18,154 r/s | 7,273 r/s | 7,083 r/s | 🔴 0.39x |
| select (full scan) | 199,796 r/s | 197,543 r/s | 157,717 r/s | 🟡 0.79x |
| select_where | 193,054 r/s | 174,222 r/s | 119,874 r/s | 🔴 0.62x |
| select_pk | 12,080 r/s | 10,714 r/s | 9,575 r/s | 🟡 0.79x |
| select_range | 204,621 r/s | 203,175 r/s | 172,645 r/s | 🟡 0.84x |
| count | 400 r/s | 854 r/s | 17 r/s | 🔴 0.02x |
| aggregate | 151 r/s | 108 r/s | 15 r/s | 🔴 0.10x |
| update | 1,017,151 r/s | 356,428 r/s | 91,204 r/s | 🔴 0.09x |
| update_range | 1,080,713 r/s | 395,630 r/s | 103,870 r/s | 🔴 0.10x |
| delete | 1,083,357 r/s | 560,818 r/s | 109,319 r/s | 🔴 0.10x |
| delete_where | 1,074,806 r/s | 640,789 r/s | 92,881 r/s | 🔴 0.09x |

**Scorecard: 🟢 0 | 🟡 3 | 🔴 10**

## Why reads improved but writes regressed

### Reads (select_pk, select_range): Improved ✅

The clustered architecture eliminates the heap indirection:
- **Before**: B-tree search → RecordId → heap page read → decode
- **After**: B-tree search → row data inline in leaf → decode

This is working as designed. `select_pk` went from 0.74x to 0.79x,
`select_range` from 0.77x to 0.84x.

### Writes (insert, update, delete): Regressed ❌

The clustered executor paths are **functionally correct but unoptimized**.
They lack the optimizations that the heap path accumulated over Phases 4-8:

| Optimization | Heap path | Clustered path | Impact |
|---|---|---|---|
| **Batch page writes** | Groups operations by page_id, 1 write per page | Per-row write_page() | 10-50x for bulk ops |
| **Field-patch** (byte-level update) | Patches 8 bytes directly in page for fixed-size cols | Full row decode → modify → re-encode → rewrite cell | 5-15x for UPDATE |
| **COUNT(*) fast path** | HeapChain::count_visible() — header-only scan, zero decode | Full clustered scan with decode | 50-100x for COUNT |
| **BatchPredicate** | WHERE evaluated on raw encoded bytes (~20ns/row) | WHERE evaluated on decoded Vec<Value> (~130ns/row) | 3-6x for filtered scans |
| **Batch WAL** | PageWrite/PageDelete entries batch N rows per page | Per-row WAL entries | 5-10x for WAL overhead |
| **Fused scan-patch** | Scan + filter + patch in single page pass | Separate candidate collection → separate update pass | 2-3x for range updates |
| **Zone maps** | Skip pages where predicate cannot match | Not available for clustered leaves | 1.5-2x for selective queries |

## Optimization roadmap for clustered path

### Priority 1: COUNT(*) fast path
- **Current**: full clustered scan + decode every row → 78ms for 50K rows
- **Target**: walk leaf chain, count cells with visible RowHeader → <1ms
- **Approach**: `count_clustered_visible(storage, root_pid, snap)` — iterate leaves,
  read RowHeader per cell (24 bytes), check `is_visible()`, no row decode
- **Expected improvement**: 0.02x → ~1.5x

### Priority 2: Batch clustered delete
- **Current**: per-row `delete_mark()` + per-row WAL entry → 10x overhead
- **Target**: group deletes by leaf page, single page read+write per page, batch WAL
- **Approach**: collect all (page_id, cell_idx) pairs → group by page → mark all on same
  page in one pass → single write_page + batch WAL entry
- **Expected improvement**: 0.10x → ~1.0x

### Priority 3: Field-patch on clustered leaves
- **Current**: full decode → modify → full encode → rewrite cell
- **Target**: locate field offset in cell bytes → patch N bytes → write page
- **Approach**: Reuse `field_patch::compute_field_location_runtime()` on clustered cell
  row_data bytes. Same as heap field-patch but offset includes cell header + key.
- **Expected improvement**: 0.09x → ~0.5x+

### Priority 4: Batch clustered insert
- **Current**: per-row `clustered_tree::insert()` + per-row WAL
- **Target**: group rows by target leaf page, batch insert, batch WAL
- **Approach**: For INSERT ... VALUES with N rows, sort by PK, insert sequentially
  (already efficient due to sorted order → append-only leaf fill)
- **Expected improvement**: 0.57x → ~0.9x

### Priority 5: BatchPredicate on clustered scan
- **Current**: decode every row, eval WHERE on Vec<Value>
- **Target**: eval WHERE on raw cell bytes without decoding non-WHERE columns
- **Approach**: Adapt BatchPredicate to work on clustered cell format (skip cell header
  + key bytes → raw row_data is same format as heap row encoding)
- **Expected improvement**: 0.62x → ~0.9x

## Compatibility requirements

All optimizations MUST be compatible with the clustered index architecture:

1. **Cell format**: optimizations operate on `[key_len][row_len][RowHeader][key][row_data]` cells
2. **MVCC**: RowHeader.txn_id_created/deleted must be respected for visibility
3. **Overflow**: cells may have overflow pointers — optimizations must handle both inline and overflow
4. **next_leaf chain**: leaf chain traversal must be preserved for range scans
5. **WAL**: all modifications must generate proper WAL entries for crash recovery
6. **Freeblock chain**: cell modifications that change size must maintain page space accounting
7. **Secondary indexes**: PK bookmark format must be preserved (not RecordId)

## Files implementing clustered executor paths

| File | Functions | Needs optimization |
|---|---|---|
| `executor/select.rs` | Clustered scan/lookup/range dispatch | BatchPredicate, COUNT fast path |
| `executor/update.rs` | `execute_clustered_update()` | Field-patch, batch page writes |
| `executor/delete.rs` | `execute_clustered_delete()` | Batch delete, batch WAL |
| `table.rs` | `scan_clustered_table()`, `lookup_clustered_row()`, `range_clustered_table()` | BatchPredicate integration |
| `vacuum.rs` | `vacuum_clustered_leaves()` | Already batch-oriented ✅ |
