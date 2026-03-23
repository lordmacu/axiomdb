# Plan: 3.4 — RowHeader + Heap/Slotted Pages

## Files to create / modify

| File | Action | What |
|---|---|---|
| `crates/nexusdb-core/src/traits.rs` | modify | Add `TransactionSnapshot` |
| `crates/nexusdb-core/src/lib.rs` | modify | Re-export `TransactionSnapshot` |
| `crates/nexusdb-core/src/error.rs` | modify | Add `HeapPageFull`, `InvalidSlot`, `AlreadyDeleted` |
| `crates/nexusdb-storage/src/heap.rs` | create | `RowHeader`, `SlotEntry`, all heap ops |
| `crates/nexusdb-storage/src/lib.rs` | modify | `pub mod heap; pub use heap::...` |

## Step 1 — TransactionSnapshot in nexusdb-core

**File:** `crates/nexusdb-core/src/traits.rs`

Add after the existing type aliases:
```rust
/// Snapshot of the committed transaction state at a point in time.
/// Used by MVCC visibility checks.
///
/// `snapshot_id` = max_committed_txn_id + 1 at the moment this snapshot was taken.
/// A row created by txn C is visible if C < snapshot_id (C was committed before snapshot).
///
/// `current_txn_id` = the txn_id of the active transaction, or 0 in autocommit/read-only mode.
/// Allows a transaction to see its own writes before committing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionSnapshot {
    pub snapshot_id: TxnId,
    pub current_txn_id: TxnId,
}

impl TransactionSnapshot {
    /// Snapshot that sees all committed data and no in-progress transactions.
    /// Used in autocommit read operations.
    pub fn committed(max_committed: TxnId) -> Self {
        Self {
            snapshot_id: max_committed + 1,
            current_txn_id: 0,
        }
    }
}
```

**File:** `crates/nexusdb-core/src/lib.rs`
Add: `pub use traits::TransactionSnapshot;`

## Step 2 — New DbError variants

**File:** `crates/nexusdb-core/src/error.rs`

Add to the `DbError` enum:
```rust
/// Heap page has insufficient free space for the requested tuple.
#[error("heap page {page_id} is full (need {needed} bytes, have {available})")]
HeapPageFull {
    page_id: u64,
    needed: usize,
    available: usize,
},

/// slot_id is out of range for this page.
#[error("invalid slot {slot_id} on page {page_id} (page has {num_slots} slots)")]
InvalidSlot {
    page_id: u64,
    slot_id: u16,
    num_slots: u16,
},

/// Attempted to delete a slot that is already dead.
#[error("slot {slot_id} on page {page_id} is already deleted")]
AlreadyDeleted {
    page_id: u64,
    slot_id: u16,
},
```

## Step 3 — heap.rs: RowHeader + SlotEntry

**File:** `crates/nexusdb-storage/src/heap.rs`

### RowHeader

```rust
/// MVCC metadata prepended to every row stored in a heap page.
///
/// Layout (24 bytes, repr(C), bytemuck::Pod):
/// Offset  Size  Field
///      0     8  txn_id_created — txn that inserted this row
///      8     8  txn_id_deleted — txn that deleted this row (0 = live)
///     16     4  row_version    — incremented on UPDATE (optimistic locking)
///     20     4  _flags         — reserved (future: TTL, HOT chain, forwarded ptr)
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RowHeader {
    pub txn_id_created: u64,
    pub txn_id_deleted: u64,
    pub row_version:    u32,
    pub _flags:         u32,
}
// compile-time size check
const _: () = assert!(std::mem::size_of::<RowHeader>() == 24);
unsafe impl bytemuck::Zeroable for RowHeader {}
unsafe impl bytemuck::Pod for RowHeader {}
```

### SlotEntry

