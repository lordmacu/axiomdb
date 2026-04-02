# Spec: 39.2 Clustered internal page format

## What to build (not how)

Build the storage-layer page format for clustered B-tree internal nodes.

This subphase must introduce a new clustered internal page type with:

- variable-size separator keys
- `n` separator keys and `n + 1` logical child pointers
- binary-search navigation semantics equivalent to the current internal B-tree pages
- page-local operations required by the clustered storage rewrite:
  - initialize an empty clustered internal page
  - read key `i`
  - read child `i`
  - update child `i`
  - find the child index for a search key
  - insert a separator key plus right-child pointer at position `pos`
  - remove a separator key and one logical child pointer
  - report free space and defragment the page

The new format must live in the storage layer, because Phase 39 is rewriting storage around a clustered index model instead of extending the existing heap+index path.

## Inputs / Outputs

- Input:
  - `&Page` and `&mut Page`
  - `usize` logical key/child positions
  - `&[u8]` separator keys
  - `u64` child page identifiers
- Output:
  - `usize` / `u16` page-local counts and positions
  - `&[u8]` separator key slices
  - `u64` logical child page identifiers
  - `Result<(), DbError>` for mutating operations
  - `Result<usize, usize>` for binary search (`Ok(exact)` / `Err(insert_pos)` style where appropriate)
- Errors:
  - out-of-range logical key/child access
  - page full when a new cell plus pointer cannot fit
  - malformed/corrupt clustered internal page layout

## Use cases

1. Initialize a new clustered internal root page with a leftmost child and zero separator keys.
2. After a child split, insert `(separator_key, right_child_pid)` at the parent position without changing traversal semantics.
3. During merge/redistribution, remove a separator key and one logical child pointer while preserving sorted order and `n keys -> n+1 children`.
4. Search for a key using binary search and return the same child slot that the current internal B-tree page would choose.
5. Defragment a fragmented internal page so future inserts can reuse contiguous space.

## Acceptance criteria

- [ ] `PageType::ClusteredInternal` exists in storage page metadata.
- [ ] A new storage module implements clustered internal page-local APIs.
- [ ] Separator keys are variable-size and are stored through a cell pointer array layout.
- [ ] Logical child access supports `n + 1` children with semantics equivalent to the current internal page API.
- [ ] Binary search returns the first separator strictly greater than the search key.
- [ ] Insert/remove operations preserve sorted keys and correct child mapping.
- [ ] Free-space accounting and defragmentation work on fragmented pages.
- [ ] Unit tests cover mixed key sizes, boundary positions, child mapping, page-full behavior, and defragmentation.

## Out of scope

- Replacing `axiomdb-index::BTree` with the clustered tree implementation
- Page split/merge policy across multiple pages
- WAL records for clustered internal page mutations
- MVCC or lock/latch behavior
- Secondary index bookmark changes

## Dependencies

- Phase 39.1 clustered leaf page format
- Existing `Page` / `PageType` primitives in `axiomdb-storage`

## ⚠️ DEFERRED

- Wiring clustered internal pages into the clustered insert path → pending in 39.3
- Lookup/range scan traversal over clustered internal pages → pending in 39.4 and 39.5
- Parent split/merge maintenance across clustered internal pages → pending in 39.8
- WAL logging for clustered internal page mutations → pending in the clustered WAL subphase
