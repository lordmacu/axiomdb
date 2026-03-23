# Spec: 3.4 — RowHeader + Heap/Slotted Pages

## What to build (not how)

A slotted data page format that stores rows with MVCC metadata (RowHeader),
and a TransactionSnapshot type for visibility checks.
This is the physical row storage layer that makes `RecordId { page_id, slot_id }` meaningful.

## Inputs / Outputs

### TransactionSnapshot
- Input: `snapshot_id: TxnId`, `current_txn_id: TxnId`
- Pure type, no I/O.

### RowHeader
- Input: raw bytes from a heap page body (zero-copy cast via bytemuck)
- Output: structured access to `txn_id_created`, `txn_id_deleted`, `row_version`

### Heap operations
- `insert_tuple(page: &mut Page, data: &[u8], txn_id: TxnId) -> Result<u16>`
  - Input: a writable heap page, arbitrary row bytes, creating txn_id
  - Output: `slot_id` (u16) assigned to the new tuple
  - Error: `DbError::HeapPageFull` if not enough free space

- `read_tuple(page: &Page, slot_id: u16) -> Result<Option<(&RowHeader, &[u8])>>`
  - Input: page + slot_id
  - Output: `Some((header_ref, data_slice))` if slot live, `None` if dead slot
  - Error: `DbError::InvalidSlot` if slot_id >= num_slots

- `delete_tuple(page: &mut Page, slot_id: u16, txn_id: TxnId) -> Result<()>`
  - Input: page + slot_id + deleting txn_id
  - Output: Ok(()) — marks `txn_id_deleted` in the RowHeader in-place
  - Error: `DbError::InvalidSlot`, `DbError::AlreadyDeleted`

- `update_tuple(page: &mut Page, slot_id: u16, new_data: &[u8], txn_id: TxnId) -> Result<u16>`
  - Input: page + old slot_id + new row bytes + txn_id
  - Output: new `slot_id` of the inserted replacement tuple
  - Semantics: delete_tuple(old) + insert_tuple(new) — MVCC-correct
  - Error: any error from delete or insert

- `scan_visible<'p>(page: &'p Page, snap: &TransactionSnapshot) -> impl Iterator<Item=(u16, &'p [u8])>`
  - Yields `(slot_id, data)` for every live, visible tuple
  - Skips dead slots and tuples not visible to the snapshot

- `free_space(page: &Page) -> usize`
  - Returns bytes available for new tuples (slot entry + data)

## Page layout

Uses the existing `PageHeader` fields — no extra header in the body:
- `page_type` = `PageType::Data`
- `item_count` = number of slots (both live and dead)
- `free_start` = page-absolute offset where slot array ends (grows right)
- `free_end` = page-absolute offset where tuple area starts (grows left)
- `lsn` = LSN of the last write (set by WAL layer, not by heap ops)

Body layout (PAGE_BODY_SIZE = 16 320 bytes):
```
[slot_0: 4B][slot_1: 4B]...[slot_N: 4B]  →free→  ←free←  [tuple_M]...[tuple_1][tuple_0]
 body[0]                   body[free_start - HEADER_SIZE]   body[free_end - HEADER_SIZE]
```

Each SlotEntry (4 bytes):
- `offset: u16` — page-absolute offset to start of tuple (0 = dead slot)
- `length: u16` — total tuple bytes = sizeof(RowHeader) + data.len() (0 = dead slot)

RowHeader (24 bytes, repr(C), bytemuck::Pod):
- `txn_id_created: u64` — txn that inserted this row
- `txn_id_deleted: u64` — txn that deleted this row (0 = live)
- `row_version: u32`    — incremented on UPDATE (optimistic locking, Phase 7)
- `_flags: u32`         — reserved (future: TTL, forwarded pointer, HOT chain)

Max tuples per page (smallest possible row = RowHeader only, 24 bytes):
- Each tuple needs: 4 (slot) + 24 (RowHeader) + 0 (data) = 28 bytes
- 16 320 / 28 ≈ 582 tuples max per page

## Visibility rule