```rust
/// 4-byte slot directory entry.
/// offset=0, length=0 means dead slot (can be reused by VACUUM).
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SlotEntry {
    /// Page-absolute offset to start of tuple (= HEADER_SIZE + body_offset).
    pub offset: u16,
    /// Total tuple bytes: size_of::<RowHeader>() + data.len().
    pub length: u16,
}
const _: () = assert!(std::mem::size_of::<SlotEntry>() == 4);
unsafe impl bytemuck::Zeroable for SlotEntry {}
unsafe impl bytemuck::Pod for SlotEntry {}

impl SlotEntry {
    pub fn is_dead(self) -> bool { self.offset == 0 && self.length == 0 }
}
```

### RowHeader::is_visible

```rust
impl RowHeader {
    /// Returns true if this row is visible to the given transaction snapshot.
    ///
    /// Visibility rule (MVCC):
    ///   created_visible: txn_id_created == snap.current_txn_id   (read own writes)
    ///                    OR txn_id_created < snap.snapshot_id    (committed before snap)
    ///   not_deleted: txn_id_deleted == 0                          (live)
    ///                OR txn_id_deleted >= snap.snapshot_id        (deleted after snap)
    ///                   AND txn_id_deleted != snap.current_txn_id (not deleted by us)
    pub fn is_visible(&self, snap: &TransactionSnapshot) -> bool {
        let created_visible = self.txn_id_created == snap.current_txn_id
            || self.txn_id_created < snap.snapshot_id;
        let not_deleted = self.txn_id_deleted == 0
            || (self.txn_id_deleted >= snap.snapshot_id
                && self.txn_id_deleted != snap.current_txn_id);
        created_visible && not_deleted
    }
}
```

## Step 4 — heap.rs: internal helpers

Internal helpers (private, not exported):

```rust
/// Number of slots on this page.
fn num_slots(page: &Page) -> u16 {
    page.header().item_count
}

/// Byte offset from page start to slot i.
fn slot_offset(i: u16) -> usize {
    HEADER_SIZE + (i as usize) * size_of::<SlotEntry>()
}

/// Read SlotEntry i (zero-copy cast).
fn read_slot(page: &Page, i: u16) -> SlotEntry {
    let off = slot_offset(i);
    *bytemuck::from_bytes(&page.as_bytes()[off..off + 4])
}

/// Write SlotEntry i in-place.
fn write_slot(page: &mut Page, i: u16, entry: SlotEntry) {
    let off = slot_offset(i);
    page.as_bytes_mut()[off..off + 4].copy_from_slice(bytemuck::bytes_of(&entry));
}

/// Free bytes available for a new (slot + tuple).
pub fn free_space(page: &Page) -> usize {
    let h = page.header();
    h.free_end as usize - h.free_start as usize
}

/// Minimum bytes needed to insert a tuple with `data_len` bytes.
fn needed(data_len: usize) -> usize {
    size_of::<SlotEntry>() + size_of::<RowHeader>() + data_len
}
```

Note: `Page` needs a `pub fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE]` method.
Check if it exists — if not, add it to `page.rs` (currently only `as_bytes` is public).

## Step 5 — heap.rs: insert_tuple

```rust
pub fn insert_tuple(page: &mut Page, data: &[u8], txn_id: TxnId) -> Result<u16, DbError> {
    let avail = free_space(page);
    let need  = needed(data.len());
    if avail < need {
        return Err(DbError::HeapPageFull {
            page_id:   page.header().page_id,
            needed:    need,
            available: avail,
        });
    }

    // Allocate tuple space at the end of the tuple area (grows left).
    let tuple_len = (size_of::<RowHeader>() + data.len()) as u16;
    let new_free_end = page.header().free_end - tuple_len;
    let tuple_abs_off = new_free_end as usize; // page-absolute offset

    // Write RowHeader at the start of the tuple.
    let header = RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version:    0,
        _flags:         0,
    };
    let raw = page.as_bytes_mut();
    raw[tuple_abs_off..tuple_abs_off + size_of::<RowHeader>()]
        .copy_from_slice(bytemuck::bytes_of(&header));
    // Write data right after the RowHeader.
    raw[tuple_abs_off + size_of::<RowHeader>()..tuple_abs_off + tuple_len as usize]
        .copy_from_slice(data);

    // Append a new slot entry.
    let slot_id = page.header().item_count;
    let entry = SlotEntry { offset: new_free_end, length: tuple_len };
    write_slot(page, slot_id, entry);

    // Update PageHeader.
    {
        let hdr = page.header_mut();
        hdr.item_count  += 1;
        hdr.free_start  += size_of::<SlotEntry>() as u16;
        hdr.free_end     = new_free_end;
    }

    page.update_checksum();
    Ok(slot_id)
}
```

