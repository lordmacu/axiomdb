# Spec: 39.8 Clustered structural rebalance

## What to build (not how)

Build the first clustered-tree structural rebalance layer for variable-size
pages and use it to unblock updates whose replacement row no longer fits in the
owning leaf.

This subphase must add clustered-tree logic that:

- repairs parent separators when a physical clustered row removal changes the
  first key of a child page
- redistributes or merges sibling clustered leaf pages based on encoded byte
  volume, not key count
- redistributes or merges sibling clustered internal pages while preserving the
  `n keys -> n + 1 children` contract
- collapses the root when structural shrink leaves an internal root with zero
  separator keys
- provides a clustered update path that can relocate a row when the `39.6`
  same-leaf rewrite path fails because the replacement no longer fits in the
  owning leaf

The implementation must remain storage-first:

- it must not wire clustered tables into the SQL executor yet
- it must not physically purge delete-marked rows from `39.7`
- it must not add WAL / undo / recovery semantics yet

## Inputs / Outputs

- Input:
  - `&mut dyn StorageEngine`
  - current clustered root page id
  - primary key bytes: `&[u8]`
  - replacement row payload bytes: `&[u8]`
  - update transaction id: `u64`
  - `&TransactionSnapshot`
- Output:
  - a dedicated structural-update result that distinguishes:
    - tree empty / key absent / current inline version invisible
    - row updated successfully with the effective clustered root page id after relocation or split/merge
- Errors:
  - malformed/corrupt clustered leaf or internal page layout
  - invalid page-type mismatch during clustered traversal
  - page-not-found when the root or a child pointer references a missing page
  - `DbError::ValueTooLarge` when the replacement payload cannot fit on an otherwise empty clustered leaf for the given key
  - storage I/O errors while reading or writing clustered pages

## Use cases

1. A visible row is updated and the replacement row still fits in the same leaf: the operation succeeds without structural rebalance.
2. A visible row is updated and the replacement row no longer fits in the same leaf: the old physical cell is removed, the row is reinserted, and the tree rebalances as needed.
3. Physical removal of the first row in a non-leftmost leaf changes that leaf's first key: the parent separator is repaired.
4. Physical removal leaves a clustered leaf underfull: it redistributes from a sibling when possible, or merges when both siblings fit in one page.
5. Structural shrink leaves the root internal page with zero separator keys: the root collapses to its only child.

## Acceptance criteria

- [ ] A clustered-tree structural rebalance path exists and is separate from the fixed-layout `axiomdb-index::BTree`.
- [ ] Leaf sibling redistribution and merge decisions use encoded byte volume, not key count.
- [ ] Internal sibling redistribution and merge preserve sorted separators and `n keys -> n + 1 children`.
- [ ] Leaf merge preserves the `next_leaf` chain.
- [ ] Parent separator keys are repaired when the first key of a child changes after physical row removal or redistribution.
- [ ] Root collapse happens when an internal clustered root ends with zero separator keys.
- [ ] A clustered update path exists that can relocate a row when `39.6` same-leaf rewrite fails.
- [ ] The relocation update path still treats empty tree, missing key, and invisible current inline version as non-mutating outcomes.
- [ ] Unit/integration tests cover leaf redistribution, leaf merge, internal redistribution or merge, separator repair, root collapse, and update relocation on a split clustered tree.

## Out of scope

- physical purge of delete-marked rows from `39.7`
- reuse of rebalance primitives by VACUUM / purge
- clustered delete-mark cleanup based on oldest-safe snapshot
- WAL logging or crash recovery for clustered split / merge / relocate-update
- undo logging or rollback restore for clustered split / merge / relocate-update
- executor integration for SQL `UPDATE` or `DELETE`
- secondary-index maintenance for clustered row relocation

## Dependencies

- `specs/fase-39/spec-39.6-clustered-btree-update-in-place.md`
- `specs/fase-39/spec-39.7-clustered-btree-delete.md`
- `crates/axiomdb-storage/src/clustered_tree.rs`
- `crates/axiomdb-storage/src/clustered_leaf.rs`
- `crates/axiomdb-storage/src/clustered_internal.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-core/src/error.rs`

## Research citations

- `research/sqlite/src/btree.c` — borrow the idea that rebalance is triggered by
  page occupancy, not by a fixed notion of key-count underflow, and that
  structural delete/update work must distinguish “no rebalance required” from
  “tree surgery required”.
- `research/mariadb-server/storage/innobase/btr/btr0btr.cc` — borrow the
  merge-feasibility mindset: sibling merge is valid only when the combined page
  contents fit after reorganization.
- `research/mariadb-server/storage/innobase/btr/btr0cur.cc` — borrow the
  separation between logical delete-mark and later page compression / merge.
- `crates/axiomdb-index/src/tree.rs` — borrow the parent-maintenance pattern:
  separator repair, sibling rebalance, and root collapse are tree concerns,
  not page-local concerns.
- AxiomDB adaptation:
  clustered pages are variable-size, so rebalance decisions are made on encoded
  byte volume and page rebuild feasibility, not on `MIN_KEYS_*` constants.

## ⚠️ DEFERRED

- reuse of structural rebalance for clustered purge of delete-marked rows → pending in 39.18
- WAL / undo / recovery for clustered split, merge, and relocate-update → pending in 39.11 and 39.12
- clustered overflow relocation for rows larger than one leaf page → pending in 39.10
- secondary-index bookmark maintenance during clustered row relocation → pending in 39.9
- parent separator growth that would itself split the current internal page during separator repair → pending in 39.10
- executor integration for clustered `UPDATE` / `DELETE` → pending in 39.16 and 39.17
