# Plan: 39.7 Clustered B-Tree delete

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_tree.rs` — add `delete_mark(...)`, reuse clustered descent helpers, and map exact-key MVCC checks to in-place delete semantics
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs` — add integration coverage for split-tree delete-mark visibility across old/new snapshots
- `docs/fase-39.md` — record the storage-first delete-mark semantics and targeted validation
- `docs/progreso.md` — close 39.7 and document deferred purge/merge work
- `docs-site/src/internals/storage.md` — document clustered delete-mark behavior and deferred purge
- `docs-site/src/internals/btree.md` — document the tree-level delete controller and invariants
- `docs-site/src/user-guide/features/indexes.md` — reflect that clustered delete primitives now exist internally
- `docs-site/src/development/roadmap.md` — advance clustered-index roadmap state
- `memory/project_state.md` — capture the new clustered mutation slice
- `memory/architecture.md` — describe clustered delete responsibilities
- `memory/lessons.md` — record any new implementation lesson from delete-mark semantics

## Algorithm / Data structure

Use the existing clustered exact-key path and rewrite only the row header:

1. Tree controller finds the exact leaf and checks current-row visibility.
2. The target clustered cell is rewritten in place with the same key and row payload.
3. Only `txn_id_deleted` changes; physical row bytes remain present on the page.

Core API shape:

```text
clustered_tree::delete_mark(
  storage,
  root_opt,
  key,
  txn_id,
  snapshot,
) -> Result<bool, DbError>
```

Pseudocode:

```text
delete_mark(storage, root_opt, key, txn_id, snapshot):
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
    txn_id_created: old_cell.row_header.txn_id_created,
    txn_id_deleted: txn_id,
    row_version: old_cell.row_header.row_version,
    _flags: old_cell.row_header._flags,
  }

  page = owned leaf page
  rewrite_cell_same_key(page, pos, key, new_header, old_cell.row_data)
  write page
  return Ok(true)
```

Why reuse `rewrite_cell_same_key(...)`:

- delete-mark is a header-only mutation, so the encoded cell size does not change
- the existing rewrite primitive already preserves key order and logical slot ownership
- the fast path writes in place without rebuilding the page when size is unchanged

Important invariants:

- primary-key order does not change
- key bytes and row payload bytes do not change
- `txn_id_created`, `row_version`, and `_flags` remain unchanged
- `txn_id_deleted` becomes the deleting transaction id
- `next_leaf` and parent separators remain unchanged
- success never allocates a new page
- old snapshots can still see the row because the physical cell remains inline

## Implementation phases

1. Add the tree-level `delete_mark(...)` API and wire it to exact-key descent plus MVCC visibility checks.
2. Reuse the clustered-leaf rewrite primitive to stamp `txn_id_deleted` without changing cell size or tree structure.
3. Add unit tests for empty-tree, missing-key, invisible-row, and post-delete visibility behavior.
4. Add integration tests over a split clustered tree validating lookup/range behavior for old and new snapshots.
5. Update docs and memory on close.

## Tests to write

- unit: delete on empty tree returns `false`
- unit: delete on missing key returns `false`
- unit: delete on invisible current version returns `false`
- unit: delete-mark preserves key bytes and row payload bytes
- unit: delete-mark preserves `row_version`
- unit: deleting transaction no longer sees the row via lookup
- unit: an older snapshot still sees the row via lookup after delete-mark
- unit: delete-mark does not change `next_leaf`
- integration: delete-mark a row on a multi-leaf clustered tree and verify current snapshots skip it in lookup/range
- integration: verify an older snapshot taken before the delete still sees the row in lookup/range
- bench: none in 39.7; clustered delete benchmarks stay deferred until SQL-visible clustered DML exists

## Anti-patterns to avoid

- Do not physically remove the clustered cell in 39.7.
- Do not trigger page merge, rebalance, or freelist changes in this subphase.
- Do not overwrite `txn_id_created` or bump `row_version` on delete-mark.
- Do not hide invisible-row behavior behind corruption or `AlreadyDeleted`; return `false`.
- Do not add executor-facing clustered delete behavior yet.
- Do not add purge/vacuum policy here; that belongs to 39.18.

## Risks

- Accidentally mutating row bytes or key bytes during header rewrite → mitigate with direct before/after assertions.
- Breaking old-snapshot visibility by physically removing the cell → mitigate by keeping the row inline and verifying old/new snapshots in both lookup and range tests.
- Rewriting an invisible current version and masking future MVCC work → mitigate by checking `RowHeader::is_visible` before any page mutation.
- Coupling delete to merge/rebalance too early → mitigate by making tree structure explicitly immutable on success in this subphase.

## Research citations

- `research/mariadb-server/storage/innobase/btr/btr0cur.cc` — adapt the
  delete-mark-in-place model for clustered records.
- `research/mariadb-server/storage/innobase/read/read0read.cc` — adapt the
  rule that delete-marked rows remain physically present until purge knows no
  snapshot can observe them.
- `research/postgres/src/backend/access/heap/heapam.c` — keep logical delete
  separate from later physical cleanup responsibilities.
- Rejected:
  - no immediate cell removal
  - no page merge or underflow handling in 39.7
  - no undo/WAL plumbing in this subphase
