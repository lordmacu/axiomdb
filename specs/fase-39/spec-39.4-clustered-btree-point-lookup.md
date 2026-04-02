# Spec: 39.4 Clustered B-Tree point lookup

## What to build (not how)

Build the first tree-level read path for clustered storage: find a single row by
primary-key bytes in the clustered B-tree and return the row stored inline in
the leaf page.

This subphase must add a dedicated clustered-tree point lookup path that:

- accepts the current clustered root page id, or absence of a root for empty-tree lookup
- descends clustered internal pages using binary search
- performs exact-key search on the clustered leaf cell pointer array
- returns the full row directly from the clustered leaf page without heap indirection
- applies MVCC visibility using `RowHeader::is_visible(&TransactionSnapshot)`
- returns `None` when the current inline version is not visible in 39.4 because clustered undo/version-chain reconstruction is not implemented yet

The implementation must remain storage-first: it builds a clustered read path on
top of the new page primitives without replacing the existing heap+index executor yet.

## Inputs / Outputs

- Input:
  - `&dyn StorageEngine`
  - current clustered root page id, or absence of a root for an empty clustered tree
  - `&[u8]` primary-key bytes
  - `&TransactionSnapshot`
- Output:
  - `Result<Option<ClusteredRow>, DbError>`
  - `None` when the tree is empty
  - `None` when the key is absent
  - `None` when the key exists but the current inline version is not visible to the supplied snapshot
  - `Some(ClusteredRow)` when the row exists and is visible
- Returned row:
  - `key: Vec<u8>`
  - `row_header: RowHeader`
  - `row_data: Vec<u8>`
- Errors:
  - malformed/corrupt clustered leaf or internal page layout
  - invalid page-type mismatch during clustered traversal
  - page-not-found when the root or a child pointer references a missing page

## Use cases

1. Lookup in an empty clustered tree returns `None`.
2. Lookup in a single-leaf clustered tree returns the full row inline.
3. Lookup in a multi-level clustered tree descends internal pages and finds the target row.
4. Lookup for a missing key returns `None`.
5. Lookup for a row whose current inline version is not visible to the snapshot returns `None`.
6. Lookup after many inserts and root splits still returns the correct row for arbitrary keys.

## Acceptance criteria

- [ ] A dedicated clustered-tree lookup API exists and is separate from the current fixed-layout B-tree path.
- [ ] Lookup in an empty clustered tree returns `Ok(None)`.
- [ ] Point lookup descends clustered internal pages in `O(log n)` page visits.
- [ ] Exact-key search on clustered leaves uses the sorted cell pointer array, not a leaf-chain scan.
- [ ] A hit returns the inline row bytes and `RowHeader` directly from the clustered leaf path, not a heap `RecordId`.
- [ ] MVCC visibility is checked with `RowHeader::is_visible(&TransactionSnapshot)`.
- [ ] When the current inline version is not visible, 39.4 returns `Ok(None)` instead of inventing undo/version-chain behavior.
- [ ] Unit/integration tests cover empty tree, root-as-leaf hit, internal-page hit, miss, invisible current version, and lookups after root splits.

## Out of scope

- undo/version-chain traversal for older visible versions
- executor integration for SQL `SELECT`
- clustered secondary-index bookmark lookup
- range scan / leaf-chain iteration
- WAL logging or recovery changes
- update/delete semantics on clustered rows
- benchmarking and user-visible performance claims

## Dependencies

- `specs/fase-39/spec-39.3-clustered-btree-insert.md`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/clustered_internal.rs`
- `crates/axiomdb-storage/src/heap.rs`
- `crates/axiomdb-core/src/traits.rs`

## ⚠️ DEFERRED

- older-version reconstruction through clustered undo/version chains → pending in 39.6, 39.7, 39.11, and 39.12
- range scan and ordered iteration over the leaf chain → pending in 39.5
- executor integration for clustered `SELECT` → pending in 39.15
- secondary-index-to-clustered lookup path → pending in 39.9
