# Spec: Phase 39.9 secondary indexes with PK bookmarks

## What to build (not how)

Build a clustered-first secondary-index path that stores a primary-key bookmark
inside the physical secondary-index entry instead of depending on a heap
`RecordId`.

The subphase must introduce a dedicated API for clustered secondary indexes that:

- derives the owning table's primary-key bookmark layout from catalog metadata
- encodes a physical secondary-index entry key as:
  - logical secondary key columns
  - plus the primary-key bookmark suffix needed to identify the base clustered row
- decodes scanned secondary-index entries back into:
  - the logical secondary-key values
  - the full primary-key bookmark
- computes prefix range bounds for point/range probes on the logical secondary key
- inserts and deletes bookmark-bearing secondary entries through the existing
  `axiomdb-index::BTree`
- treats row relocation as a no-op for secondary-index maintenance when the
  logical secondary key and primary key stay unchanged

This path is for the clustered rewrite and does not replace the current
heap-backed executor path yet.

## Inputs / Outputs

- Input:
  - `secondary_idx: &IndexDef`
  - `primary_idx: &IndexDef`
  - row values as `&[Value]`
  - encoded logical-key bounds as `&[Value]` or `&[u8]` depending on helper
  - `storage: &dyn StorageEngine` / `&mut dyn StorageEngine`
  - `root_page_id: u64` or `AtomicU64`
- Output:
  - encoded physical secondary key `Vec<u8>`
  - decoded bookmark payload as ordered primary-key `Vec<Value>`
  - scan results carrying bookmark values instead of relying on `RecordId`
  - mutation helpers returning root-page updates when CoW root changes
- Errors:
  - missing or malformed primary-key metadata for the table
  - invalid index definition (primary index passed as secondary, empty PK, etc.)
  - key encoding failures such as `IndexKeyTooLong`
  - B-Tree/storage errors propagated from the underlying engine

## Use cases

1. A non-unique secondary index on `email` stores multiple rows with the same
   logical key and returns distinct primary-key bookmarks in key order.
2. A secondary index whose logical key already contains part of the primary key
   avoids duplicating those PK columns in the bookmark suffix.
3. A clustered-row relocation changes the physical row location but keeps the
   same primary key and secondary-key values, so secondary maintenance becomes
   a no-op.
4. A future clustered executor can scan a secondary index, recover the primary
   key bookmark from the physical entry, and then probe the clustered tree by PK.

## Acceptance criteria

- [ ] A dedicated clustered-secondary module exists, separate from the current
      heap `RecordId` secondary-index maintenance path.
- [ ] The module can derive the PK bookmark suffix layout from a secondary index
      plus the table primary index.
- [ ] The physical secondary entry key encodes the logical secondary key plus
      the missing PK bookmark columns in sortable order.
- [ ] The module can decode a scanned physical secondary key back into the full
      primary-key bookmark.
- [ ] Prefix range helpers can scan all entries for a logical secondary key
      without assuming a fixed 10-byte `RecordId` suffix.
- [ ] Insert/delete/update helpers can maintain bookmark-bearing secondary
      entries through the existing `BTree`.
- [ ] Relocation-only updates are explicitly recognized as secondary no-ops when
      PK and logical secondary membership stay stable.
- [ ] Unit and integration tests cover duplicate logical keys, overlapping PK
      columns, bookmark decode, and relocation-stable maintenance.

## Out of scope

- Replacing the current heap-backed executor/planner/FK path with bookmark-based
  probes.
- Changing the on-page `axiomdb-index` payload from fixed-size `RecordId` bytes
  to a variable-size bookmark payload.
- Hidden-rowid fallback for heap tables without an explicit primary key.
- Clustered MVCC visibility reconstruction, WAL, or crash-recovery semantics.
- SQL-visible clustered table execution (`39.13`–`39.17`).

## Dependencies

- `axiomdb-index::BTree` exists and can store ordered keys.
- `IndexDef.columns` is populated for both secondary and primary indexes.
- `encode_index_key` / `decode_index_key` are available for sortable SQL key bytes.
- Phase `39.2`–`39.8` clustered storage primitives are already in place.

## ⚠️ DEFERRED

- Wiring this bookmark path into the SQL executor and planner for clustered
  tables → pending in `39.13`–`39.17`.
- Replacing fixed `RecordId` payload bytes in the generic B-Tree page format
  with bookmark-native payloads → revisit only if clustered executor still needs it.
- Hidden-key support for tables without an explicit primary key → revisit with
  clustered table DDL semantics in `39.13`.
