# Spec: 11.2d — BLOB Reference Tracking

## What to build (not how)

Add reference counting for TOAST/BLOB overflow chains so a chain can be shared by
more than one logical BLOB reference and reclaimed only after the last reference
is released.

This subphase prepares the storage contract needed by the future
content-addressed BLOB store in Phase 14.9. The SQL surface remains unchanged:
applications still insert and read `TEXT`, `JSON`, and `BYTES` values normally.

The current Phase 11.2 TOAST implementation writes large `TEXT` and `BYTES`
values into overflow chains through `clustered_overflow::write_chain()` and
stores an inline u24 sentinel pointer:

- `0xFF_FFFE` — uncompressed TOAST pointer
- `0xFF_FFFD` — LZ4-compressed TOAST pointer

11.2d keeps that inline pointer contract stable and adds a refcounted overflow
chain format for TOAST/BLOB-owned chains.

## Research findings

### PostgreSQL TOAST

PostgreSQL stores out-of-line values in a separate TOAST relation. The inline
`varatt_external` pointer stores the raw size, external size/compression method,
TOAST table OID, and value OID. The TOAST table stores chunk rows keyed by that
value OID, and `toast_delete_datum()` deletes all chunks matching the pointer's
value OID.

During table rewrite, PostgreSQL may preserve a value OID and skip writing the
data again if that value already exists in the new TOAST table. It does not use
a per-page refcount for normal TOAST ownership.

Reference: `research/postgresql/src/backend/access/common/toast_internals.c`.

### SQLite overflow pages

SQLite stores only a linked overflow chain: the B-tree cell points to the first
overflow page, each overflow page stores the next page number, and auto-vacuum
pointer maps help find the next page without always reading the page body. On
delete/update, SQLite frees the overflow chain owned by the cell. It does not
share overflow chains or refcount them.

Reference: `research/sqlite/src/btree.c`.

### InnoDB external BLOBs

InnoDB stores externally stored field references inside the clustered record.
The reference includes space/page/offset/length fields, and the high bits of the
length field encode ownership/inherited flags. Only the owner record is allowed
to free the external field during purge; inherited BLOB references avoid double
free across row versions without needing a mutable page-level refcount.

Reference: `research/mariadb/storage/innobase/include/btr0cur.h` and
`research/mariadb/storage/innobase/btr/btr0cur.cc`.

### Selected design

Use a versioned refcounted TOAST/BLOB chain only for AxiomDB's TOAST/BLOB path.
This combines SQLite/InnoDB's compact linked-page chain with a first-page
refcount required by the future content-addressed BLOB store. It intentionally
does not retrofit clustered-row overflow pages, and it keeps PostgreSQL's lesson
that TOAST ownership must be explicit rather than inferred from arbitrary tuple
copies.

The rejected alternatives are:

- Sidecar catalog refcount: simpler to query but violates the 11.2d requirement
  that the counter lives in the overflow page header.
- Mutating the existing legacy `clustered_overflow` layout for every caller:
  too risky because clustered rows already depend on the legacy chain contract.
- InnoDB-style ownership bit only: robust for MVCC inherited BLOBs, but not
  sufficient for Phase 14.9 content-addressed dedup where N rows may share one
  physical BLOB chain.

## Inputs / Outputs

- Input: a large `TEXT`, `JSON`, or `BYTES` value that is externalized by the
  TOAST write path.
- Output: a TOAST pointer whose first overflow page stores a reference counter
  initialized to `1`.
- Input: `free_blob(first_page_id)` or equivalent release call for a TOAST/BLOB
  pointer.
- Output: the chain reference counter is decremented. If it reaches `0`, every
  page in the chain is freed. If it remains above `0`, the physical pages remain
  allocated.
- Errors:
  - Corrupt chain page type, invalid header, loop, or underflowing refcount
    returns `DbError::BTreeCorrupted` or another existing storage corruption
    error.
  - Releasing an unknown or already-freed page returns the underlying storage
    read/free error.

## Use cases

1. Insert a large `BYTES` value.
   The TOAST path writes a refcounted chain, stores the sentinel pointer inline,
   and a later read reconstructs the original bytes.

2. Delete a row that owns the only reference.
   `free_blob(first_page_id)` decrements `1 → 0` and frees the full chain.

3. Release one reference from a shared chain.
   `free_blob(first_page_id)` decrements `N → N-1` and leaves the chain readable.

4. Prepare Phase 14.9 content-addressed dedup.
   A future duplicate insert can reuse an existing chain by incrementing its
   refcount instead of writing the payload again.

5. Read old non-refcounted chains.
   Existing Phase 11.2 TOAST chains remain readable during the transition. They
   are treated as unshared legacy chains and freed with the old chain-free path.

## Acceptance criteria

- [ ] New TOAST/BLOB overflow chains store a refcount in the first overflow page
      header and initialize it to `1`.
- [ ] `free_blob(first_page_id)` decrements the refcount and frees the chain only
      when it reaches `0`.
- [ ] Refcount underflow is impossible in normal operation and returns a
      corruption error if detected.
- [ ] The read path reconstructs both refcounted and legacy TOAST chains.
- [ ] The delete/update TOAST cleanup path uses the refcount-aware release
      function for TOAST pointers.
- [ ] Existing clustered-row overflow chains keep using their current
      `clustered_overflow::{write_chain, read_chain, free_chain}` contract.
- [ ] Unit tests cover create/read/free, decref-without-free, underflow/corrupt
      header, and legacy-chain compatibility.
- [ ] Integration tests cover large `BYTES` or `TEXT` insert/read/delete through
      the SQL path.

## Out of scope

- Content-addressed dedup by SHA-256. Phase 14.9 will decide lookup/indexing and
  when to increment an existing chain refcount.
- Background BLOB garbage collection. Phase 14.10 owns periodic GC after
  content-store refcounts can reach zero asynchronously.
- SQL-visible BLOB handles or a new SQL type. This subphase is storage plumbing
  behind the existing TOAST pointer contract.
- Retrofitting clustered-row overflow cells to share refcounted chains. Clustered
  row overflow is row-owned and should remain on the existing path unless a
  later subphase explicitly migrates it.

## Dependencies

- Phase 11.2 TOAST sentinels and `encode_toast_pointer()` /
  `decode_toast_pointer()` in `crates/axiomdb-types/src/codec.rs`.
- Existing `clustered_overflow` page-chain primitives in
  `crates/axiomdb-storage/src/clustered_overflow.rs`.
- Existing TOAST write/read/free wiring in `crates/axiomdb-sql/src/table_write.rs`
  and `crates/axiomdb-sql/src/table.rs`.
