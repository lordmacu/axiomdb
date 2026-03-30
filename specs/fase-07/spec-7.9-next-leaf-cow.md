# Spec: 7.9 — Resolve next_leaf CoW Gap

## Context

AxiomDB's B+ Tree uses Copy-on-Write (CoW): every write creates new pages
instead of modifying existing ones. Leaf pages have a `next_leaf` pointer
linking them in ascending key order. When a leaf splits under CoW, the
predecessor leaf's `next_leaf` becomes stale — it points to the old (freed)
page ID instead of the new left page.

Currently, `range_in` avoids this by re-descending the tree from the root
at every leaf boundary (O(log n) per boundary). This is correct but
suboptimal for large range scans.

**Reference:** PostgreSQL maintains correct sibling pointers by updating 3
pages atomically in a WAL critical section. SQLite avoids the problem entirely
by using parent-descent (no leaf chain). AxiomDB will fix the pointers
during split/merge, enabling O(1) leaf-chain traversal.

---

## What to build

After every leaf split or merge, update the predecessor leaf's `next_leaf`
pointer so the leaf chain remains correct under CoW.

### Split

Before: `[Pred] --next→ [Original(pid=X)]`
After:  `[Pred'] --next→ [Left(pid=Y)] --next→ [Right(pid=Z)]`

The predecessor must be found via the parent node and updated. Since CoW
writes a new page for the predecessor, this is a `read_page + set_next_leaf
+ write_page` on the predecessor.

### Merge

Before: `[Pred] --next→ [Left] --next→ [Right] --next→ [Successor]`
After:  `[Pred'] --next→ [Merged] --next→ [Successor]`

Same pattern: find and update predecessor.

### Range scan optimization

Once `next_leaf` is always correct, `range_in` can follow leaf pointers
directly (O(1) per boundary) instead of re-descending the tree (O(log n)).

---

## Acceptance Criteria

- [ ] After leaf split, predecessor leaf's `next_leaf` points to new left page
- [ ] After leaf merge, predecessor leaf's `next_leaf` points to merged page
- [ ] `range_in` uses `next_leaf` for leaf-to-leaf traversal (O(1))
- [ ] Range scan results identical to tree-traversal approach
- [ ] No stale `next_leaf` pointers after any sequence of inserts/deletes
- [ ] Existing range scan tests pass
- [ ] `cargo test -p axiomdb-index` passes clean
- [ ] `cargo test --workspace` passes clean

---

## Out of Scope

- **Doubly-linked leaves (prev_leaf):** not adding reverse pointers
- **Concurrent split safety:** under RwLock, only one writer at a time
- **next_leaf for non-leaf (internal) nodes:** not applicable

## Dependencies

- B-Tree split/merge logic in `tree.rs` — already exists
- `LeafNodePage::next_leaf_val()` / `set_next_leaf()` — already exists
- Range iterator in `iter.rs` — will be modified
