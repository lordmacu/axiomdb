# Plan: 39.6 Clustered B-Tree update in place

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_leaf.rs` — add a page-local rewrite primitive for replacing one existing clustered cell while preserving its key and logical slot
- `crates/axiomdb-storage/src/clustered_tree.rs` — add `update_in_place(...)`, reuse clustered descent helpers, map page-local rewrite outcomes to tree-level semantics, and add unit coverage
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs` — add integration coverage for same-leaf growth updates and no-fit failures on a split clustered tree

## Algorithm / Data structure

Use a two-tier update path:

1. Tree controller finds the exact leaf and checks current-row visibility.
2. Leaf primitive rewrites the cell while preserving key order and leaf ownership.

Core API shape:

```text
clustered_tree::update_in_place(
  storage,
  root_opt,
  key,
  new_row_data,
  txn_id,
  snapshot,
) -> Result<bool, DbError>
```

Leaf primitive shape:

```text
clustered_leaf::rewrite_cell_same_key(
  page,
  pos,
  expected_key,
  new_row_header,
  new_row_data,
) -> Result<Option<Vec<u8>>, DbError>
```

Where:

- `Some(old_cell_image)` means the rewrite succeeded and returns the previous
  encoded cell image for future WAL/undo work.
- `None` means the new cell does not fit in the same leaf page even after
  reclaiming the old cell budget and rebuilding the page.

Pseudocode:

```text
update_in_place(storage, root_opt, key, new_row_data, txn_id, snapshot):
  validate_inline_row(key, new_row_data)
  if root_opt is None:
    return Ok(false)

  leaf = descend_to_leaf(root_pid, key)
  pos = exact leaf search
  if key missing:
    return Ok(false)

  old_cell = read_cell(leaf, pos)
  if !old_cell.row_header.is_visible(snapshot):
    return Ok(false)

  new_header = RowHeader {
    txn_id_created: txn_id,
    txn_id_deleted: 0,
    row_version: old_cell.row_header.row_version + 1,
    _flags: old_cell.row_header._flags,
  }

  page = owned leaf page
  rewritten = clustered_leaf::rewrite_cell_same_key(page, pos, key, new_header, new_row_data)
  match rewritten:
    Some(_) -> write page, return Ok(true)
    None -> return Err(DbError::HeapPageFull { ...same-leaf growth impossible... })
```

Leaf rewrite strategy:

```text
rewrite_cell_same_key(page, pos, expected_key, new_header, new_row_data):
  old_cell = read_cell(page, pos)
  verify old_cell.key == expected_key
  old_image = encoded bytes of old cell
  new_cell_size = encoded size(new_header, expected_key, new_row_data)

  if new_cell_size <= old_cell_size:
    overwrite cell body in place
    zero unused suffix if the new payload is smaller
    keep the same pointer slot and body offset
    return Some(old_image)

  cells = collect all live cells as owned images
  replace cells[pos] with the new image
  rebuild the same leaf page with the same next_leaf pointer
  if rebuild fits:
    return Some(old_image)
  else:
    restore original page bytes and return None
```

Important invariants:

- logical key order must remain unchanged
- pointer-array position of the key stays the same
- `next_leaf` stays unchanged
- no parent separator changes
- success never allocates a new page

## Implementation phases

1. Add clustered-leaf helpers for extracting one cell image and rewriting one existing cell while preserving key identity.
2. Add the tree-level `update_in_place(...)` API and wire it to exact-key descent plus MVCC visibility checks.
3. Implement the two rewrite paths:
   - overwrite-in-place fast path when the new encoded cell fits the old budget
   - rebuild-the-same-leaf fallback when growth still fits on the leaf after compaction
4. Surface explicit same-leaf failure as `DbError::HeapPageFull`.
5. Add unit and integration tests, then update docs and memory on close.

## Tests to write

- unit: update on empty tree returns `false`
- unit: missing key returns `false`
- unit: invisible current version returns `false`
- unit: same-size rewrite preserves key and bumps `row_version`
- unit: larger rewrite succeeds when it fits after rebuilding the same leaf
- unit: no-fit rewrite returns `DbError::HeapPageFull`
- unit: successful update does not change `next_leaf`
- integration: update a row on a multi-leaf clustered tree and verify lookup/range expose the new inline bytes
- integration: attempt a growth update on a split tree that cannot stay in the same leaf and verify explicit failure
- bench: none in 39.6; clustered write benchmarks stay deferred until SQL-visible clustered DML exists

## Anti-patterns to avoid

- Do not implement `39.6` as delete+insert through the whole tree.
- Do not change the primary key during update.
- Do not split or merge leaves as part of this subphase.
- Do not overwrite an invisible current version.
- Do not introduce executor-facing clustered update behavior yet.
- Do not hide “same leaf no longer fits” behind `false`; it must be an explicit error.

## Risks

- Off-by-one corruption in variable-size cell rewrite → mitigate with direct old/new image assertions and post-update range-order tests.
- Accidentally changing leaf order or `next_leaf` → mitigate with invariants and tests that compare full leaf-chain order before/after update.
- Rebuild fallback mutating the page on failure → mitigate by rebuilding on a temporary owned page first or restoring the original bytes before returning `None`.
- Overwriting invisible versions and breaking future MVCC expectations → mitigate by checking visibility in the tree controller before any page mutation.

## Research citations

- `research/sqlite/src/btree.c` — adapt the overwrite optimization: use the
  cheapest direct rewrite when the replacement cell fits the current budget.
- `research/postgres/src/backend/access/heap/heapam.c` — take the lesson that
  in-place update is a special path with explicit invariants and future WAL
  implications, not generic row mutation.
- Rejected:
  - no full SQLite-style delete+insert table update path
  - no PostgreSQL HOT/version-chain machinery yet
  - no executor-visible clustered update in this subphase