## Step 6 — heap.rs: read_tuple

```rust
pub fn read_tuple(page: &Page, slot_id: u16) -> Result<Option<(&RowHeader, &[u8])>, DbError> {
    let n = num_slots(page);
    if slot_id >= n {
        return Err(DbError::InvalidSlot {
            page_id:   page.header().page_id,
            slot_id,
            num_slots: n,
        });
    }
    let entry = read_slot(page, slot_id);
    if entry.is_dead() {
        return Ok(None);
    }
    let off = entry.offset as usize;
    let len = entry.length as usize;
    let bytes = &page.as_bytes()[off..off + len];
    let header: &RowHeader = bytemuck::from_bytes(&bytes[..size_of::<RowHeader>()]);
    let data = &bytes[size_of::<RowHeader>()..];
    Ok(Some((header, data)))
}
```

## Step 7 — heap.rs: delete_tuple

```rust
pub fn delete_tuple(page: &mut Page, slot_id: u16, txn_id: TxnId) -> Result<(), DbError> {
    let n = num_slots(page);
    if slot_id >= n {
        return Err(DbError::InvalidSlot {
            page_id:   page.header().page_id,
            slot_id,
            num_slots: n,
        });
    }
    let entry = read_slot(page, slot_id);
    if entry.is_dead() {
        return Err(DbError::AlreadyDeleted {
            page_id: page.header().page_id,
            slot_id,
        });
    }

    // Write txn_id_deleted in-place into the RowHeader stored in the page.
    let off = entry.offset as usize + std::mem::offset_of!(RowHeader, txn_id_deleted);
    page.as_bytes_mut()[off..off + 8].copy_from_slice(&txn_id.to_le_bytes());

    page.update_checksum();
    Ok(())
}
```

Note: `std::mem::offset_of!` is stable since Rust 1.77. If unavailable, use
`offset_of!(RowHeader, txn_id_deleted)` from the `memoffset` crate, or compute it
as `size_of::<u64>()` (since `txn_id_deleted` is the second u64 = offset 8).

## Step 8 — heap.rs: update_tuple

```rust
pub fn update_tuple(
    page: &mut Page,
    slot_id: u16,
    new_data: &[u8],
    txn_id: TxnId,
) -> Result<u16, DbError> {
    delete_tuple(page, slot_id, txn_id)?;
    insert_tuple(page, new_data, txn_id)
}
```

Note: `delete_tuple` calls `update_checksum()` and `insert_tuple` calls it again.
For a tiny optimization we could skip the first checksum update, but correctness
over micro-optimization at this layer.

## Step 9 — heap.rs: scan_visible

```rust
pub fn scan_visible<'p>(
    page: &'p Page,
    snap: &TransactionSnapshot,
) -> impl Iterator<Item = (u16, &'p [u8])> + 'p {
    let n = num_slots(page);
    let snap = *snap; // copy (it's 2×u64, cheap)
    (0..n).filter_map(move |slot_id| {
        let entry = read_slot(page, slot_id);
        if entry.is_dead() {
            return None;
        }
        let off = entry.offset as usize;
        let len = entry.length as usize;
        let bytes = &page.as_bytes()[off..off + len];
        let header: &RowHeader = bytemuck::from_bytes(&bytes[..size_of::<RowHeader>()]);
        if !header.is_visible(&snap) {
            return None;
        }
        let data = &bytes[size_of::<RowHeader>()..];
        Some((slot_id, data))
    })
}
```

## Step 10 — Tests

**Unit tests in heap.rs** (`#[cfg(test)]`):

