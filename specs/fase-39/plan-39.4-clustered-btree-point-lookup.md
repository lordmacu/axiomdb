# Plan: 39.4 Clustered B-Tree point lookup

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_tree.rs` — add `ClusteredRow`, point-lookup API, root-to-leaf descent helpers, and snapshot visibility filtering
- `crates/axiomdb-storage/src/lib.rs` — no API rename expected, only re-export continues to expose `clustered_tree`
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs` — add point-lookup integration coverage over a tree built by clustered inserts

## Algorithm / Data structure

Use a dedicated read path in the same `clustered_tree` module introduced in 39.3.

Core API shape:

```text
ClusteredRow {
  key: Vec<u8>,
  row_header: RowHeader,
  row_data: Vec<u8>,
}

lookup(storage, root_opt, key, snapshot) -> Result<Option<ClusteredRow>, DbError>
```

Pseudocode:

```text
lookup(storage, root_opt, key, snapshot):
  if root_opt is None:
    return Ok(None)

  pid = root_opt
  loop:
    page = read_page(pid)
    switch page_type:
      ClusteredInternal:
        child_idx = find_child_idx(page, key)
        pid = child_at(page, child_idx)
      ClusteredLeaf:
        break
      other:
        return corruption error

  pos = exact_search_in_leaf(page, key)
  if miss:
    return Ok(None)

  cell = read_cell(page, pos)
  if !cell.row_header.is_visible(snapshot):
    return Ok(None)

  return Ok(Some(ClusteredRow {
    key: cell.key.to_vec(),
    row_header: cell.row_header,
    row_data: cell.row_data.to_vec(),
  }))
```

Implementation notes:

- Add one private helper for root-to-leaf descent; 39.5/39.6 can reuse its shape later.
- Reuse page-local binary-search primitives:
  - `clustered_internal::find_child_idx`
  - `clustered_internal::child_at`
  - clustered leaf exact search helper already used by 39.3
- Do not scan `next_leaf` for point lookup; this must stay a true tree descent.
- Return owned row bytes because `PageRef` ownership ends when `lookup()` returns.

## Implementation phases

1. Add `ClusteredRow` and public `lookup(...)` API to `clustered_tree.rs`.
2. Implement root-to-leaf descent over clustered internal pages.
3. Reuse exact leaf search and convert a hit into `ClusteredRow`.
4. Apply `RowHeader::is_visible(snapshot)` and define the 39.4 invisible-row behavior as `Ok(None)`.
5. Add unit/integration tests for empty tree, miss, visible hit, invisible hit, and multi-level lookup after many inserts.

## Tests to write

- unit: empty clustered tree returns `None`
- unit: root-as-leaf lookup returns full inline row
- unit: missing key returns `None`
- unit: invisible current inline version returns `None`
- unit: multi-level tree lookup finds a row after root/internal splits
- integration: build a 10K-row clustered tree through `insert(...)`, then probe a representative set of keys and verify exact row bytes
- bench: none in 39.4; point/range read benchmarking starts after 39.5 exposes both lookup and scan paths

## Anti-patterns to avoid

- Do not genericize the old `axiomdb-index::BTree` over clustered pages in this subphase.
- Do not follow `next_leaf` during exact point lookup.
- Do not invent undo/version-chain traversal before clustered undo exists.
- Do not return borrowed slices tied to a `PageRef` that dies at function exit.
- Do not route lookup through heap `RecordId` or any current executor path.

## Risks

- Returning `None` for an invisible current version could be confused with “key absent” → mitigate with explicit spec wording and tests that target invisibility.
- Page-type mismatch in traversal could silently hide corruption → mitigate by returning `DbError::BTreeCorrupted`.
- Future range-scan/update/delete work may need lookup-position helpers → mitigate by keeping descent and exact-search helpers local and reusable instead of inlining everything into the public API.