```
A tuple with header H is visible to snapshot S if:
  (H.txn_id_created == S.current_txn_id      -- read your own writes
   OR H.txn_id_created < S.snapshot_id)       -- created before snapshot
  AND
  (H.txn_id_deleted == 0                      -- not deleted
   OR (H.txn_id_deleted >= S.snapshot_id      -- deleted after snapshot
       AND H.txn_id_deleted != S.current_txn_id)) -- not deleted by us
```

`snapshot_id` = max_committed_txn_id + 1 at the moment the snapshot was taken.
`current_txn_id` = 0 means autocommit / read-only (no read-your-own-writes needed).

## Use cases

1. **Autocommit INSERT**: txn_id=1 inserts row, commits. snapshot_id=2. Row visible. ✓
2. **Uncommitted row**: txn_id=5 inserts row (not committed). Reader at snapshot_id=5.
   txn_id_created=5 < 5? No. current_txn_id=0 ≠ 5. Not visible. ✓
3. **Read your own writes**: txn_id=5 inserts row, same txn reads. current_txn_id=5 == 5. Visible. ✓
4. **DELETE visible**: txn_id=3 deletes row (txn_id_deleted=3). Reader at snapshot_id=4.
   txn_id_deleted=3 >= 4? No → row gone. ✓
5. **DELETE not yet committed**: txn_id=3 deletes (not committed). Reader at snapshot_id=3.
   txn_id_deleted=3 >= 3? Yes. txn_id_deleted=3 != current_txn_id=0? Yes → still visible. ✓
6. **UPDATE MVCC**: old slot marked deleted (txn_id_deleted=current), new slot inserted.
   Old invisible to current txn via `txn_id_deleted == current_txn_id` check. ✓
7. **Dead slot**: slot.offset=0, slot.length=0 → skip, never returned by scan_visible. ✓
8. **Full page**: insert_tuple returns DbError::HeapPageFull. Caller allocates new page. ✓
9. **Slot out of range**: read_tuple(slot_id=999) on page with 10 slots → DbError::InvalidSlot. ✓

## Acceptance criteria

- [ ] `RowHeader` is 24 bytes, `repr(C)`, implements `bytemuck::Pod` + `bytemuck::Zeroable`
- [ ] `SlotEntry` is 4 bytes, `repr(C)`, implements `bytemuck::Pod`
- [ ] `TransactionSnapshot` lives in `axiomdb-core`
- [ ] `insert_tuple` + `read_tuple` roundtrip: data in == data out, RowHeader fields correct
- [ ] `delete_tuple` marks `txn_id_deleted` in-place; read_tuple still returns Some (tuple exists)
- [ ] `scan_visible` skips dead slots and non-visible tuples; yields correct (slot_id, data) pairs
- [ ] `update_tuple` = delete old + insert new; old slot is dead; new slot is live
- [ ] `free_space` decreases by `4 + 24 + data.len()` after each insert
- [ ] Insert until page full → HeapPageFull; after full, free_space < 28 bytes
- [ ] `PageHeader.item_count` equals number of slots (live + dead)
- [ ] `PageHeader.free_start` and `free_end` are consistent after every operation
- [ ] `update_checksum()` called after every mutating operation
- [ ] All 9 visibility use cases above verified by unit tests
- [ ] No `unwrap()` in `src/` (only in tests)
- [ ] Integration test: insert 100 rows via heap + lookup all via B+ Tree RecordId → correct

## Out of scope

- TTL / `expires_at` field — deferred
- VACUUM (compaction of dead slots) — deferred to Phase 7
- HOT (Heap-Only Tuple) chain for same-page updates — deferred
- Multi-page tuple overflow — rows must fit in one page (max data = 16 320 - 24 - 4 = 16 292 bytes)
- Transaction status table / clog — deferred to Phase 7
- Page-level locking / latching — deferred to Phase 7

## Dependencies

- `axiomdb-core`: `TxnId`, `RecordId` (already exist)
- `axiomdb-storage`: `Page`, `PageHeader`, `PageType`, `StorageEngine`, `HEADER_SIZE`, `PAGE_SIZE` (already exist)
- `bytemuck`: already a dependency of `axiomdb-storage`
- New `DbError` variants needed: `HeapPageFull`, `InvalidSlot { slot_id: u16 }`, `AlreadyDeleted { slot_id: u16 }`
