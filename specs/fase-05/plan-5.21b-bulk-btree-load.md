# Plan: 5.21b — Bulk B-Tree load for staged INSERT flush

## Files to create/modify

- `crates/axiomdb-index/src/tree.rs` — add `BTree::bulk_load_sorted`
- `crates/axiomdb-sql/src/index_maintenance.rs` — use bulk path in `batch_insert_into_indexes`
- `crates/axiomdb-index/tests/` or inline `#[cfg(test)]` — unit tests for bulk load

## Algorithm

### Phase 1: Fill leaf pages sequentially

```text
bulk_load_sorted(storage, old_root_pid, entries, fillfactor):
  if entries is empty:
    return old_root_pid  // no-op

  threshold = fill_threshold(ORDER_LEAF, fillfactor)
  leaves: Vec<(page_id, separator_key)> = []

  // Build leaves left-to-right
  current_leaf = new LeafNodePage (is_leaf=1, num_keys=0)
  current_pid = alloc_page(Index)

  for (key, rid) in entries:
    if current_leaf.num_keys >= threshold:
      // Finalize current leaf — DON'T write next_leaf yet (set in linking pass)
      leaves.push((current_pid, first_key_of_current_leaf))
      write current_leaf to current_pid

      // Start new leaf
      prev_pid = current_pid
      current_pid = alloc_page(Index)
      current_leaf = new LeafNodePage (is_leaf=1, num_keys=0)

      // Link previous leaf → current
      read prev page, set next_leaf = current_pid, rewrite

    // Append at end — O(1), no binary search, no shift
    pos = current_leaf.num_keys
    current_leaf.key_lens[pos] = key.len()
    current_leaf.keys[pos][..key.len()] = key
    current_leaf.rids[pos] = encode_rid(rid)
    current_leaf.num_keys += 1

  // Write last leaf
  if current_leaf.num_keys > 0:
    leaves.push((current_pid, first_key_of_current_leaf))
    write current_leaf to current_pid
```

### Phase 2: Build internal nodes bottom-up

```text
  if leaves.len() == 1:
    free old_root_pid
    return leaves[0].0  // single leaf is root

  level = leaves  // [(page_id, separator_key)]
  while level.len() > 1:
    next_level = []
    for each group of up to ORDER_INTERNAL entries in level:
      internal_pid = alloc_page(Index)
      internal = new InternalNodePage
      internal.children[0] = group[0].page_id
      for i in 1..group.len():
        internal.keys[i-1] = group[i].separator_key
        internal.children[i] = group[i].page_id
      internal.num_keys = group.len() - 1
      write internal to internal_pid
      next_level.push((internal_pid, group[0].separator_key))
    level = next_level

  free old_root_pid
  return level[0].0  // final root
```

### Phase 3: Wire into batch_insert_into_indexes

In `batch_insert_into_indexes`, for each index where `committed_empty` contains `index_id`:

```text
  // Collect and sort (key, rid) pairs for this index
  let mut pairs: Vec<(Vec<u8>, RecordId)> = vec![]
  for (row, rid) in rows.zip(rids):
    if null/predicate skip: continue
    let key = encode_key(idx, row, rid)  // includes rid suffix for non-unique
    pairs.push((key, rid))
  pairs.sort_by(|a, b| a.0.cmp(&b.0))

  // Bulk load
  let refs: Vec<(&[u8], RecordId)> = pairs.iter().map(|(k,r)| (k.as_slice(), *r)).collect()
  let new_root = BTree::bulk_load_sorted(storage, idx.root_page_id, &refs, idx.fillfactor)?
  idx.root_page_id = new_root
  updated_roots.push((idx.index_id, new_root))
```

## Implementation phases

1. Add `BTree::bulk_load_sorted` with unit tests in `axiomdb-index`
2. Wire into `batch_insert_into_indexes` when `committed_empty` matches
3. Benchmark verification

## Tests to write

- unit:
  - empty entries → returns old root unchanged
  - 1 entry → single leaf with 1 key, lookup finds it
  - ORDER_LEAF entries → single full leaf, no internal nodes
  - ORDER_LEAF + 1 entries → 2 leaves + 1 internal root
  - 50 000 entries → multi-level tree, range scan returns all in order
  - verify next_leaf chain is correct (iterate leaves via next_leaf)
- integration:
  - `BEGIN; INSERT 1000 rows; COMMIT;` — index scan returns all rows in PK order
- bench:
  - `local_bench.py --scenario insert --rows 50000`

## Anti-patterns to avoid

- Do NOT modify `insert_in` or `insert_subtree` — this is a parallel code path
- Do NOT use bulk_load_sorted for non-empty trees — undefined behavior
- Do NOT skip fillfactor enforcement (leaf pages must respect it)
- Do NOT forget to free old_root_pid (page leak)
- Do NOT forget next_leaf linking (breaks range scans and leaf iteration)

## Risks

- Risk: keys not actually sorted → corrupted tree
  Mitigation: debug_assert sorted order; caller sorts before calling

- Risk: leaf linking requires re-reading previous page to set next_leaf
  Mitigation: buffer previous leaf's page_id and write next_leaf before writing current
  (write previous leaf AFTER knowing current's page_id, not before)

- Risk: internal node construction with remainder groups
  Mitigation: careful chunking — last group may have fewer than ORDER_INTERNAL entries