```rust
// Helpers
fn fresh_heap_page() -> Page { /* PageType::Data, id=42 */ }

// Test: insert + read roundtrip
fn test_insert_read_roundtrip()
// Test: delete marks txn_id_deleted, read still returns Some
fn test_delete_marks_txn_id_deleted()
// Test: dead slot returns None from read_tuple
fn test_dead_slot_returns_none()  // force dead via SlotEntry direct write in test
// Test: update = delete + insert
fn test_update_returns_new_slot()
// Test: free_space decreases correctly
fn test_free_space_accounting()
// Test: page full returns HeapPageFull
fn test_page_full_error()
// Test: slot out of range returns InvalidSlot
fn test_invalid_slot_error()
// Test: double delete returns AlreadyDeleted
fn test_double_delete_error()
// Test: PageHeader fields (item_count, free_start, free_end) consistent after ops
fn test_page_header_fields_consistent()
// Test: checksum valid after every operation
fn test_checksum_valid_after_ops()

// Visibility tests (9 cases from spec)
fn test_visibility_autocommit_insert()        // use case 1
fn test_visibility_uncommitted_row()          // use case 2
fn test_visibility_read_own_writes()          // use case 3
fn test_visibility_delete_visible()           // use case 4
fn test_visibility_delete_uncommitted()       // use case 5
fn test_visibility_update_own_delete()        // use case 6
fn test_visibility_dead_slot_skipped()        // use case 7
fn test_scan_visible_filters_correctly()      // covers uses 1-7 in one scan
```

**Integration test** in `crates/nexusdb-storage/tests/heap_btree.rs`:
```rust
// Insert 100 rows via heap page + record RecordIds in B+ Tree
// Read all via B+ Tree → heap roundtrip
// Delete 50 via heap + verify scan_visible returns 50
fn test_heap_btree_roundtrip_100_rows()
```

**Benchmark** in `crates/nexusdb-storage/benches/storage.rs` (extend existing):
```rust
// Insert throughput on a single page
fn bench_heap_insert_sequential()  // target: > 1M inserts/s on MemoryStorage
// scan_visible throughput (100% visible, full page)
fn bench_heap_scan_full_page()
```

## Anti-patterns to avoid

- **NO** extra byte in the page body for a "heap header" — use existing PageHeader fields
- **NO** `unwrap()` in src/ — use `?` with typed errors
- **NO** `offset_of!` from memoffset crate — use `std::mem::offset_of!` (stable since 1.77) or hardcode with a compile-time assert
- **NO** copying tuple data to verify reads — return `&[u8]` slices into the page (zero-copy)
- **NO** scanning all slots in delete — direct slot access via `read_slot(page, slot_id)` is O(1)
- **NO** removing slots on delete — slots are permanent (VACUUM compacts later, Phase 7)
- **NO** updating checksum twice in `update_tuple` — acceptable: correctness > micro-opt here

## Risks

| Risk | Mitigation |
|---|---|
| `free_end` underflows if tuple too large | Checked in `insert_tuple` before write: `avail < need` |
| `offset_of!` not stable on Rust < 1.77 | Check `rustup show`; alternatively hardcode offset = 8 + compile-time assert |
| Slot array and tuple area overlap | `free_start <= free_end` invariant maintained; verified in tests |
| `as_bytes_mut()` doesn't exist on `Page` | Add it; symmetric to `as_bytes()` which already exists |
| `bytemuck::from_bytes` panics on unaligned slice | Page is align(64); body offsets are correct; verify in tests |

## Implementation order

```
1. nexusdb-core: TransactionSnapshot + re-export
2. nexusdb-core: DbError variants (HeapPageFull, InvalidSlot, AlreadyDeleted)
3. nexusdb-storage/page.rs: add as_bytes_mut() if missing
4. nexusdb-storage/heap.rs: RowHeader + SlotEntry + free_space + helpers
5. nexusdb-storage/heap.rs: insert_tuple + read_tuple
6. cargo test -p nexusdb-storage — compile check
7. nexusdb-storage/heap.rs: delete_tuple + update_tuple + scan_visible
8. Unit tests for all heap ops + all 9 visibility cases
9. Integration test heap_btree_roundtrip
10. Benchmark heap_insert_sequential
11. cargo test --workspace + clippy + fmt
12. Closing protocol
```
