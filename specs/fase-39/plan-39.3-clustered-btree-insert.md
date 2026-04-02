# Plan: 39.3 Clustered B-Tree insert

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_tree.rs` — dedicated clustered-tree insert path, root bootstrap, descent, leaf/internal split propagation
- `crates/axiomdb-storage/src/lib.rs` — export the clustered tree module
- `crates/axiomdb-storage/src/clustered_leaf.rs` — expose any small helpers needed for split planning / tests without changing the existing page semantics
- `crates/axiomdb-storage/src/clustered_internal.rs` — expose any small helpers needed for internal split planning / root bootstrap

## Algorithm / Data structure

Use a dedicated tree controller above the existing clustered page primitives.

Core result shape:

```text
InsertResult =
  Ok { root_pid }
  Split {
    left_pid,
    right_pid,
    sep_key
  }
```

Pseudocode:

```text
insert(root_opt, key, row_header, row_data):
  if root_opt is None:
    root = alloc ClusteredLeaf
    init leaf
    insert cell at pos 0
    write page
    return root

  result = insert_subtree(root_pid, key, row_header, row_data)
  match result:
    Ok { root_pid } -> return root_pid
    Split { left_pid, right_pid, sep_key }:
      new_root = alloc ClusteredInternal
      init internal with leftmost_child = left_pid
      insert separator at pos 0 with right_child = right_pid
      write new root
      return new_root

insert_subtree(pid, key, row_header, row_data):
  page = read pid
  switch page_type:
    ClusteredLeaf:
      pos = search(key)
      if duplicate -> DuplicateKey
      try insert_cell
      if success -> write same pid, return Ok { pid }
      if HeapPageFull:
        defragment once
        retry insert_cell
        if success -> write same pid, return Ok { pid }
        else -> split leaf by byte volume and return Split

    ClusteredInternal:
      child_idx = find_child_idx(key)
      child_pid = child_at(child_idx)
      child_result = insert_subtree(child_pid, ...)
      if child_result is Ok { same pid } -> return Ok { pid }
      if child_result is Ok { new pid }:
        update child pointer in place
        write same pid
        return Ok { pid }
      if child_result is Split { left_pid, right_pid, sep_key }:
        set_child_at(child_idx, left_pid)
        try insert_at(child_idx, sep_key, right_pid)
        if success -> write same pid, return Ok { pid }
        if HeapPageFull:
          defragment once
          retry insert_at
          if success -> write same pid, return Ok { pid }
          else -> split internal and return Split
```

Leaf split rule:

- materialize all existing leaf cells plus the new row
- sort by key order via logical pointer order
- choose the split point by cumulative encoded cell bytes so the two resulting
  pages are as close as possible in occupied byte volume
- separator key = first key stored on the right page
- left page keeps `next_leaf = right_pid`
- right page inherits the old `next_leaf`

Internal split rule:

- materialize:
  - `leftmost_child`
  - all separator cells `(key, right_child)`
  - the incoming `(sep_key, right_child)` at `child_idx`
- choose split point by cumulative separator-cell byte volume
- left page keeps separators before `mid`
- promoted separator = key at `mid`
- right page gets separators after `mid`
- right page `leftmost_child` = child immediately to the right of the promoted separator

## Implementation phases

1. Create `clustered_tree.rs` with root bootstrap and `insert(...) -> Result<u64, DbError>`.
2. Implement recursive descent over `ClusteredInternal` and non-split leaf insert over `ClusteredLeaf`.
3. Add defrag-before-split behavior for both leaf and internal pages.
4. Implement clustered leaf split by byte volume and root split handling.
5. Implement clustered internal split and separator propagation.
6. Add tests for empty tree, duplicate key rejection, leaf split, internal split, root split, and 10K-row sorted inserts.

## Tests to write

- unit: insert first row into empty clustered tree creates a clustered leaf root
- unit: duplicate key insert is rejected
- unit: non-split leaf insert preserves sorted order
- unit: fragmented leaf defragments before splitting
- unit: leaf split sets correct separator and `next_leaf` chain
- unit: internal insert absorbs separator when parent has room
- unit: internal split promotes the correct separator and child mapping
- integration: insert 10K rows with mixed row sizes and verify in-order traversal across leaf chain
- integration: repeated root splits still keep all keys reachable in sorted order
- bench: none in 39.3; benchmarking belongs after lookup/scan exist in 39.4/39.5

## Anti-patterns to avoid

- Do not retrofit the current `axiomdb-index::BTree` generic over clustered pages in this subphase.
- Do not split leaves by cell count; split by encoded byte volume.
- Do not skip the defragmentation retry before splitting.
- Do not silently accept rows that require overflow-page support.
- Do not mix WAL/undo concerns into the first clustered insert controller.

## Risks

- Wrong separator selection during byte-volume split → mitigate with tests that verify full sorted traversal after every split shape.
- Child mapping drift in internal split → mitigate by materializing full logical child list before rebuilding left/right pages.
- Hidden dependence on fixed-layout tree helpers → mitigate by keeping the clustered controller independent and reusing only algorithm shape, not page structs.
