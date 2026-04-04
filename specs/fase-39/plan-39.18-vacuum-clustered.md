# Plan: 39.18 — VACUUM for Clustered Index

## Files to modify

| File | Change |
|---|---|
| `crates/axiomdb-sql/src/vacuum.rs` | Finalize clustered VACUUM dispatch, clustered leaf purge, clustered secondary cleanup, and CoW root persistence |
| `crates/axiomdb-catalog/src/writer.rs` | Reuse `update_index_root()` / `update_table_root()` from VACUUM when bulk delete rotates a root |
| `crates/axiomdb-sql/tests/integration_clustered_vacuum.rs` | **NEW**: 5+ integration tests |
| `tools/wire-test.py` | Add clustered `VACUUM` smoke if SQL-visible behavior changes |

## Implementation phases

### Phase 1: Finalize clustered dispatch and root-persistence contract
In `vacuum.rs`, keep the clustered dispatch and thread the active transaction
through vacuum helpers so VACUUM can persist any CoW root rotations:
```rust
if table_def.is_clustered() {
    return vacuum_clustered_table(table_def, indexes, storage, txn, snap, oldest_safe_txn, bloom);
}
```

Also fix the shared VACUUM root-rotation gap in the existing heap/index path:
if `BTree::delete_many_in(...)` rotates an index root, persist the new root
through `CatalogWriter` in the same transaction.

### Phase 2: Implement vacuum_clustered_leaves()

```rust
fn vacuum_clustered_leaves(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
    oldest_safe_txn: u64,
) -> Result<VacuumLeafResult, DbError> {
    let mut dead_count = 0u64;
    let mut pages_modified = 0u64;

    // Find leftmost leaf by descending via child 0.
    let mut current_pid = leftmost_leaf(storage, root_pid)?;

    while current_pid != clustered_leaf::NULL_PAGE {
        let mut page = storage.read_page(current_pid)?.into_page();
        let next_pid = clustered_leaf::next_leaf(&page);
        let mut modified = false;

        // Reverse iteration: remove_cell shifts pointers, reverse avoids skipping.
        let n = clustered_leaf::num_cells(&page) as usize;
        for idx in (0..n).rev() {
            let cell = clustered_leaf::read_cell(&page, idx as u16)?;
            if cell.row_header.txn_id_deleted != 0
                && cell.row_header.txn_id_deleted < oldest_safe_txn
            {
                // Free overflow chain if present.
                if let Some(overflow_pid) = cell.overflow_first_page {
                    clustered_overflow::free_chain(storage, overflow_pid)?;
                }
                clustered_leaf::remove_cell(&mut page, idx)?;
                dead_count += 1;
                modified = true;
            }
        }

        if modified {
            // Conditional defragmentation: compact if >30% waste.
            let waste = clustered_leaf::total_freeblock_space_pub(&page);
            let capacity = clustered_leaf::page_capacity_bytes();
            if capacity > 0 && waste * 100 / capacity > 30 {
                clustered_leaf::defragment(&mut page);
            }
            page.update_checksum();
            storage.write_page(current_pid, &page)?;
            pages_modified += 1;
        }

        current_pid = next_pid;
    }

    Ok(VacuumLeafResult { dead_count, pages_modified })
}
```

### Phase 3: Implement vacuum_clustered_secondary()
Reuse existing `vacuum_index()` pattern but with clustered **physical existence**
check after clustered leaf purge:

```rust
fn vacuum_clustered_secondary(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    index: &IndexDef,
    primary_idx: &IndexDef,
    table_def: &TableDef,
    bloom: &mut BloomRegistry,
) -> Result<u64, DbError> {
    let layout = ClusteredSecondaryLayout::derive(index, primary_idx)?;
    let all_entries = BTree::range_in(storage, index.root_page_id, None, None)?;

    let mut dead_keys: Vec<Vec<u8>> = Vec::new();
    for (_rid, key_bytes) in &all_entries {
        let entry = layout.decode_entry_key(key_bytes)?;
        let pk_key = encode_index_key(&entry.primary_key)?;
        // Check if clustered row still exists physically after purge.
        match clustered_tree::lookup_physical(storage, Some(table_def.root_page_id), &pk_key)? {
            Some(_) => {} // Row alive — keep index entry.
            None => dead_keys.push(key_bytes.clone()), // Row dead — mark for deletion.
        }
    }

    let count = dead_keys.len() as u64;
    if !dead_keys.is_empty() {
        dead_keys.sort_unstable();
        let root = AtomicU64::new(index.root_page_id);
        BTree::delete_many_in(storage, &root, &dead_keys)?;
        let new_root = root.load(Ordering::Relaxed);
        if new_root != index.root_page_id {
            CatalogWriter::new(storage, txn)?.update_index_root(index.index_id, new_root)?;
        }
    }
    Ok(count)
}
```

