//! Clustered index leaf page — stores full row data inline in B-tree leaves.
//!
//! Uses a **SQLite-style cell pointer array** for variable-size cells:
//!
//! ```text
//! [PageHeader: 64B (managed by Page)]
//! Body (16,320 bytes):
//!   [ClusteredLeafHeader: 16B]
//!   [CellPtr 0: 2B][CellPtr 1: 2B]...[CellPtr N-1: 2B]  ← sorted by key
//!                      free space (gap)
//!   [Cell content area: cells in arbitrary order]          ← grows ←
//! ```
//!
//! Each cell:
//! ```text
//! [key_len: u16 LE][total_row_len: u32 LE][RowHeader: 24B][key_data]
//! [local_row_prefix][overflow_first_page?: u64 LE]
//! ```
//!
//! The cell pointer array is kept sorted by key, enabling binary search.
//! Cell content is allocated from the end of the page body growing leftward,
//! with a freeblock chain to reclaim space from deleted cells.

use axiomdb_core::error::DbError;

use crate::heap::RowHeader;
use crate::page::{Page, PageType, HEADER_SIZE, PAGE_SIZE};

include!("clef_access.rs");
include!("clef_write.rs");
include!("clef_modify.rs");

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;

    fn make_page() -> Page {
        let mut page = Page::new(PageType::ClusteredLeaf, 1);
        init_clustered_leaf(&mut page);
        page.update_checksum();
        page
    }

    fn make_row_header(txn_id: u64) -> RowHeader {
        RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        }
    }

    #[test]
    fn test_init_empty_page() {
        let page = make_page();
        assert_eq!(num_cells(&page), 0);
        assert_eq!(cell_content_start(&page), BODY_SIZE as u16);
        assert_eq!(freeblock_offset(&page), 0);
        assert_eq!(next_leaf(&page), NULL_PAGE);
        assert_eq!(free_space(&page), BODY_SIZE - CL_HEADER_SIZE);
    }

    #[test]
    fn test_insert_one_cell() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let key = b"key1";
        let data = b"hello world";

        insert_cell(&mut page, 0, key, &hdr, data).unwrap();
        assert_eq!(num_cells(&page), 1);

        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.key, b"key1");
        assert_eq!(cell.row_data, b"hello world");
        assert_eq!(cell.row_header.txn_id_created, 1);
    }

    #[test]
    fn test_insert_sorted_order() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert keys out of order, but at correct sorted positions.
        // "charlie" first
        let pos = search(&page, b"charlie").unwrap_err();
        insert_cell(&mut page, pos, b"charlie", &hdr, b"c").unwrap();

        // "alpha" before charlie
        let pos = search(&page, b"alpha").unwrap_err();
        insert_cell(&mut page, pos, b"alpha", &hdr, b"a").unwrap();

        // "bravo" between alpha and charlie
        let pos = search(&page, b"bravo").unwrap_err();
        insert_cell(&mut page, pos, b"bravo", &hdr, b"b").unwrap();

        assert_eq!(num_cells(&page), 3);

        // Verify sorted order.
        let c0 = read_cell(&page, 0).unwrap();
        let c1 = read_cell(&page, 1).unwrap();
        let c2 = read_cell(&page, 2).unwrap();
        assert_eq!(c0.key, b"alpha");
        assert_eq!(c1.key, b"bravo");
        assert_eq!(c2.key, b"charlie");
    }

    #[test]
    fn test_rewrite_same_key_same_size_overwrites_in_place() {
        let mut page = make_page();
        let old_hdr = make_row_header(3);
        let new_hdr = make_row_header(9);
        insert_cell(&mut page, 0, b"alpha", &old_hdr, b"hello").unwrap();

        let old_ptr = cell_ptr_at(&page, 0);
        let old_image = rewrite_cell_same_key(&mut page, 0, b"alpha", &new_hdr, b"world").unwrap();

        assert!(old_image.is_some());
        assert_eq!(cell_ptr_at(&page, 0), old_ptr);

        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.key, b"alpha");
        assert_eq!(cell.row_data, b"world");
        assert_eq!(cell.row_header.txn_id_created, 9);
    }

    #[test]
    fn test_rewrite_same_key_growth_rebuilds_same_leaf() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let new_hdr = make_row_header(7);
        set_next_leaf(&mut page, 777);

        for key in [1u32, 2, 3, 4] {
            let pos = search(&page, &key.to_be_bytes()).unwrap_err();
            insert_cell(
                &mut page,
                pos,
                &key.to_be_bytes(),
                &hdr,
                &vec![key as u8; 400],
            )
            .unwrap();
        }

        let before_free = free_space(&page);
        let old_next = next_leaf(&page);
        let old_num = num_cells(&page);

        let old_image = rewrite_cell_same_key(
            &mut page,
            2,
            &3u32.to_be_bytes(),
            &new_hdr,
            &vec![3u8; 2_000],
        )
        .unwrap();

        assert!(old_image.is_some());
        assert_eq!(next_leaf(&page), old_next);
        assert_eq!(num_cells(&page), old_num);
        assert!(free_space(&page) < before_free);

        let keys: Vec<Vec<u8>> = (0..num_cells(&page))
            .map(|idx| read_cell(&page, idx).unwrap().key.to_vec())
            .collect();
        assert_eq!(
            keys,
            vec![
                1u32.to_be_bytes().to_vec(),
                2u32.to_be_bytes().to_vec(),
                3u32.to_be_bytes().to_vec(),
                4u32.to_be_bytes().to_vec(),
            ]
        );

        let cell = read_cell(&page, 2).unwrap();
        assert_eq!(cell.row_header.txn_id_created, 7);
        assert_eq!(cell.row_data.len(), 2_000);
    }

    #[test]
    fn test_rewrite_same_key_returns_none_when_growth_no_longer_fits() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let new_hdr = make_row_header(8);

        for key in 0u32..7 {
            let pos = search(&page, &key.to_be_bytes()).unwrap_err();
            insert_cell(
                &mut page,
                pos,
                &key.to_be_bytes(),
                &hdr,
                &vec![key as u8; 2_100],
            )
            .unwrap();
        }

        let before = *page.as_bytes();
        let rewritten = rewrite_cell_same_key(
            &mut page,
            0,
            &0u32.to_be_bytes(),
            &new_hdr,
            &vec![9u8; max_inline_row_bytes(4).unwrap()],
        )
        .unwrap();

        assert!(rewritten.is_none());
        assert_eq!(page.as_bytes(), &before);
    }

    #[test]
    fn test_insert_overflow_backed_descriptor_roundtrips() {
        let mut page = make_page();
        let hdr = make_row_header(4);
        let key = b"overflowed";
        let local_len = max_inline_row_bytes(key.len()).unwrap();
        let total_row_len = local_len + 123;
        let local_row = vec![0x2A; local_len];

        insert_cell_with_overflow(
            &mut page,
            0,
            key,
            &hdr,
            total_row_len,
            &local_row,
            Some(777),
        )
        .unwrap();

        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.key, key);
        assert_eq!(cell.row_header.txn_id_created, 4);
        assert_eq!(cell.total_row_len, total_row_len);
        assert_eq!(cell.row_data, local_row);
        assert_eq!(cell.overflow_first_page, Some(777));
    }

    #[test]
    fn test_search_exact_and_miss() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        for key in [b"aaa" as &[u8], b"ccc", b"eee", b"ggg"] {
            let pos = search(&page, key).unwrap_err();
            insert_cell(&mut page, pos, key, &hdr, b"x").unwrap();
        }

        // Exact matches.
        assert_eq!(search(&page, b"aaa"), Ok(0));
        assert_eq!(search(&page, b"ccc"), Ok(1));
        assert_eq!(search(&page, b"eee"), Ok(2));
        assert_eq!(search(&page, b"ggg"), Ok(3));

        // Misses (insertion positions).
        assert_eq!(search(&page, b"000"), Err(0)); // before all
        assert_eq!(search(&page, b"bbb"), Err(1)); // between aaa and ccc
        assert_eq!(search(&page, b"ddd"), Err(2)); // between ccc and eee
        assert_eq!(search(&page, b"fff"), Err(3)); // between eee and ggg
        assert_eq!(search(&page, b"zzz"), Err(4)); // after all
    }

    #[test]
    fn test_insert_until_full() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let data = [0u8; 100]; // 100 bytes of row data

        let mut count = 0u32;
        loop {
            let key = count.to_be_bytes();
            let pos = search(&page, &key).unwrap_err();
            match insert_cell(&mut page, pos, &key, &hdr, &data) {
                Ok(()) => count += 1,
                Err(DbError::HeapPageFull { .. }) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }

        assert!(count > 100, "should fit >100 cells, got {count}");
        assert_eq!(num_cells(&page), count as u16);
    }

    #[test]
    fn test_remove_and_reuse() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert 10 cells.
        for i in 0u32..10 {
            let key = i.to_be_bytes();
            let pos = search(&page, &key).unwrap_err();
            insert_cell(&mut page, pos, &key, &hdr, b"data_here").unwrap();
        }
        assert_eq!(num_cells(&page), 10);
        let space_before = free_space(&page);

        // Remove cell at index 5.
        remove_cell(&mut page, 5).unwrap();
        assert_eq!(num_cells(&page), 9);
        let space_after = free_space(&page);
        assert!(
            space_after > space_before,
            "free space should increase after remove"
        );

        // Insert a new cell — should reuse freed space.
        let new_key = 5u32.to_be_bytes();
        let pos = search(&page, &new_key).unwrap_err();
        insert_cell(&mut page, pos, &new_key, &hdr, b"data_here").unwrap();
        assert_eq!(num_cells(&page), 10);
    }

    #[test]
    fn test_defragment() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert 20 cells.
        for i in 0u32..20 {
            let key = i.to_be_bytes();
            let pos = search(&page, &key).unwrap_err();
            insert_cell(&mut page, pos, &key, &hdr, b"test_data_here!!").unwrap();
        }

        // Remove every other cell (creates fragmentation).
        for i in (0..10).rev() {
            remove_cell(&mut page, i * 2).unwrap();
        }
        assert_eq!(num_cells(&page), 10);

        let space_before_defrag = free_space(&page);
        let gap_before = gap_space(&page);

        // Defragment.
        defragment(&mut page);

        let space_after = free_space(&page);
        let gap_after = gap_space(&page);

        // After defrag, all free space should be contiguous (gap = total free).
        assert_eq!(freeblock_offset(&page), 0, "no freeblocks after defrag");
        assert_eq!(gap_after, space_after, "all free space is gap after defrag");
        assert!(gap_after >= gap_before, "gap should not shrink");
        // Total free space is preserved (no data lost).
        assert_eq!(space_after, space_before_defrag);

        // Verify all remaining cells are intact and in order.
        for i in 0..10u16 {
            let cell = read_cell(&page, i).unwrap();
            let expected_key = ((i as u32) * 2 + 1).to_be_bytes();
            assert_eq!(
                cell.key, &expected_key,
                "cell {i} key mismatch after defrag"
            );
            assert_eq!(cell.row_data, b"test_data_here!!");
        }
    }

    #[test]
    fn test_next_leaf_chain() {
        let mut page = make_page();
        assert_eq!(next_leaf(&page), NULL_PAGE);

        set_next_leaf(&mut page, 42);
        assert_eq!(next_leaf(&page), 42);

        set_next_leaf(&mut page, NULL_PAGE);
        assert_eq!(next_leaf(&page), NULL_PAGE);
    }

    #[test]
    fn test_mvcc_visibility() {
        let mut page = make_page();

        // Insert a live cell (txn_id_deleted = 0).
        let hdr_live = RowHeader {
            txn_id_created: 10,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };
        insert_cell(&mut page, 0, b"live", &hdr_live, b"data").unwrap();

        // Insert a deleted cell (txn_id_deleted = 20).
        let hdr_dead = RowHeader {
            txn_id_created: 10,
            txn_id_deleted: 20,
            row_version: 1,
            _flags: 0,
        };
        let pos = search(&page, b"dead").unwrap_err();
        insert_cell(&mut page, pos, b"dead", &hdr_dead, b"gone").unwrap();

        // Read both cells and check MVCC fields.
        let live = read_cell(&page, search(&page, b"live").unwrap() as u16).unwrap();
        assert_eq!(live.row_header.txn_id_deleted, 0);

        let dead = read_cell(&page, search(&page, b"dead").unwrap() as u16).unwrap();
        assert_eq!(dead.row_header.txn_id_deleted, 20);
    }

    #[test]
    fn test_many_inserts_and_removes_stress() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert 50 cells.
        for i in 0u32..50 {
            let key = format!("{i:08}");
            let pos = search(&page, key.as_bytes()).unwrap_err();
            insert_cell(&mut page, pos, key.as_bytes(), &hdr, b"value").unwrap();
        }
        assert_eq!(num_cells(&page), 50);

        // Remove 25 cells.
        for i in (0..25).rev() {
            remove_cell(&mut page, i * 2).unwrap();
        }
        assert_eq!(num_cells(&page), 25);

        // Defragment.
        defragment(&mut page);

        // Insert 25 more cells.
        for i in 50u32..75 {
            let key = format!("{i:08}");
            let pos = search(&page, key.as_bytes()).unwrap_err();
            insert_cell(&mut page, pos, key.as_bytes(), &hdr, b"value").unwrap();
        }
        assert_eq!(num_cells(&page), 50);

        // Verify sorted order.
        for i in 0..49 {
            let c1 = read_cell(&page, i).unwrap();
            let c2 = read_cell(&page, i + 1).unwrap();
            assert!(c1.key < c2.key, "cells not sorted at {i}");
        }
    }

    #[test]
    fn test_rewrite_cell_key_mismatch_returns_error() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        insert_cell(&mut page, 0, b"alpha", &hdr, b"data").unwrap();

        let err = rewrite_cell_same_key(&mut page, 0, b"bravo", &hdr, b"new_data").unwrap_err();
        assert!(
            matches!(err, DbError::BTreeCorrupted { ref msg } if msg.contains("key mismatch")),
            "expected BTreeCorrupted key mismatch, got {err:?}"
        );

        // Page unchanged.
        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.key, b"alpha");
        assert_eq!(cell.row_data, b"data");
    }

    #[test]
    fn test_freeblock_reuse_after_remove_larger_cell() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert a large cell then a small cell.
        let pos = search(&page, b"aaa").unwrap_err();
        insert_cell(&mut page, pos, b"aaa", &hdr, &[0xAA; 500]).unwrap();
        let pos = search(&page, b"zzz").unwrap_err();
        insert_cell(&mut page, pos, b"zzz", &hdr, b"tiny").unwrap();

        let space_before_remove = free_space(&page);

        // Remove the large cell — creates a freeblock.
        remove_cell(&mut page, 0).unwrap(); // "aaa" is at index 0
        assert_eq!(num_cells(&page), 1);

        let space_after_remove = free_space(&page);
        assert!(space_after_remove > space_before_remove);

        // Insert a new cell that is smaller than the freeblock — should split
        // the freeblock and reuse part of it.
        let pos = search(&page, b"bbb").unwrap_err();
        insert_cell(&mut page, pos, b"bbb", &hdr, b"reused").unwrap();
        assert_eq!(num_cells(&page), 2);

        // Verify both cells readable.
        let c0 = read_cell(&page, 0).unwrap();
        let c1 = read_cell(&page, 1).unwrap();
        assert_eq!(c0.key, b"bbb");
        assert_eq!(c0.row_data, b"reused");
        assert_eq!(c1.key, b"zzz");
        assert_eq!(c1.row_data, b"tiny");
    }

    #[test]
    fn test_read_cell_out_of_bounds_returns_error() {
        let page = make_page();
        match read_cell(&page, 0) {
            Err(DbError::Other(msg)) => {
                assert!(msg.contains("out of range"), "unexpected message: {msg}");
            }
            Err(other) => panic!("expected Other error, got {other:?}"),
            Ok(_) => panic!("expected error for empty page read_cell(0)"),
        }
    }

    // ── In-place patch primitives ─────────────────────────────────────────────

    #[test]
    fn test_patch_field_in_place_basic() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        // row_data: [null_bitmap:1][i32:4][i64:8] — 13 bytes for schema (Int, BigInt)
        // null_bitmap=0, int_val=42, bigint_val=1000
        let mut row_data = [0u8; 13];
        row_data[1..5].copy_from_slice(&42i32.to_le_bytes());
        row_data[5..13].copy_from_slice(&1000i64.to_le_bytes());

        insert_cell(&mut page, 0, b"key1", &hdr, &row_data).unwrap();

        // Find where row_data starts (abs offset).
        let (row_data_abs, _key_len) = cell_row_data_abs_off(&page, 0).unwrap();

        // Patch the Int field (at row_data offset 1, size 4) to value 99.
        let new_int: [u8; 4] = 99i32.to_le_bytes();
        let field_abs = row_data_abs + 1; // bitmap(1) then int
        patch_field_in_place(&mut page, field_abs, &new_int).unwrap();

        // Verify via read_cell.
        let cell = read_cell(&page, 0).unwrap();
        let int_bytes = &cell.row_data[1..5];
        assert_eq!(i32::from_le_bytes(int_bytes.try_into().unwrap()), 99);

        // Surrounding bytes unchanged.
        assert_eq!(cell.row_data[0], 0); // null bitmap
        let bigint_bytes = &cell.row_data[5..13];
        assert_eq!(i64::from_le_bytes(bigint_bytes.try_into().unwrap()), 1000);
    }

    #[test]
    fn test_patch_field_in_place_out_of_bounds() {
        let mut page = make_page();
        let result = patch_field_in_place(&mut page, PAGE_SIZE - 2, &[1u8, 2, 3, 4]);
        assert!(result.is_err(), "expected error for out-of-bounds patch");
    }

    #[test]
    fn test_update_row_header_in_place_roundtrip() {
        let mut page = make_page();
        let old_hdr = RowHeader {
            txn_id_created: 7,
            txn_id_deleted: 0,
            row_version: 3,
            _flags: 5,
        };
        insert_cell(&mut page, 0, b"pk", &old_hdr, b"rowdata").unwrap();

        let new_hdr = RowHeader {
            txn_id_created: 42,
            txn_id_deleted: 0,
            row_version: 4,
            _flags: 5,
        };
        update_row_header_in_place(&mut page, 0, &new_hdr).unwrap();

        // Verify via read_cell.
        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.row_header.txn_id_created, 42);
        assert_eq!(cell.row_header.txn_id_deleted, 0);
        assert_eq!(cell.row_header.row_version, 4);
        assert_eq!(cell.row_header._flags, 5);

        // Key and row_data must be untouched.
        assert_eq!(cell.key, b"pk");
        assert_eq!(cell.row_data, b"rowdata");
    }

    #[test]
    fn test_update_row_header_in_place_out_of_bounds_idx() {
        let mut page = make_page();
        let new_hdr = make_row_header(1);
        let result = update_row_header_in_place(&mut page, 0, &new_hdr);
        assert!(result.is_err(), "expected error for cell_idx >= num_cells");
    }

    #[test]
    fn test_cell_row_data_abs_off_correct() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let row_data = b"hello_row";
        insert_cell(&mut page, 0, b"mykey", &hdr, row_data).unwrap();

        let (abs_off, key_len) = cell_row_data_abs_off(&page, 0).unwrap();
        assert_eq!(key_len, b"mykey".len());

        // Verify the bytes at abs_off match row_data.
        let b = page.as_bytes();
        assert_eq!(&b[abs_off..abs_off + row_data.len()], row_data);
    }

    #[test]
    fn test_patch_and_header_together() {
        // Simulate a full UPDATE in-place: patch a field + bump txn_id/row_version.
        let mut page = make_page();
        let old_hdr = RowHeader {
            txn_id_created: 10,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };
        // row_data: [bitmap:1][real:8] = 9 bytes, Real value = 1.0
        let mut row_data = [0u8; 9];
        row_data[1..9].copy_from_slice(&1.0f64.to_le_bytes());
        insert_cell(&mut page, 0, b"k", &old_hdr, &row_data).unwrap();

        let (rda, _) = cell_row_data_abs_off(&page, 0).unwrap();
        let field_abs = rda + 1; // skip bitmap byte

        // Read old Real value.
        let old_bytes = &page.as_bytes()[field_abs..field_abs + 8];
        assert_eq!(f64::from_le_bytes(old_bytes.try_into().unwrap()), 1.0);

        // Patch to 2.0.
        patch_field_in_place(&mut page, field_abs, &2.0f64.to_le_bytes()).unwrap();

        // Update header: txn_id_created=20, row_version=1.
        let new_hdr = RowHeader {
            txn_id_created: 20,
            txn_id_deleted: 0,
            row_version: 1,
            _flags: 0,
        };
        update_row_header_in_place(&mut page, 0, &new_hdr).unwrap();

        // Verify.
        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.row_header.txn_id_created, 20);
        assert_eq!(cell.row_header.row_version, 1);
        let real_bytes = &cell.row_data[1..9];
        assert_eq!(f64::from_le_bytes(real_bytes.try_into().unwrap()), 2.0);
        // Key unchanged.
        assert_eq!(cell.key, b"k");
    }
}
