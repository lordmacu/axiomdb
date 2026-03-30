# Plan: 7.9 — Resolve next_leaf CoW Gap

## Files to modify

| File | Change |
|------|--------|
| `crates/axiomdb-index/src/tree.rs` | Update predecessor after split/merge |
| `crates/axiomdb-index/src/iter.rs` | Use next_leaf instead of tree traversal |
| `docs-site/src/internals/btree.md` | Update leaf linking section |
| `docs/progreso.md` | Mark 7.9 |

## Algorithm

### Find predecessor leaf

Given a parent internal node and the child slot index of the node that was
split, find the rightmost leaf of the left sibling subtree:

```rust
fn find_predecessor_leaf(
    storage: &dyn StorageEngine,
    parent_page: &Page,
    child_slot_idx: usize,
) -> Option<u64> {
    if child_slot_idx == 0 {
        return None;  // leftmost child, no predecessor in this parent
    }
    // Get the left sibling's page_id from parent's children array
    let left_child_pid = parent.child(child_slot_idx - 1);
    // Descend to rightmost leaf of left subtree
    descend_rightmost_leaf(storage, left_child_pid)
}
```

### Update predecessor on split

In `insert_leaf` after splitting and writing new pages:
```rust
// Find predecessor via parent
if let Some(pred_pid) = find_predecessor_leaf(storage, &parent, child_idx) {
    let mut pred_page = storage.read_page(pred_pid)?.into_page();
    let pred_leaf = LeafNodePage::from_body_mut(pred_page.body_mut());
    pred_leaf.set_next_leaf(new_left_pid);
    pred_page.update_checksum();
    storage.write_page(pred_pid, &pred_page)?;
}
```

### Update predecessor on merge

Same pattern after merge_children.

### Simplify range iterator

Replace `find_next_leaf` (tree descent) with direct `next_leaf` follow:
```rust
fn advance_to_next_leaf(&mut self, storage: &dyn StorageEngine) -> Option<u64> {
    let page = storage.read_page(self.current_leaf_pid)?;
    let leaf = LeafNodePage::from_body(&page.body());
    let next = leaf.next_leaf_val();
    if next == 0 { None } else { Some(next) }
}
```

## Implementation Phases

### Phase 1: Predecessor update on split
1. Add `find_predecessor_leaf()` helper
2. In `insert_leaf`: after split, find and update predecessor
3. Test: insert sequence that causes split → verify next_leaf chain

### Phase 2: Predecessor update on merge
4. In `merge_children`: find and update predecessor after merge
5. Test: delete sequence that causes merge → verify next_leaf chain

### Phase 3: Optimize range iterator
6. Modify `iter.rs` to use next_leaf directly
7. Verify all range scan tests still pass
8. Benchmark: compare O(1) vs O(log n) leaf traversal

### Phase 4: Documentation + close
9. Update docs + progreso

## Risks

| Risk | Mitigation |
|------|------------|
| Predecessor not found (leftmost leaf of tree) | Return None, no update needed |
| Split of root leaf (no parent) | Root split creates new parent; predecessor is None |
| Multiple cascading splits | Each split updates its own predecessor independently |
