# Plan: Phase 39.9 secondary indexes with PK bookmarks

## Files to create/modify

- `crates/axiomdb-sql/src/clustered_secondary.rs`
  - clustered-first helpers for PK bookmark layout, key encode/decode,
    prefix-range bounds, and B-Tree maintenance
- `crates/axiomdb-sql/src/lib.rs`
  - export the new clustered-secondary module
- `crates/axiomdb-sql/tests/`
  - add integration coverage for bookmark-bearing secondary-index scans and
    relocation-stable maintenance

## Algorithm / Data structure

Use the existing `axiomdb-index::BTree` as an ordered key container, but make
the physical secondary key carry the clustered bookmark:

```text
physical_secondary_key =
    encode_index_key(secondary_logical_values ++ missing_primary_key_values)
```

Where:

- `secondary_logical_values` are the values of `secondary_idx.columns`
- `missing_primary_key_values` are the PK columns from `primary_idx.columns`
  that are not already present in `secondary_idx.columns`

This mirrors the relevant storage idea from InnoDB and SQLite `WITHOUT ROWID`:
the secondary record carries enough primary-key information to reach the base row
without a heap `RecordId`.

### Layout derivation

```text
derive_layout(secondary_idx, primary_idx):
  reject if secondary_idx.is_primary
  reject if primary_idx.columns.is_empty()

  secondary_cols = secondary_idx.columns.col_idx
  pk_cols = primary_idx.columns.col_idx

  suffix_cols = []
  for pk_col in pk_cols:
    if pk_col not in secondary_cols:
      suffix_cols.push(pk_col)

  physical_decode_order = secondary_cols ++ suffix_cols
  return layout { secondary_cols, pk_cols, suffix_cols, physical_decode_order }
```

### Encode physical key

```text
encode_entry_key(layout, row):
  logical_vals = row[secondary_cols]
  suffix_vals = row[suffix_cols]
  physical_vals = logical_vals ++ suffix_vals
  return encode_index_key(physical_vals)
```

### Decode bookmark from a scanned key

```text
decode_bookmark(layout, physical_key):
  decoded_vals = decode_index_key(physical_key, layout.physical_decode_order.len())
  map values for secondary_cols from decoded_vals[0..secondary_cols.len()]
  map values for suffix_cols from decoded_vals[secondary_cols.len()..]
  rebuild bookmark in primary_idx column order
  return bookmark_vals
```

### Range bounds

```text
logical_prefix_bounds(logical_vals):
  prefix = encode_index_key(logical_vals)
  lo = prefix
  hi = prefix || [0xFF] * (MAX_INDEX_KEY - prefix.len())
```

This removes the fixed `10-byte RID suffix` assumption from clustered-secondary
lookups.

### Insert / delete / update

```text
insert_secondary(root, row):
  key = encode_entry_key(layout, row)
  BTree::insert_in(storage, root, key, DUMMY_RID, fillfactor)

delete_secondary(root, row):
  key = encode_entry_key(layout, row)
  BTree::delete_in(storage, root, key)

update_secondary(old_row, new_row):
  old_key = encode_entry_key(layout, old_row) if indexed
  new_key = encode_entry_key(layout, new_row) if indexed
  if old_key == new_key:
    return no-op
  else:
    delete old_key if present
    insert new_key if present
```

Use a fixed dummy `RecordId` payload only as a compatibility artifact of the
current B-Tree page format. The clustered-secondary API must never derive row
identity from that payload.

## Implementation phases

1. Add the new `clustered_secondary` module with layout derivation and pure
   encode/decode helpers.
2. Add range-bound helpers and scan helpers that recover PK bookmarks from
   `BTree::range_in(...)` key bytes.
3. Add insert/delete/update maintenance helpers and explicit relocation-no-op
   logic.
4. Add unit tests for layout derivation, overlapping PK columns, bounds, and
   bookmark decode.
5. Add an integration test that builds a bookmark-bearing secondary index in a
   real `BTree`, scans duplicate logical keys, and proves relocation stability.

## Tests to write

- unit:
  - derive suffix columns when secondary already contains part of the PK
  - encode/decode roundtrip for physical secondary keys
  - prefix bounds include all duplicate logical keys
  - relocation-only update returns no-op
- integration:
  - insert multiple rows with the same logical secondary key and recover all PK
    bookmarks from a `BTree` range scan
  - delete one bookmark-bearing entry without affecting siblings with the same
    logical secondary key
  - update a row to a new secondary value and verify old/new prefix probes
- bench:
  - none in `39.9`; real bookmark-vs-heap performance belongs with executor
    integration in `39.13+`

## Anti-patterns to avoid

- Do not rewrite the current heap executor/planner/FK paths in this subphase.
- Do not add catalog format changes just to persist bookmark metadata that can
  be derived from the primary index.
- Do not assume a fixed-width suffix; the whole point is removing the RID-sized
  assumption.
- Do not treat the legacy `RecordId` payload returned by `BTree::range_in` as
  meaningful for clustered-secondary identity.

## Risks

- Prefix upper-bound mistakes could over-scan or miss bookmark-bearing entries
  → mitigate with exact integration tests on duplicate logical keys.
- Layout derivation bugs could rebuild the PK bookmark in the wrong column order
  → mitigate with explicit overlapping-column tests.
- Future executor code might accidentally use the legacy `RecordId` payload
  anyway → mitigate by naming/documenting the API around bookmarks only and
  testing the decoded bookmark path directly.