### Phase 4: Wire together in vacuum_clustered_table()

```rust
fn vacuum_clustered_table(
    table_def: &TableDef,
    indexes: &[IndexDef],
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    snap: TransactionSnapshot,
    oldest_safe_txn: u64,
    bloom: &mut BloomRegistry,
) -> Result<VacuumResult, DbError> {
    // Phase 1: Clustered leaf cleanup.
    let leaf_result = vacuum_clustered_leaves(storage, table_def.root_page_id, oldest_safe_txn)?;

    // Phase 2: Secondary index cleanup.
    let primary_idx = indexes.iter().find(|i| i.is_primary);
    let mut dead_index_entries = 0u64;
    if let Some(pk_idx) = primary_idx {
        for idx in indexes.iter().filter(|i| !i.is_primary && !i.columns.is_empty()) {
            dead_index_entries += vacuum_clustered_secondary(
                storage, txn, idx, pk_idx, table_def, bloom,
            )?;
        }
    }

    Ok(VacuumResult {
        dead_rows: leaf_result.dead_count,
        dead_index_entries,
        pages_modified: leaf_result.pages_modified,
    })
}
```

### Phase 5: Extend validation
Add tests that prove:
- overflow chains are freed after clustered purge
- secondary cleanup uses physical row existence, not snapshot visibility
- secondary root rotations remain queryable after VACUUM
- heap VACUUM still behaves correctly after the shared root-persistence fix

## Tests to write

1. **Basic VACUUM**: INSERT 10 → DELETE 5 → VACUUM → verify 5 dead cells removed
2. **Overflow cleanup**: INSERT large rows → DELETE → VACUUM → overflow pages freed
3. **Secondary index cleanup**: INSERT with secondary index → DELETE → VACUUM → index entries removed
4. **Secondary root persistence**: force secondary cleanup to rotate/shrink the root, then verify index lookups still work
5. **VACUUM on empty table**: no error, 0 dead cells
6. **VACUUM with no dead cells**: all alive → 0 removed
7. **VACUUM after UPDATE**: UPDATE marks old version dead → VACUUM cleans it
8. **Space reclaimed**: after VACUUM + defragment, free_space increases

## Anti-patterns to avoid

- DO NOT vacuum cells where `txn_id_deleted >= oldest_safe_txn` (still visible to some reader)
- DO NOT skip overflow chain cleanup (leaked pages = disk space leak)
- DO NOT defragment unconditionally (waste CPU on pages with minimal fragmentation)
- DO NOT modify page without `update_checksum()` before write
- DO NOT iterate forward during removal (index shift causes cell skip)
- DO NOT decide secondary cleanup from `lookup(..., snapshot)`; purge must use physical existence after clustered leaf cleanup
- DO NOT ignore `delete_many_in()` root rotation; stale catalog roots silently corrupt later scans

## Risks

- **Concurrent access**: Current single-writer model means VACUUM runs exclusively.
  No concurrent reader safety concern yet. Phase 40 will need coordination.
- **Large table scan**: Full leaf chain scan is O(pages). For 1GB table with 16KB pages,
  that's ~64K page reads. Acceptable for on-demand VACUUM.
- **Defragmentation cost**: O(cells) per page copy. With ~100 cells/page, ~100 copies
  per defrag. Negligible vs I/O cost of page read/write.
- **Root rotation during cleanup**: bulk delete can shrink or rotate a secondary root.
  Mitigation: persist the post-delete root through `CatalogWriter` in the same transaction.
