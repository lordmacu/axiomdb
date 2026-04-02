# Spec: 39.3 Clustered B-Tree insert

## What to build (not how)

Build the first tree-level write path for the clustered storage engine: insert a
full row into a clustered B-tree where leaf pages store the primary key and row
payload inline.

This subphase must add a dedicated clustered-tree insert path that:

- bootstraps an empty clustered tree root
- descends internal clustered pages using binary search
- inserts a `(key, row_header, row_data)` cell into a clustered leaf in sorted order
- defragments a leaf before splitting when fragmentation is the only reason the row does not fit
- splits clustered leaf pages by data volume, not by cell count
- propagates `(separator_key, right_child_pid)` into clustered internal pages
- splits clustered internal pages when needed and creates a new root when the old root splits

The implementation must remain storage-first: it builds a clustered tree path on
top of the new page primitives without replacing the existing heap+index engine yet.

## Inputs / Outputs

- Input:
  - `&mut dyn StorageEngine`
  - current clustered root page id, or absence of a root for bootstrap
  - `&[u8]` primary-key bytes
  - `&RowHeader`
  - `&[u8]` row payload bytes
- Output:
  - `Result<u64, DbError>` returning the effective root page id after the insert
  - same page id when the root does not change
  - a new page id when root split creates a new clustered internal root
- Errors:
  - duplicate key
  - page full when a row cannot fit even after defragmentation and the row is too large for the non-overflow scope of 39.3
  - malformed/corrupt clustered leaf or internal page layout
  - invalid page-type mismatch during clustered traversal

## Use cases

1. Insert into an empty clustered tree: allocate a clustered leaf root and write the first row inline.
2. Insert into a single leaf where the row fits contiguously: update the page in place.
3. Insert into a fragmented leaf: defragment first, then insert without splitting.
4. Insert into a full leaf: split the leaf by cumulative byte volume, then insert the separator into the parent.
5. Insert into a tree whose parent also overflows: split clustered internal pages and create a new root.
6. Insert 10K rows with mixed row sizes and verify that an in-order traversal sees sorted keys.

## Acceptance criteria

- [ ] A dedicated clustered-tree insert API exists and is separate from the current fixed-layout B-tree path.
- [ ] Inserting into an empty clustered tree creates a clustered leaf root and returns its page id.
- [ ] Non-split clustered leaf inserts preserve sorted key order.
- [ ] If a leaf insert fails because of fragmentation, the implementation defragments once and retries before splitting.
- [ ] Leaf splits distribute cells by approximate byte volume, not by cell count.
- [ ] Parent updates use clustered internal pages and preserve `n keys -> n+1 children` semantics.
- [ ] Root split creates a new clustered internal root with the correct left and right child pointers.
- [ ] Duplicate primary keys are rejected.
- [ ] Rows that require overflow-page support are rejected explicitly in 39.3 and left deferred to 39.10.
- [ ] Unit/integration tests cover empty tree, non-split insert, defrag-before-split, leaf split, internal split, root split, duplicate key, and 10K-row sorted verification.

## Out of scope

- WAL logging for clustered inserts
- undo/rollback handlers for clustered inserts
- overflow pages for large rows
- point lookup and range scan public APIs
- executor integration for SQL `INSERT`
- page merge / delete / vacuum

## Dependencies

- `specs/fase-39/spec-39.2-clustered-internal-page.md`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/clustered_internal.rs`
- `crates/axiomdb-index/src/tree.rs` for traversal and split-shape reference only

## ⚠️ DEFERRED

- WAL and undo semantics for clustered inserts → pending in 39.11 and 39.12
- overflow-row support → pending in 39.10
- clustered point lookup / range scan APIs → pending in 39.4 and 39.5
- executor integration for clustered tables → pending in 39.14
