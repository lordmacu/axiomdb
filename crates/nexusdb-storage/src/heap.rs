//! Heap (slotted) page — physical row storage with MVCC metadata.
//!
//! Every data page (`PageType::Data`) is a slotted page:
//!
//! ```text
//! ┌──────────────────────────────────────────────────────────────────────────┐
//! │ PageHeader (64 B)  item_count=slots  free_start→  ←free_end  lsn       │
//! ├──────────────────────────────────────────────────────────────────────────┤
//! │ [SlotEntry 4B][SlotEntry 4B]...  →  free space  ←  ...tuple_1 tuple_0  │
//! └──────────────────────────────────────────────────────────────────────────┘
//! ```
//!
//! - **Slot array** starts at `body[0]` and grows toward higher addresses.
//! - **Tuples** are appended from `body[end]` and grow toward lower addresses.
//! - A dead slot has `offset == 0 && length == 0` — it is skipped by all scans.
//! - `PageHeader.item_count` = total slots (live + dead).
//! - `PageHeader.free_start` = page-absolute offset to end of slot array.
//! - `PageHeader.free_end`   = page-absolute offset to start of tuple area.
//!
//! Each tuple layout:
//! ```text
//! [RowHeader 24B][row data bytes...]
//! ```
//!
//! ## MVCC visibility
//!
//! Every tuple starts with a [`RowHeader`] that records which transaction
//! created and (optionally) deleted it. [`RowHeader::is_visible`] implements
//! the MVCC snapshot rule used by [`scan_visible`].

use std::mem::size_of;

use nexusdb_core::{DbError, TransactionSnapshot, TxnId};

use crate::page::{Page, HEADER_SIZE, PAGE_SIZE};

// ── RowHeader ─────────────────────────────────────────────────────────────────

/// MVCC metadata prepended to every row stored in a heap page.
///
/// Layout (24 bytes, `repr(C)`, `bytemuck::Pod`):
///
/// ```text
/// Offset  Size  Field
///      0     8  txn_id_created — transaction that inserted this row
///      8     8  txn_id_deleted — transaction that deleted this row (0 = live)
///     16     4  row_version    — incremented on UPDATE (optimistic locking, Phase 7)
///     20     4  _flags         — reserved (future: TTL flag, HOT chain, forwarded ptr)
/// Total: 24 bytes
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RowHeader {
    pub txn_id_created: u64,
    pub txn_id_deleted: u64,
    pub row_version: u32,
    pub _flags: u32,
}

const _: () = assert!(size_of::<RowHeader>() == 24, "RowHeader must be 24 bytes");

// SAFETY: RowHeader is repr(C) with only integer fields — no padding, any bit
// pattern is valid, size equals sum of fields (verified by the assert above).
unsafe impl bytemuck::Zeroable for RowHeader {}
unsafe impl bytemuck::Pod for RowHeader {}

impl RowHeader {
    /// Returns `true` if this row is visible to the given transaction snapshot.
    ///
    /// ## Visibility rule (MVCC)
    ///
    /// A tuple is visible to snapshot `S` when:
    /// - **Created before snapshot or by us:**
    ///   `txn_id_created == S.current_txn_id`  (read your own writes)
    ///   OR `txn_id_created < S.snapshot_id`   (committed before snapshot)
    /// - **Not deleted, or deleted after snapshot and not by us:**
    ///   `txn_id_deleted == 0`                  (live)
    ///   OR `(txn_id_deleted >= S.snapshot_id` — not yet committed when snap taken,
    ///   `AND txn_id_deleted != S.current_txn_id)` — not deleted by us
    ///
    /// `snapshot_id = max_committed_txn_id + 1` at the time the snapshot was taken.
    pub fn is_visible(&self, snap: &TransactionSnapshot) -> bool {
        let created_visible =
            self.txn_id_created == snap.current_txn_id || self.txn_id_created < snap.snapshot_id;
        let not_deleted = self.txn_id_deleted == 0
            || (self.txn_id_deleted >= snap.snapshot_id
                && self.txn_id_deleted != snap.current_txn_id);
        created_visible && not_deleted
    }
}

