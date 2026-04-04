# Spec: 39.18 — VACUUM for Clustered Index

## What to build (not how)

Physically remove dead cells from clustered B-tree leaf pages and reclaim space.
Dead cells are rows with `txn_id_deleted != 0` that are no longer visible to any
active transaction. Without VACUUM, dead cells accumulate indefinitely, wasting
page space and slowing scans.

## Research findings

### InnoDB purge (reference for safety boundary)
- **Undo-log driven**: purge thread scans undo history, not table pages. Efficient
  for sparse deletes. `row_purge_remove_clust_if_poss()` → `btr_cur_pessimistic_delete()`.
- **Safety**: `txn_id < purge_sys.view.snapshot_id`. Purge view advances only when ALL
  readers at that snapshot time have closed.
- **Two-phase B-tree deletion**: optimistic (leaf-only) → pessimistic (with rebalance).
- **Secondary index**: `row_purge_remove_sec_if_poss()` — two-phase per entry.
- **No explicit defrag**: B-tree natural rebalancing handles space consolidation.

### PostgreSQL VACUUM (reference for scan-based approach)
- **Full heap scan**: `lazy_scan_heap()` iterates all pages. Identifies dead tuples
  via `xmax < OldestXmin`. Stores TIDs in `dead_items` store.
- **Three-phase**: (1) heap scan + dead identification, (2) index bulk-delete, (3) heap LP_UNUSED marking.
- **On-access pruning**: `heap_page_prune_opt()` — mini-VACUUM during SELECT/UPDATE
  if page has reclaimable space. Complementary to full VACUUM.
- **Visibility map**: tracks all-visible pages to skip in future VACUUMs.

### AxiomDB existing heap VACUUM
- `vacuum_heap_chain()`: linear chain walk, `mark_slot_dead()` for dead slots.
- `vacuum_index()`: full B-tree scan, visibility check per entry, bulk delete.
- Safety: `oldest_safe_txn = max_committed + 1`.
- No defragmentation (slots zeroed but space not reclaimed).

### Building blocks already available for clustered
- `clustered_leaf::remove_cell(page, pos)` — physical cell removal + freeblock chain ✅
- `clustered_leaf::defragment(page)` — full page compaction ✅
- `clustered_overflow::free_chain(storage, first_page)` — overflow cleanup ✅
- `clustered_leaf::free_space(page)` — gap + freeblock total ✅
- `clustered_leaf::num_cells(page)` — cell count ✅
- `clustered_leaf::read_cell(page, idx)` — read cell + RowHeader ✅
- `clustered_leaf::next_leaf(page)` — leaf chain traversal ✅

## Inputs / Outputs

- Input: `VACUUM table_name` on a clustered table (or implicit call)
- Output: count of dead cells removed + count of dead index entries removed
- Side effects: page space reclaimed, overflow chains freed, secondary indexes cleaned,
  and any rotated secondary roots persisted back to the catalog

## VACUUM algorithm for clustered tables

### Phase 1: Scan clustered leaves and remove dead cells

```
oldest_safe_txn = max_committed + 1  (no active readers past this point)
dead_count = 0

current_page_id = leftmost leaf (descend from root via child 0)

while current_page_id != NULL_PAGE:
    page = read_page(current_page_id)
    next = clustered_leaf::next_leaf(page)
    modified = false

    // Scan cells in REVERSE order (to avoid index shift issues during removal)
    for idx in (0..num_cells(page)).rev():
        cell = read_cell(page, idx)
        if cell.row_header.txn_id_deleted != 0
           && cell.row_header.txn_id_deleted < oldest_safe_txn:
            // Safe to purge
            if let Some(overflow_pid) = cell.overflow_first_page:
                clustered_overflow::free_chain(storage, overflow_pid)
            remove_cell(page, idx)
            dead_count += 1
            modified = true

    // Conditional defragmentation: if freeblock waste exceeds threshold
    if modified:
        let waste = total_freeblock_space(page)
        let capacity = page_capacity_bytes()
        if waste * 100 / capacity > 30:  // >30% waste → defragment
            defragment(page)
        write_page(current_page_id, page)

    current_page_id = next
```

