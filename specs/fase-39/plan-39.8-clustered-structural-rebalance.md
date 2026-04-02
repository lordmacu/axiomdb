# Plan: 39.8 Clustered structural rebalance

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_tree.rs` — add structural rebalance helpers for leaf/internal siblings, parent separator repair, root collapse, and the relocate-update controller
- `crates/axiomdb-storage/src/clustered_leaf.rs` — add helpers for extracting owned leaf cells and rebuilding / redistributing sibling pages while preserving `next_leaf`
- `crates/axiomdb-storage/src/clustered_internal.rs` — add helpers for extracting owned internal cells / child mapping and rebuilding merged or redistributed internal siblings
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs` — add relocation-update and post-rebalance shape coverage across split clustered trees

## Algorithm / Data structure

Use byte-volume rebuilds instead of fixed-slot rotations.

### 1. Physical leaf shrink

After an oversized update chooses structural relocation:

- descend to the owning leaf
- remove the old physical cell by exact key
- detect whether the leaf's first key changed
- if the leaf stays sufficiently occupied, only repair ancestor separators as needed
- otherwise, rebalance the leaf with its left or right sibling

### 2. Leaf rebalance

Represent sibling contents as ordered owned cells:

```text
LeafCells = [OwnedLeafCell]
```

Decision rule:

```text
combined = left_cells + right_cells
if combined fits on one page:
  merge into the left page
  left.next_leaf = right.next_leaf
  remove separator + right child from parent
else:
  redistribute by encoded byte volume
  rebuild both siblings
  parent separator becomes first key of right sibling
```

Redistribution target:

- choose the split point that minimizes the byte-volume difference between the two rebuilt siblings
- never produce an empty sibling

### 3. Internal rebalance

Represent an internal sibling pair plus the parent separator between them as:

```text
leftmost_child
[(sep_key_0, right_child_0), ...]
parent_sep
[(right_sep_key_0, right_child_0), ...]
```

Decision rule mirrors leaves:

```text
combined = left cells + parent separator + right cells
if combined fits on one internal page:
  merge into the left page
  remove separator + right child from parent
else:
  redistribute by separator encoded byte volume
  rebuild left and right internal pages
  promote the new first key of the right partition into parent
```

### 4. Root collapse

If rebalancing removes the last separator from the root internal page:

- root must become its only remaining child page id
- the old empty root page is freed

### 5. Relocate-update controller

Public controller shape:

```text
clustered_tree::update_with_relocation(
  storage,
  root_pid,
  key,
  new_row_data,
  txn_id,
  snapshot,
) -> Result<Option<u64>, DbError>
```

Where:

- `None` means empty tree, missing key, or invisible current inline version
- `Some(new_root_pid)` means the row was updated and the returned root is the
  effective clustered root after any split, redistribute, merge, or root collapse

Pseudocode:

```text
update_with_relocation(storage, root_pid, key, new_row_data, txn_id, snapshot):
  validate_inline_row(key, new_row_data)
  try same-leaf update_in_place(...)
  if success:
    return Some(root_pid)
  if not found / invisible:
    return None

  old_cell = locate exact visible row
  physically remove old cell from source leaf
  repair separators and rebalance source path as needed

  insert replacement row with new RowHeader {
    txn_id_created = txn_id
    txn_id_deleted = 0
    row_version = old.row_version + 1
    _flags = old._flags
  }

  return Some(effective_root_pid)
```

Important invariants:

- primary-key order remains globally sorted
- no duplicate live cell with the same key exists after relocate-update completes
- leaf `next_leaf` chain remains correct after merge / redistribute
- parent separators always equal the first key of their right child
- internal pages always preserve `n keys -> n + 1 children`
- root is never an empty internal page after the operation completes

## Implementation phases

1. Add owned-cell extraction and page rebuild helpers for clustered leaf/internal siblings.
2. Add leaf redistribute/merge helpers and parent separator repair helpers.
3. Add internal redistribute/merge helpers and root collapse logic.
4. Add the relocate-update controller that uses `39.6` same-leaf update as the fast path and structural rebalance as the fallback path.
5. Add unit/integration tests for structural shrink, separator repair, root collapse, and relocate-update.

## Tests to write

- unit: leaf removal that changes first key repairs parent separator
- unit: underfull leaf redistributes from sibling when merge does not fit
- unit: underfull leaf merges when combined contents fit in one page
- unit: internal rebalance preserves `n keys -> n + 1 children`
- unit: root collapses to only child after structural shrink
- unit: relocate-update succeeds after `update_in_place` would have returned `HeapPageFull`
- unit: relocate-update preserves sorted leaf-chain order
- integration: relocate-update on a multi-leaf clustered tree returns the correct new root and updated row bytes
- integration: repeated relocate-updates keep the leaf chain ordered and lookups/range scans correct
- bench: none in 39.8; structural clustered write benchmarks stay deferred until SQL-visible clustered DML exists

## Anti-patterns to avoid

- Do not physically purge `39.7` delete-marked rows in this subphase.
- Do not use fixed `MIN_KEYS_*` style thresholds for clustered siblings; use encoded byte volume.
- Do not rebalance variable-size pages by “move one key left/right” loops as if all cells cost the same.
- Do not leave separator repair implicit; update it explicitly whenever a right child's first key changes.
- Do not change SQL-visible executor behavior yet.
- Do not add WAL or undo placeholders that pretend structural writes are crash-safe before 39.11/39.12.

## Risks

- Variable-size redistribution producing a sibling that still does not fit → mitigate by rebuilding from owned cells and validating each page rebuild independently.
- Parent separator drift after first-key change → mitigate with explicit separator-repair helpers and assertions in lookup/range tests.
- Parent separator repair can grow the separator key itself → current 39.8 mitigation is compact rebuild of the same page; split-on-separator-repair stays deferred to 39.10.
- Root collapse bugs leaving an empty internal root → mitigate with a dedicated collapse step and targeted tests.
- Relocate-update temporarily deleting the only visible copy of the row on failure → mitigate by validating row-size constraints before removal and limiting the fallback to cases the rebuild/insert path can represent.

## Research citations

- `research/sqlite/src/btree.c` — adapt the occupancy-driven rebalance trigger and the explicit split between “local delete/update” and “rebalance required”.
- `research/mariadb-server/storage/innobase/btr/btr0btr.cc` — adapt merge-feasibility-after-reorganization for sibling merge decisions.
- `research/mariadb-server/storage/innobase/btr/btr0cur.cc` — reject coupling logical delete-mark directly to merge; keep page compression/merge as separate work.
- `crates/axiomdb-index/src/tree.rs` — adapt the tree-level sequencing of rebalance, parent repair, and root collapse, but not the fixed-key-count heuristics.