// ── SlotEntry ─────────────────────────────────────────────────────────────────

/// 4-byte slot directory entry in the body of a heap page.
///
/// - `offset` = page-absolute byte offset to the start of the tuple.
/// - `length` = total tuple bytes: `size_of::<RowHeader>() + data.len()`.
/// - Dead slot: `offset == 0 && length == 0`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SlotEntry {
    pub offset: u16,
    pub length: u16,
}

const _: () = assert!(size_of::<SlotEntry>() == 4, "SlotEntry must be 4 bytes");

// SAFETY: SlotEntry is repr(C) with two u16 fields — no padding, any bit pattern valid.
unsafe impl bytemuck::Zeroable for SlotEntry {}
unsafe impl bytemuck::Pod for SlotEntry {}

impl SlotEntry {
    #[inline]
    pub fn is_dead(self) -> bool {
        self.offset == 0 && self.length == 0
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Total number of slot entries on this page.
#[inline]
fn num_slots(page: &Page) -> u16 {
    page.header().item_count
}

/// Page-absolute byte offset of slot entry `i`.
#[inline]
fn slot_abs_offset(i: u16) -> usize {
    HEADER_SIZE + i as usize * size_of::<SlotEntry>()
}

/// Reads slot entry `i` from the page (zero-copy cast).
#[inline]
fn read_slot(page: &Page, i: u16) -> SlotEntry {
    let off = slot_abs_offset(i);
    *bytemuck::from_bytes(&page.as_bytes()[off..off + size_of::<SlotEntry>()])
}

/// Writes slot entry `i` in-place into the page.
#[inline]
fn write_slot(page: &mut Page, i: u16, entry: SlotEntry) {
    let off = slot_abs_offset(i);
    page.as_bytes_mut()[off..off + size_of::<SlotEntry>()]
        .copy_from_slice(bytemuck::bytes_of(&entry));
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Rounds `n` up to the nearest multiple of 8 (satisfies `RowHeader` align(8)).
#[inline]
fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// Bytes available for a new slot entry + tuple on this page.
///
/// A tuple with `data_len` bytes requires `4 + align8(24 + data_len)` bytes.
/// Because `RowHeader` has alignment 8, tuples are always padded to 8-byte boundaries.
pub fn free_space(page: &Page) -> usize {
    let h = page.header();
    // free_start and free_end are page-absolute (include HEADER_SIZE).
    h.free_end as usize - h.free_start as usize
}

/// Inserts a new tuple with `data` bytes into `page`, tagging it with `txn_id`.
///
/// Returns the `slot_id` assigned to the new tuple.
///
/// Tuples are placed at 8-byte-aligned offsets so that `bytemuck` zero-copy
/// casts of [`RowHeader`] are always valid. The [`SlotEntry`] stores the
/// *actual* (unpadded) tuple length, not the allocated (padded) size.
///
/// # Errors
/// - [`DbError::HeapPageFull`] if the page has insufficient free space.
pub fn insert_tuple(page: &mut Page, data: &[u8], txn_id: TxnId) -> Result<u16, DbError> {
    // Actual byte count for this tuple (RowHeader + data, no padding).
    let tuple_len_actual = size_of::<RowHeader>() + data.len();
    // Allocated byte count on the page (padded to 8-byte alignment so that
    // the tuple's start offset — which equals free_end after subtraction —
    // is always a multiple of 8, satisfying RowHeader's alignment requirement).
    let tuple_len_alloc = align8(tuple_len_actual);
    let need = size_of::<SlotEntry>() + tuple_len_alloc;
    let avail = free_space(page);
    if avail < need {
        return Err(DbError::HeapPageFull {
            page_id: page.header().page_id,
            needed: need,
            available: avail,
        });
    }

    // Grow the tuple area downward (tuples are packed from the end of the page).
    // new_free_end is a multiple of 8 because free_end starts at PAGE_SIZE (multiple of 8)
    // and we subtract multiples of 8 each time.
    let new_free_end = page.header().free_end - tuple_len_alloc as u16;
    let tuple_abs = new_free_end as usize;

    // Write RowHeader + data into the tuple area.
    let header = RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: 0,
        _flags: 0,
    };
    {
        let raw = page.as_bytes_mut();
        raw[tuple_abs..tuple_abs + size_of::<RowHeader>()]
            .copy_from_slice(bytemuck::bytes_of(&header));
        raw[tuple_abs + size_of::<RowHeader>()..tuple_abs + tuple_len_actual].copy_from_slice(data);
        // Padding bytes (tuple_len_alloc - tuple_len_actual) are already zero
        // because Page::new zero-initialises the body, and we never reuse space
        // in this implementation (VACUUM handles compaction in Phase 7).
    }

    // Append slot entry: stores the ACTUAL (unpadded) length so readers get
    // the exact data slice without trailing padding bytes.
    let slot_id = page.header().item_count;
    let entry = SlotEntry {
        offset: new_free_end,
        length: tuple_len_actual as u16,
    };
    write_slot(page, slot_id, entry);

    // Update PageHeader bookkeeping.
    {
        let hdr = page.header_mut();
        hdr.item_count += 1;
        hdr.free_start += size_of::<SlotEntry>() as u16;
        hdr.free_end = new_free_end;
    }

    page.update_checksum();
    Ok(slot_id)
}

/// Returns `(row_header_ref, data_slice)` for `slot_id`, or `None` if the slot is dead.
///
/// The returned references point directly into the page buffer — zero copy.
///
/// # Errors
/// - [`DbError::InvalidSlot`] if `slot_id >= num_slots`.
pub fn read_tuple(page: &Page, slot_id: u16) -> Result<Option<(&RowHeader, &[u8])>, DbError> {
    let n = num_slots(page);
    if slot_id >= n {
        return Err(DbError::InvalidSlot {
            page_id: page.header().page_id,
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

/// Marks `slot_id` as deleted by `txn_id` by writing `txn_id_deleted` in-place.
///
/// The slot remains physically present (MVCC: old versions must be readable
/// by concurrent snapshots). VACUUM will reclaim the space later (Phase 7).
///
/// # Errors
/// - [`DbError::InvalidSlot`] if `slot_id >= num_slots`.
/// - [`DbError::AlreadyDeleted`] if the slot is already dead.
pub fn delete_tuple(page: &mut Page, slot_id: u16, txn_id: TxnId) -> Result<(), DbError> {
    let n = num_slots(page);
    if slot_id >= n {
        return Err(DbError::InvalidSlot {
            page_id: page.header().page_id,
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

    // Write txn_id_deleted directly into the RowHeader stored in the page body.
    // txn_id_deleted is at byte offset 8 inside RowHeader (after txn_id_created: u64).
    const TXN_DELETED_OFFSET_IN_HEADER: usize = 8;
    let field_abs = entry.offset as usize + TXN_DELETED_OFFSET_IN_HEADER;
    page.as_bytes_mut()[field_abs..field_abs + size_of::<u64>()]
        .copy_from_slice(&txn_id.to_le_bytes());

    page.update_checksum();
    Ok(())
}

/// Replaces the row at `slot_id` with `new_data` under transaction `txn_id`.
///
/// Implemented as MVCC delete + insert:
/// - The old slot is marked deleted (`txn_id_deleted = txn_id`).
/// - A new slot is inserted with `txn_id_created = txn_id`.
///
/// Returns the new `slot_id` of the replacement tuple.
///
/// # Errors
/// Any error from [`delete_tuple`] or [`insert_tuple`].
pub fn update_tuple(
    page: &mut Page,
    slot_id: u16,
    new_data: &[u8],
    txn_id: TxnId,
) -> Result<u16, DbError> {
    delete_tuple(page, slot_id, txn_id)?;
    insert_tuple(page, new_data, txn_id)
}

/// Returns an iterator over `(slot_id, data)` for all tuples visible to `snap`.
///
/// Dead slots and tuples whose [`RowHeader`] is not visible to `snap` are skipped.
/// The returned `data` slices point directly into the page buffer — zero copy.
pub fn scan_visible<'p>(
    page: &'p Page,
    snap: &TransactionSnapshot,
) -> impl Iterator<Item = (u16, &'p [u8])> + 'p {
    let n = num_slots(page);
    let snap = *snap;
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

// ── Compile-time invariant checks ─────────────────────────────────────────────

/// Minimum bytes consumed by a zero-length-data tuple:
/// slot entry (4) + 8-aligned allocation of RowHeader (24, already 8-aligned) = 28.
pub const MIN_TUPLE_OVERHEAD: usize = size_of::<SlotEntry>() + size_of::<RowHeader>();

/// Maximum data bytes that fit in a single heap page (conservative: ignores alignment padding).
pub const MAX_TUPLE_DATA: usize = PAGE_SIZE - HEADER_SIZE - MIN_TUPLE_OVERHEAD;

const _: () = assert!(
    MIN_TUPLE_OVERHEAD == 28,
    "SlotEntry(4) + RowHeader(24, 8-aligned) must equal 28 bytes"
);

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::PageType;

    fn fresh_page() -> Page {
        let mut p = Page::new(PageType::Data, 42);
        // Data pages: free_start and free_end initialized by Page::new
        // free_start = HEADER_SIZE (64), free_end = PAGE_SIZE (16384)
        p
    }

    // ── insert + read ──────────────────────────────────────────────────────

    #[test]
    fn test_insert_read_roundtrip() {
        let mut page = fresh_page();
        let data = b"hello world";
        let slot = insert_tuple(&mut page, data, 1).unwrap();
        assert_eq!(slot, 0);

        let (hdr, got) = read_tuple(&page, slot).unwrap().unwrap();
        assert_eq!(got, data);
        assert_eq!(hdr.txn_id_created, 1);
        assert_eq!(hdr.txn_id_deleted, 0);
        assert_eq!(hdr.row_version, 0);
    }

    #[test]
    fn test_insert_multiple_slots() {
        let mut page = fresh_page();
        let slot0 = insert_tuple(&mut page, b"row0", 1).unwrap();
        let slot1 = insert_tuple(&mut page, b"row1", 1).unwrap();
        let slot2 = insert_tuple(&mut page, b"row2", 2).unwrap();
        assert_eq!((slot0, slot1, slot2), (0, 1, 2));

        assert_eq!(read_tuple(&page, 0).unwrap().unwrap().1, b"row0");
        assert_eq!(read_tuple(&page, 1).unwrap().unwrap().1, b"row1");
        assert_eq!(read_tuple(&page, 2).unwrap().unwrap().1, b"row2");
        assert_eq!(read_tuple(&page, 2).unwrap().unwrap().0.txn_id_created, 2);
    }

    #[test]
    fn test_page_header_fields_consistent() {
        let mut page = fresh_page();
        let initial_free = free_space(&page);

        insert_tuple(&mut page, b"abc", 1).unwrap(); // 3 bytes data
        let after_one = free_space(&page);
        // Consumed = slot(4) + align8(RowHeader(24) + 3) = 4 + align8(27) = 4 + 32 = 36
        let expected_consumed = size_of::<SlotEntry>() + align8(size_of::<RowHeader>() + 3);
        assert_eq!(initial_free - after_one, expected_consumed);
        assert_eq!(page.header().item_count, 1);

        // free_start and free_end must be consistent
        let h = page.header();
        assert!(h.free_start < h.free_end);
        assert_eq!(h.free_start as usize - HEADER_SIZE, size_of::<SlotEntry>());
    }

    #[test]
    fn test_checksum_valid_after_insert() {
        let mut page = fresh_page();
        insert_tuple(&mut page, b"data", 1).unwrap();
        assert!(page.verify_checksum().is_ok());
    }

    // ── delete ─────────────────────────────────────────────────────────────

    #[test]
    fn test_delete_marks_txn_id_deleted() {
        let mut page = fresh_page();
        let slot = insert_tuple(&mut page, b"to delete", 1).unwrap();

        delete_tuple(&mut page, slot, 2).unwrap();

        // Slot still physically exists (read_tuple returns Some).
        let (hdr, data) = read_tuple(&page, slot).unwrap().unwrap();
        assert_eq!(data, b"to delete");
        assert_eq!(hdr.txn_id_deleted, 2);
        assert!(page.verify_checksum().is_ok());
    }

    #[test]
    fn test_delete_nonexistent_slot_error() {
        let mut page = fresh_page();
        let err = delete_tuple(&mut page, 99, 1).unwrap_err();
        assert!(matches!(err, DbError::InvalidSlot { slot_id: 99, .. }));
    }

    #[test]
    fn test_double_delete_error() {
        let mut page = fresh_page();
        // Force a dead slot by inserting then hard-zeroing the slot entry.
        insert_tuple(&mut page, b"x", 1).unwrap();
        // Mark it dead manually for the test.
        write_slot(
            &mut page,
            0,
            SlotEntry {
                offset: 0,
                length: 0,
            },
        );
        page.update_checksum();

        let err = delete_tuple(&mut page, 0, 2).unwrap_err();
        assert!(matches!(err, DbError::AlreadyDeleted { slot_id: 0, .. }));
    }

    // ── update ─────────────────────────────────────────────────────────────

    #[test]
    fn test_update_returns_new_slot() {
        let mut page = fresh_page();
        let old_slot = insert_tuple(&mut page, b"old", 1).unwrap();
        let new_slot = update_tuple(&mut page, old_slot, b"new", 2).unwrap();

        // Old slot: still physically present, marked deleted by txn 2.
        let (old_hdr, old_data) = read_tuple(&page, old_slot).unwrap().unwrap();
        assert_eq!(old_data, b"old");
        assert_eq!(old_hdr.txn_id_deleted, 2);

        // New slot: live, created by txn 2.
        let (new_hdr, new_data) = read_tuple(&page, new_slot).unwrap().unwrap();
        assert_eq!(new_data, b"new");
        assert_eq!(new_hdr.txn_id_created, 2);
        assert_eq!(new_hdr.txn_id_deleted, 0);

        assert_eq!(page.header().item_count, 2);
    }

    // ── free space + page full ──────────────────────────────────────────────

    #[test]
    fn test_free_space_decreases_correctly() {
        let mut page = fresh_page();
        let before = free_space(&page);
        insert_tuple(&mut page, b"hello", 1).unwrap(); // 5 bytes data
        let after = free_space(&page);
        // slot(4) + align8(24 + 5) = 4 + align8(29) = 4 + 32 = 36
        let expected = size_of::<SlotEntry>() + align8(size_of::<RowHeader>() + 5);
        assert_eq!(before - after, expected);
    }

    #[test]
    fn test_page_full_returns_error() {
        let mut page = fresh_page();
        // Fill the page with tuples.
        loop {
            // Use a zero-length data tuple (cheapest: 28 bytes).
            match insert_tuple(&mut page, &[], 1) {
                Ok(_) => {}
                Err(DbError::HeapPageFull { .. }) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }
        assert!(free_space(&page) < MIN_TUPLE_OVERHEAD);
    }

    #[test]
    fn test_invalid_slot_error() {
        let page = fresh_page();
        let err = read_tuple(&page, 999).unwrap_err();
        assert!(matches!(err, DbError::InvalidSlot { slot_id: 999, .. }));
    }

    // ── visibility ─────────────────────────────────────────────────────────

    fn snap(snapshot_id: u64, current_txn_id: u64) -> TransactionSnapshot {
        TransactionSnapshot {
            snapshot_id,
            current_txn_id,
        }
    }

    #[test]
    fn test_visibility_autocommit_insert() {
        // Use case 1: txn 1 inserts + commits. snapshot_id=2. Row visible.
        let hdr = RowHeader {
            txn_id_created: 1,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };
        assert!(hdr.is_visible(&snap(2, 0)));
    }

    #[test]
    fn test_visibility_uncommitted_row() {
        // Use case 2: txn 5 inserts (not committed). Reader at snapshot_id=5.
        let hdr = RowHeader {
            txn_id_created: 5,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };
        // snapshot_id=5, current_txn_id=0: txn 5 < 5? No. 5 == 0? No. Not visible.
        assert!(!hdr.is_visible(&snap(5, 0)));
    }

    #[test]
    fn test_visibility_read_own_writes() {
        // Use case 3: txn 5 inserts, same txn reads. current_txn_id=5.
        let hdr = RowHeader {
            txn_id_created: 5,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };
        assert!(hdr.is_visible(&snap(5, 5)));
    }

    #[test]
    fn test_visibility_delete_committed() {
        // Use case 4: txn 3 deletes (committed). Reader at snapshot_id=4.
        // txn_id_deleted=3 >= 4? No → row is gone.
        let hdr = RowHeader {
            txn_id_created: 1,
            txn_id_deleted: 3,
            row_version: 0,
            _flags: 0,
        };
        assert!(!hdr.is_visible(&snap(4, 0)));
    }

    #[test]
    fn test_visibility_delete_uncommitted() {
        // Use case 5: txn 3 deletes (not committed). Reader at snapshot_id=3.
        // txn_id_deleted=3 >= 3? Yes. txn_id_deleted=3 != current_txn_id=0? Yes → still visible.
        let hdr = RowHeader {
            txn_id_created: 1,
            txn_id_deleted: 3,
            row_version: 0,
            _flags: 0,
        };
        assert!(hdr.is_visible(&snap(3, 0)));
    }

    #[test]
    fn test_visibility_own_delete_invisible() {
        // Use case 6: txn 5 deletes its own row. Should be invisible to itself.
        let hdr = RowHeader {
            txn_id_created: 5,
            txn_id_deleted: 5,
            row_version: 0,
            _flags: 0,
        };
        // current_txn_id=5: created_visible=true (own write).
        // not_deleted: txn_id_deleted=5 >= snap.snapshot_id=5? Yes.
        //              txn_id_deleted=5 != current_txn_id=5? No → deleted by us → row gone.
        assert!(!hdr.is_visible(&snap(5, 5)));
    }

    #[test]
    fn test_visibility_dead_slot_skipped_by_scan() {
        // Use case 7: dead slot is never returned by scan_visible.
        let mut page = fresh_page();
        insert_tuple(&mut page, b"live", 1).unwrap();
        insert_tuple(&mut page, b"dead", 1).unwrap();
        // Kill slot 1.
        write_slot(
            &mut page,
            1,
            SlotEntry {
                offset: 0,
                length: 0,
            },
        );
        page.update_checksum();

        let visible: Vec<_> = scan_visible(&page, &snap(2, 0)).collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].0, 0);
        assert_eq!(visible[0].1, b"live");
    }

    #[test]
    fn test_scan_visible_filters_correctly() {
        // Three rows: committed by txn 1, committed by txn 2, in-progress txn 5.
        // Snapshot: snapshot_id=3, current_txn_id=0.
        // Expected visible: txn1 row and txn2 row. txn5 row not visible.
        let mut page = fresh_page();
        insert_tuple(&mut page, b"txn1", 1).unwrap();
        insert_tuple(&mut page, b"txn2", 2).unwrap();
        insert_tuple(&mut page, b"txn5", 5).unwrap();

        let snap3 = snap(3, 0);
        let visible: Vec<_> = scan_visible(&page, &snap3).collect();
        assert_eq!(visible.len(), 2);
        assert_eq!(visible[0].1, b"txn1");
        assert_eq!(visible[1].1, b"txn2");
    }

    #[test]
    fn test_scan_visible_skips_deleted() {
        let mut page = fresh_page();
        insert_tuple(&mut page, b"row0", 1).unwrap();
        insert_tuple(&mut page, b"row1", 1).unwrap();
        // Delete row0 in txn 2.
        delete_tuple(&mut page, 0, 2).unwrap();

        // Snapshot at 3: txn2 committed → row0 is gone.
        let visible: Vec<_> = scan_visible(&page, &snap(3, 0)).collect();
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].1, b"row1");
    }

    #[test]
    fn test_transaction_snapshot_committed_helper() {
        let s = TransactionSnapshot::committed(7);
        assert_eq!(s.snapshot_id, 8);
        assert_eq!(s.current_txn_id, 0);
    }

    #[test]
    fn test_transaction_snapshot_active_helper() {
        let s = TransactionSnapshot::active(10, 7);
        assert_eq!(s.snapshot_id, 8);
        assert_eq!(s.current_txn_id, 10);
    }
}
