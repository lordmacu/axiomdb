# Plan: 39.2 Clustered internal page format

## Files to create/modify

- `crates/axiomdb-storage/src/page.rs` — add `PageType::ClusteredInternal`
- `crates/axiomdb-storage/src/clustered_internal.rs` — implement the clustered internal slotted-page format and page-local operations
- `crates/axiomdb-storage/src/lib.rs` — export the new module

## Algorithm / Data structure

Use a slotted-page layout parallel to `clustered_leaf`, but with internal-node semantics:

```text
Body:
  [ClusteredInternalHeader: 16B]
    is_leaf: u8 = 0
    _pad0: u8
    num_cells: u16
    cell_content_start: u16
    freeblock_offset: u16
    leftmost_child: u64
  [CellPtr array: num_cells × 2B]
  [Free space gap]
  [Cell content area]
    cell:
      [right_child: u64]
      [key_len: u16]
      [key_bytes]
```

Why this shape:

- `leftmost_child` lives in the header
- each separator cell stores the child to its right
- logical child `i` becomes:
  - `i == 0` → `leftmost_child`
  - `i > 0` → `right_child` stored in cell `i - 1`

This preserves the current traversal contract:

```text
find_child_idx(search_key) = first separator strictly greater than search_key
next child pid            = child_at(idx)
```

Pseudocode:

```text
init(page, leftmost_child):
  set page type = ClusteredInternal
  write 16B header
  num_cells = 0
  cell_content_start = BODY_SIZE
  freeblock_offset = 0
  leftmost_child = input

key_at(page, i):
  off = cell_ptr_at(i)
  key_len = read_u16(off + 8)
  return page[off + 10 .. off + 10 + key_len]

child_at(page, i):
  if i == 0: return leftmost_child
  off = cell_ptr_at(i - 1)
  return read_u64(off)

find_child_idx(page, search_key):
  binary search over key_at(mid)
  return first mid where key_at(mid) > search_key

insert_at(page, pos, sep_key, right_child):
  cell_size = 8 + 2 + sep_key.len()
  allocate cell from freeblock chain or contiguous gap
  write cell(right_child, key_len, key_bytes)
  shift pointer array right
  pointer[pos] = new_cell_offset
  num_cells += 1

remove_at(page, key_pos, child_pos):
  materialize logical children[0..=n] and keys[0..n)
  remove keys[key_pos] and children[child_pos]
  rebuild page from scratch into compact form

defragment(page):
  copy live cells into temp buffers
  rewrite compactly from end of body
  rewrite pointer array
  freeblock_offset = 0
```

## Implementation phases

1. Add `PageType::ClusteredInternal` and export the new module from storage.
2. Implement header accessors and pointer-array helpers for clustered internal pages.
3. Implement read APIs: `num_cells`, `key_at`, `child_at`, `set_child_at`, `search` / `find_child_idx`, `free_space`.
4. Implement mutating APIs: `init`, `insert_at`, `remove_at`, `defragment`.
5. Write unit tests that compare child-selection semantics against the current internal-page invariant.

## Tests to write

- unit: initialize empty clustered internal page and validate header fields
- unit: insert separator keys of mixed lengths at front/middle/end
- unit: `child_at` and `set_child_at` preserve `n + 1` logical children
- unit: `find_child_idx` matches current internal-node semantics on exact hits and gaps
- unit: remove key+child and verify the rebuilt page stays sorted and navigable
- unit: fragmented page defragments and recovers contiguous gap space
- unit: page-full insert returns the expected storage error
- integration: none in 39.2; tree-level integration belongs to 39.3+
- bench: none in 39.2; page-local microbench can wait until the clustered tree path exists

## Anti-patterns to avoid

- Do not reuse `InternalNodePage` fixed arrays; that defeats the clustered storage rewrite.
- Do not store child pointers in a way that changes the current `child_at(idx)` traversal contract.
- Do not add a compile-time `MAX_KEY_LEN` cap to clustered internal pages.
- Do not wire `axiomdb-index::BTree` to the new format in this subphase.
- Do not rely on aligned struct casts for variable-size cells; parse from raw page bytes.

## Risks

- Child mapping off-by-one → mitigate by encoding `leftmost_child` explicitly and testing all boundary slots.
- Fragmentation bugs during remove/insert → mitigate by keeping `remove_at` as rebuild-to-compact form first.
- Semantics drift from the current B-tree traversal → mitigate with tests that assert the same binary-search contract.
