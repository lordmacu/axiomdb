# Plan: 39.10 Clustered overflow pages

## Files to create/modify

- `crates/axiomdb-storage/src/clustered_overflow.rs` — new overflow-page helper
  for chained payload allocation, reconstruction, and free
- `crates/axiomdb-storage/src/clustered_leaf.rs` — widen the clustered leaf cell
  format to support inline prefixes plus optional overflow-page references
- `crates/axiomdb-storage/src/clustered_tree.rs` — teach insert/lookup/range/
  update/delete/rebalance paths to work with overflow-backed clustered rows
- `crates/axiomdb-storage/src/lib.rs` — export the new clustered overflow module
- `crates/axiomdb-storage/tests/integration_clustered_tree.rs` — add mixed
  inline/overflow integration coverage across inserts, scans, updates, and
  physical removals

## Algorithm / Data structure

Use a SQLite-style local payload prefix with a separate overflow-page chain,
adapted so that `RowHeader` always stays inline inside the clustered leaf cell.

### 1. Clustered leaf cell format

Change the physical leaf cell from:

```text
[key_len: u16][row_len: u16][RowHeader][key][row]
```

to:

```text
[key_len: u16]
[total_row_len: u32]
[RowHeader: 24B]
[key bytes]
[local_row_prefix bytes]
[overflow_first_page: u64]   // present only when total_row_len > local_prefix_len
```

Derived helpers:

- `max_local_row_bytes(key_len)`:
  compute the local inline budget from a quarter-page target after subtracting
  fixed clustered-leaf overhead for that key
- `local_row_len(key_len, total_row_len)`:
  `min(total_row_len, max_local_row_bytes(key_len))`
- `has_overflow(key_len, total_row_len)`:
  `total_row_len > local_row_len(key_len, total_row_len)`
- `cell_footprint(...)`:
  use physical on-page bytes, not logical row length

Important consequence:

- once a row is overflow-backed for a fixed primary key, its leaf footprint is
  stable because the local prefix length is capped by `max_local_row_bytes(key_len)`

### 2. Overflow page format

Use `PageType::Overflow` pages as a singly linked list:

```text
PageHeader (64B, existing)
Body:
  [next_overflow_page: u64]
  [payload bytes...]
```

Where:

- each non-terminal overflow page is packed with `payload_capacity` bytes
- the last page may be partially used
- the logical row length stored in the leaf cell is the source of truth for how
  many bytes to reconstruct

Public helper shape:

```text
clustered_overflow::write_chain(storage, tail_bytes) -> first_page_id
clustered_overflow::read_chain(storage, first_page_id, expected_len) -> Vec<u8>
clustered_overflow::free_chain(storage, first_page_id) -> ()
```

Validation rules:

- early end of chain before `expected_len` bytes → corruption
- non-overflow page encountered in the chain → corruption
- extra linked pages after `expected_len` bytes were reconstructed → corruption
- allocation/write failure during chain creation → free any newly allocated pages

### 3. Page-local cell ownership model

Do not make structural tree code depend on fully reconstructed logical rows.

Represent owned clustered leaf cells as physical descriptors:

```text
OwnedLeafCell {
  key,
  row_header,
  total_row_len,
  local_row_prefix,
  overflow_first_page,
}
```

This lets `split`, `rebalance`, `merge`, and page rebuild operations move
overflow-backed rows between leaf pages without reallocating or rereading the
overflow chain.

### 4. Insert path

Pseudocode:

```text
insert(storage, root, key, row_header, logical_row):
  descriptor = materialize_leaf_cell(storage, key, row_header, logical_row)
  insert descriptor into clustered leaf
  on split/rebuild, move descriptor physically as-is
```

`materialize_leaf_cell(...)`:

```text
if logical_row.len() <= local_budget(key):
  store row fully inline
else:
  local_prefix = logical_row[..local_budget]
  overflow_tail = logical_row[local_budget..]
  first_page = write_chain(storage, overflow_tail)
  store descriptor with local_prefix + first_page
```

If the leaf insert fails after a new overflow chain was created, free the new
chain before returning the error.

### 5. Lookup and range scan

Leaf decoding becomes two-stage:

```text
page-local decode:
  key
  row_header
  total_row_len
  local_row_prefix
  overflow_first_page?

logical reconstruction:
  row = local_row_prefix
  if overflow_first_page:
    row += read_chain(storage, overflow_first_page, total_row_len - local_prefix.len())
```

`lookup(...)` and `ClusteredRangeIter` continue to expose `ClusteredRow` with
full logical row bytes.

### 6. Update and delete semantics

`update_in_place(...)`:

- decode the old physical descriptor
- materialize the new descriptor for the replacement logical row
- if the new descriptor needs a fresh overflow chain, allocate it first
- attempt same-key rewrite on the owning leaf page
- on success:
  - free the old overflow chain if it is no longer referenced
  - keep the new chain
- on failure:
  - free any newly allocated chain
  - leave the old descriptor untouched

`delete_mark(...)`:

- rewrite only the inline `RowHeader`
- keep `total_row_len`, local prefix, and overflow pointer unchanged

`delete_physical(...)` / relocate-update:

- read the physical descriptor before removing the cell
- after the physical delete succeeds, free the removed row's overflow chain
- relocating an updated row allocates a fresh chain for the new row if needed

### 7. Structural clustered operations

Existing `39.8` logic must switch from logical-row ownership to physical-cell
ownership:

- `collect_leaf_cells()` returns physical `OwnedLeafCell`
- `leaf_footprint()` uses encoded physical bytes, not logical row length
- `rebuild_leaf_page()` writes the exact physical descriptor, preserving the
  overflow pointer
- sibling redistribution/merge never reconstruct or rewrite overflow payload
  unless a user-visible update/delete actually changes the row bytes

## Implementation phases

1. Add `clustered_overflow.rs` with chain write/read/free helpers and unit tests.
2. Widen `clustered_leaf` cell metadata and rewrite decode/encode helpers around
   `total_row_len`, local prefixes, and optional overflow references.
3. Change `clustered_tree` owned-cell/rebuild logic to operate on physical leaf
   descriptors instead of fully inline row bytes.
4. Teach clustered insert/lookup/range to materialize and reconstruct
   overflow-backed rows transparently.
5. Teach `update_in_place`, `update_with_relocation`, and physical delete to
   preserve or free overflow chains at the correct moment.
6. Add integration coverage for mixed inline/overflow trees and overflow-aware
   updates/deletes.

## Tests to write

- unit: overflow chain write/read roundtrip across one page and multiple pages
- unit: freeing an overflow chain releases every page in the chain
- unit: clustered leaf encode/decode roundtrip for inline and overflow-backed
  cells
- unit: insert accepts rows that exceeded the old inline-only limit
- unit: lookup reconstructs overflow-backed row bytes exactly
- unit: range scan returns mixed inline/overflow rows in PK order
- unit: update transitions inline → overflow
- unit: update transitions overflow → overflow
- unit: update transitions overflow → inline and frees the old chain
- unit: delete-mark keeps overflow-backed row bytes reachable for snapshots that
  still see the row
- unit: physical delete frees the removed overflow chain
- integration: clustered split/rebalance/merge preserve overflow-backed rows
  without data loss
- integration: relocate-update on a multi-leaf tree keeps lookups and range
  scans correct for mixed inline/overflow rows
- bench: none in 39.10; clustered write/read benchmarks stay deferred until the
  executor path is clustered-aware

## Anti-patterns to avoid

- Do not build a generic TOAST subsystem here; keep the scope to clustered-row
  overflow support.
- Do not move `RowHeader` off-page; MVCC visibility in Phase 39 still depends on
  inline headers.
- Do not free overflow chains on `delete_mark`.
- Do not make rebalance/merge rebuild logical row bytes when a physical
  descriptor move is sufficient.
- Do not size splits or underflow decisions using logical row length; use the
  actual physical leaf footprint.
- Do not add compression, WAL, or crash-recovery placeholders that suggest
  durability guarantees not yet implemented.

## Risks

- New overflow chain allocated but leaf rewrite/insert fails afterward → mitigate
  with explicit best-effort cleanup of the newly allocated chain before
  returning the error.
- Old overflow chain freed too early during update → mitigate by freeing the old
  chain only after the new leaf descriptor is durably accepted by the in-memory
  page write path.
- Structural code accidentally reconstructs logical rows and churns chains
  during pure page movement → mitigate by changing `OwnedLeafCell` to a physical
  descriptor and keeping page rebuild APIs descriptor-oriented.
- Corrupted overflow chain loops or overruns expected bytes → mitigate by
  bounding traversal by `ceil(expected_len / payload_capacity)` pages plus one
  terminal check.
- Mixed inline/overflow rows skew split heuristics if logical length is used →
  mitigate with physical-footprint helpers and explicit tests around split and
  rebalance boundaries.

## Research citations

- `research/sqlite/src/btreeInt.h` — adapt the local-payload-plus-overflow-chain
  layout, but keep AxiomDB's inline MVCC header.
- `research/mariadb-server/storage/innobase/handler/ha_innodb.cc` — adapt the
  clustered-only external-storage rule for large variable-length payloads.
- `research/mariadb-server/storage/innobase/row/row0ins.cc` and
  `research/mariadb-server/storage/innobase/row/row0upd.cc` — adapt the split
  between record materialization and externally stored payload management.