**Why reverse iteration**: `remove_cell(page, idx)` shifts cell pointers left.
Iterating in reverse ensures already-checked cells don't shift positions.

### Phase 2: Clean secondary indexes

For each secondary index on the clustered table:
```
for each entry in secondary B-tree (full scan):
    decode PK bookmark from entry key
    encode PK as clustered key
    lookup_physical in clustered tree AFTER leaf purge
    if not found:
        add to dead_keys list

bulk_delete dead_keys from secondary B-tree
persist new secondary root if CoW rotates it
```

This reuses the existing `vacuum_index()` pattern, but with **physical clustered
existence after purge**, not `snapshot` visibility. A secondary bookmark entry
must stay alive while its clustered row still exists physically, even if that
row is invisible to the current snapshot.

### Phase 3: Page merge (optional, if page is nearly empty)

After dead cell removal, some leaf pages may be very sparse. If a leaf has
< 25% capacity used, it could be merged with a sibling (using existing 39.8
merge infrastructure). However, merge during VACUUM is complex and risky.

**Decision**: Skip merge in initial VACUUM. Rely on B-tree natural rebalancing
during future inserts. Add merge-on-VACUUM as optimization in future phase.

## Defragmentation heuristic

**When to defragment a page after removing dead cells:**

```
waste_ratio = total_freeblock_space(page) / page_capacity_bytes()

if waste_ratio > 0.30:    // >30% of page is freeblocks
    defragment(page)       // Compact all cells contiguously
```

**Why 30%**: SQLite uses 60 fragment bytes as threshold (but fragments, not freeblocks).
PostgreSQL doesn't defragment during lazy VACUUM (only VACUUM FULL). InnoDB doesn't
defragment at all (B-tree rebalancing). 30% is a balanced threshold — avoids
defragmenting pages with minor waste while reclaiming significantly fragmented pages.

## Acceptance criteria

- [ ] VACUUM on clustered table scans all leaf pages via next_leaf chain
- [ ] Dead cells identified: `txn_id_deleted != 0 && txn_id_deleted < oldest_safe_txn`
- [ ] Dead cells physically removed via `remove_cell()`
- [ ] Overflow chains freed via `free_chain()` for dead overflow-backed cells
- [ ] Page defragmented when freeblock waste > 30% of capacity
- [ ] Secondary index entries cleaned (full scan + clustered physical-existence check after purge)
- [ ] Any secondary root changes from CoW bulk-delete persisted to the catalog
- [ ] `ensure_heap_runtime` guard removed for VACUUM on clustered tables
- [ ] Existing heap VACUUM unchanged
- [ ] Integration test: INSERT 10 → DELETE 5 → VACUUM → verify space reclaimed
- [ ] Integration test: overflow-backed rows cleaned after VACUUM
- [ ] Integration test: secondary index entries cleaned after VACUUM
- [ ] Integration test: VACUUM on empty table → no error
- [ ] Integration test: VACUUM when no dead cells → no changes

## Out of scope

- Background auto-VACUUM thread (future optimization)
- Page merge during VACUUM (rely on B-tree natural rebalancing)
- On-access page pruning during SELECT/UPDATE
- Visibility map (skip pages known to be all-visible)
- VACUUM FULL (complete table rewrite for maximum compaction)

## Dependencies

- 39.1 (Clustered leaf) — `remove_cell()`, `defragment()`, `free_space()` ✅
- 39.7 (Delete mark) — dead cells have `txn_id_deleted` set ✅
- 39.10 (Overflow) — `free_chain()` for overflow cleanup ✅
- 39.15-39.17 (Executor integration) — dead cells from DELETE/UPDATE ✅
