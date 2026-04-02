use std::ops::Bound;

use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_storage::{
    clustered_internal, clustered_leaf, clustered_tree, MemoryStorage, PageType, RowHeader,
    StorageEngine,
};

fn row_header(txn_id: u64) -> RowHeader {
    RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: 0,
        _flags: 0,
    }
}

fn row_bytes(seed: u32, len: usize) -> Vec<u8> {
    (0..len)
        .map(|idx| ((seed as usize + idx) % 251) as u8)
        .collect()
}

fn clustered_page_type(page_type: u8) -> Result<PageType, DbError> {
    PageType::try_from(page_type)
}

fn leftmost_leaf_pid(storage: &dyn StorageEngine, mut pid: u64) -> Result<u64, DbError> {
    loop {
        let page = storage.read_page(pid)?;
        match clustered_page_type(page.header().page_type)? {
            PageType::ClusteredLeaf => return Ok(pid),
            PageType::ClusteredInternal => {
                pid = clustered_internal::child_at(&page, 0)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!("expected clustered tree page, found {other:?} at {pid}"),
                });
            }
        }
    }
}

fn collect_leaf_chain_keys(
    storage: &dyn StorageEngine,
    root_pid: u64,
) -> Result<Vec<Vec<u8>>, DbError> {
    let mut leaf_pid = leftmost_leaf_pid(storage, root_pid)?;
    let mut keys = Vec::new();

    loop {
        let page = storage.read_page(leaf_pid)?;
        let page_type = clustered_page_type(page.header().page_type)?;
        assert_eq!(page_type, PageType::ClusteredLeaf);

        for idx in 0..clustered_leaf::num_cells(&page) {
            keys.push(clustered_leaf::read_cell(&page, idx)?.key.to_vec());
        }

        let next = clustered_leaf::next_leaf(&page);
        if next == clustered_leaf::NULL_PAGE {
            break;
        }
        leaf_pid = next;
    }

    Ok(keys)
}

fn leaf_pid_for_key(storage: &dyn StorageEngine, mut pid: u64, key: &[u8]) -> Result<u64, DbError> {
    loop {
        let page = storage.read_page(pid)?;
        match clustered_page_type(page.header().page_type)? {
            PageType::ClusteredLeaf => return Ok(pid),
            PageType::ClusteredInternal => {
                let child_idx = clustered_internal::find_child_idx(&page, key)?;
                pid = clustered_internal::child_at(&page, child_idx as u16)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!("expected clustered tree page, found {other:?} at {pid}"),
                });
            }
        }
    }
}

#[test]
fn clustered_insert_keeps_ten_thousand_rows_sorted_across_root_splits() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..10_000 {
        let row_len = 96 + (key as usize % 5) * 211;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header((key % 19) as u64 + 1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let root_pid = root.unwrap();
    let root_page = storage.read_page(root_pid).unwrap();
    assert_eq!(
        clustered_page_type(root_page.header().page_type).unwrap(),
        PageType::ClusteredInternal
    );

    let keys = collect_leaf_chain_keys(storage.as_ref(), root_pid).unwrap();
    assert_eq!(keys.len(), 10_000);
    for (idx, key) in keys.iter().enumerate() {
        let expected = (idx as u32).to_be_bytes();
        assert_eq!(key.as_slice(), expected.as_slice());
    }
}

#[test]
fn clustered_lookup_returns_inline_rows_after_many_splits() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..10_000 {
        let row_len = 128 + (key as usize % 6) * 173;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header((key % 23) as u64 + 1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let root_pid = root.unwrap();
    let snapshot = TransactionSnapshot::committed(10_000);

    for probe in [0u32, 7, 63, 511, 4096, 9999] {
        let row = clustered_tree::lookup(
            storage.as_ref(),
            Some(root_pid),
            &probe.to_be_bytes(),
            &snapshot,
        )
        .unwrap()
        .expect("probe key must exist");

        let expected_len = 128 + (probe as usize % 6) * 173;
        assert_eq!(row.key, probe.to_be_bytes());
        assert_eq!(row.row_data, row_bytes(probe, expected_len));
        assert_eq!(row.row_header.txn_id_created, (probe % 23) as u64 + 1);
    }

    let missing = clustered_tree::lookup(
        storage.as_ref(),
        Some(root_pid),
        &10_001u32.to_be_bytes(),
        &snapshot,
    )
    .unwrap();
    assert!(missing.is_none());
}

#[test]
fn clustered_range_scan_returns_rows_in_order_across_many_leaves() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..10_000 {
        let row_len = 96 + (key as usize % 7) * 157;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header((key % 29) as u64 + 1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let rows: Vec<_> = clustered_tree::range(
        storage.as_ref(),
        root,
        Bound::Unbounded,
        Bound::Unbounded,
        &TransactionSnapshot::committed(20_000),
    )
    .unwrap()
    .map(|row| row.unwrap())
    .collect();

    assert_eq!(rows.len(), 10_000);
    for (idx, row) in rows.iter().enumerate() {
        let key = idx as u32;
        let expected_len = 96 + (key as usize % 7) * 157;
        assert_eq!(row.key, key.to_be_bytes());
        assert_eq!(row.row_data, row_bytes(key, expected_len));
    }
}

#[test]
fn clustered_range_scan_respects_bounds_across_leaf_splits() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..10_000 {
        let row_len = 160 + (key as usize % 5) * 181;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header((key % 13) as u64 + 1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let rows: Vec<_> = clustered_tree::range(
        storage.as_ref(),
        root,
        Bound::Included(1_234u32.to_be_bytes().to_vec()),
        Bound::Excluded(4_321u32.to_be_bytes().to_vec()),
        &TransactionSnapshot::committed(20_000),
    )
    .unwrap()
    .map(|row| row.unwrap())
    .collect();

    assert_eq!(rows.len(), 4_321 - 1_234);
    assert_eq!(rows.first().unwrap().key, 1_234u32.to_be_bytes());
    assert_eq!(rows.last().unwrap().key, 4_320u32.to_be_bytes());

    for (offset, row) in rows.iter().enumerate() {
        let key = 1_234u32 + offset as u32;
        let expected_len = 160 + (key as usize % 5) * 181;
        assert_eq!(row.key, key.to_be_bytes());
        assert_eq!(row.row_data, row_bytes(key, expected_len));
    }
}

#[test]
fn clustered_mixed_inline_and_overflow_rows_roundtrip_through_lookup_and_range() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;
    let overflow_threshold = clustered_leaf::max_inline_row_bytes(4).unwrap();

    for key in 0u32..512 {
        let row_len = if key % 3 == 0 {
            overflow_threshold + 700 + (key as usize % 5) * 17
        } else {
            120 + (key as usize % 7) * 31
        };
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header((key % 11) as u64 + 1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let snapshot = TransactionSnapshot::committed(10_000);

    for probe in [0u32, 3, 7, 42, 255, 511] {
        let expected_len = if probe % 3 == 0 {
            overflow_threshold + 700 + (probe as usize % 5) * 17
        } else {
            120 + (probe as usize % 7) * 31
        };
        let row = clustered_tree::lookup(
            storage.as_ref(),
            Some(root),
            &probe.to_be_bytes(),
            &snapshot,
        )
        .unwrap()
        .expect("probe key must exist");
        assert_eq!(row.row_data, row_bytes(probe, expected_len));
    }

    let overflow_leaf = leaf_pid_for_key(storage.as_ref(), root, &3u32.to_be_bytes()).unwrap();
    let overflow_page = storage.read_page(overflow_leaf).unwrap();
    let overflow_slot = clustered_leaf::search(&overflow_page, &3u32.to_be_bytes()).unwrap();
    let overflow_cell = clustered_leaf::read_cell(&overflow_page, overflow_slot as u16).unwrap();
    assert!(overflow_cell.overflow_first_page.is_some());
    assert_eq!(
        overflow_cell.total_row_len,
        overflow_threshold + 700 + (3usize % 5) * 17
    );

    let rows: Vec<_> = clustered_tree::range(
        storage.as_ref(),
        Some(root),
        Bound::Unbounded,
        Bound::Unbounded,
        &snapshot,
    )
    .unwrap()
    .map(|row| row.unwrap())
    .collect();
    assert_eq!(rows.len(), 512);
    for (idx, row) in rows.iter().enumerate() {
        let key = idx as u32;
        let expected_len = if key % 3 == 0 {
            overflow_threshold + 700 + (key as usize % 5) * 17
        } else {
            120 + (key as usize % 7) * 31
        };
        assert_eq!(row.key, key.to_be_bytes());
        assert_eq!(row.row_data, row_bytes(key, expected_len));
    }
}

#[test]
fn clustered_update_in_place_is_visible_to_lookup_and_range_on_split_tree() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..2_000 {
        let row_len = 180 + (key as usize % 5) * 121;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let changed = clustered_tree::update_in_place(
        storage.as_mut(),
        Some(root),
        &1_111u32.to_be_bytes(),
        &vec![7u8; 900],
        9,
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap();
    assert!(changed);

    let row = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &1_111u32.to_be_bytes(),
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap()
    .expect("updated row must exist");
    assert_eq!(row.row_data, vec![7u8; 900]);
    assert_eq!(row.row_header.txn_id_created, 9);
    assert_eq!(row.row_header.row_version, 1);

    let rows: Vec<_> = clustered_tree::range(
        storage.as_ref(),
        Some(root),
        Bound::Included(1_110u32.to_be_bytes().to_vec()),
        Bound::Included(1_112u32.to_be_bytes().to_vec()),
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap()
    .map(|row| row.unwrap())
    .collect();

    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].key, 1_111u32.to_be_bytes());
    assert_eq!(rows[1].row_data, vec![7u8; 900]);
}

#[test]
fn clustered_update_in_place_transitions_inline_and_overflow_and_frees_old_chain() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..256 {
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &row_bytes(key, 160),
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let overflow_len = clustered_leaf::max_inline_row_bytes(4).unwrap() + 900;
    let grown = clustered_tree::update_in_place(
        storage.as_mut(),
        Some(root),
        &42u32.to_be_bytes(),
        &row_bytes(42, overflow_len),
        9,
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap();
    assert!(grown);

    let leaf_pid = leaf_pid_for_key(storage.as_ref(), root, &42u32.to_be_bytes()).unwrap();
    let leaf_page = storage.read_page(leaf_pid).unwrap();
    let slot = clustered_leaf::search(&leaf_page, &42u32.to_be_bytes()).unwrap();
    let overflow_cell = clustered_leaf::read_cell(&leaf_page, slot as u16).unwrap();
    let old_overflow_page = overflow_cell
        .overflow_first_page
        .expect("updated row must be overflow-backed");
    assert_eq!(overflow_cell.total_row_len, overflow_len);

    let row = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &42u32.to_be_bytes(),
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap()
    .expect("grown row must remain visible");
    assert_eq!(row.row_data, row_bytes(42, overflow_len));
    assert_eq!(row.row_header.row_version, 1);

    let shrunk = clustered_tree::update_in_place(
        storage.as_mut(),
        Some(root),
        &42u32.to_be_bytes(),
        &row_bytes(42, 64),
        10,
        &TransactionSnapshot::active(10, 9),
    )
    .unwrap();
    assert!(shrunk);

    let leaf_page = storage.read_page(leaf_pid).unwrap();
    let slot = clustered_leaf::search(&leaf_page, &42u32.to_be_bytes()).unwrap();
    let inline_cell = clustered_leaf::read_cell(&leaf_page, slot as u16).unwrap();
    assert!(inline_cell.overflow_first_page.is_none());
    assert_eq!(inline_cell.total_row_len, 64);
    assert!(storage.read_page(old_overflow_page).is_err());

    let row = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &42u32.to_be_bytes(),
        &TransactionSnapshot::active(10, 9),
    )
    .unwrap()
    .expect("shrunk row must remain visible");
    assert_eq!(row.row_data, row_bytes(42, 64));
    assert_eq!(row.row_header.row_version, 2);
}

#[test]
fn clustered_update_in_place_reports_heap_page_full_when_same_leaf_growth_fails() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..7 {
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 2_100],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let err = clustered_tree::update_in_place(
        storage.as_mut(),
        Some(root),
        &3u32.to_be_bytes(),
        &vec![8u8; 8_000],
        10,
        &TransactionSnapshot::active(10, 1),
    )
    .unwrap_err();
    assert!(matches!(err, DbError::HeapPageFull { .. }));

    let row = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &3u32.to_be_bytes(),
        &TransactionSnapshot::committed(1),
    )
    .unwrap()
    .expect("failed update must keep original row");
    assert_eq!(row.row_data, vec![3u8; 2_100]);
    assert_eq!(row.row_header.txn_id_created, 1);
}

#[test]
fn clustered_delete_mark_keeps_overflow_chain_until_later_purge() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let key = 77u32;
    let overflow_len = clustered_leaf::max_inline_row_bytes(4).unwrap() + 333;
    let payload = row_bytes(key, overflow_len);
    let root = clustered_tree::insert(
        storage.as_mut(),
        None,
        &key.to_be_bytes(),
        &row_header(1),
        &payload,
    )
    .unwrap();

    let leaf_pid = leaf_pid_for_key(storage.as_ref(), root, &key.to_be_bytes()).unwrap();
    let leaf_page = storage.read_page(leaf_pid).unwrap();
    let slot = clustered_leaf::search(&leaf_page, &key.to_be_bytes()).unwrap();
    let overflow_first_page = clustered_leaf::read_cell(&leaf_page, slot as u16)
        .unwrap()
        .overflow_first_page
        .expect("row must be overflow-backed");

    let deleted = clustered_tree::delete_mark(
        storage.as_mut(),
        Some(root),
        &key.to_be_bytes(),
        9,
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap();
    assert!(deleted);
    assert!(storage.read_page(overflow_first_page).is_ok());

    let old_snapshot = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &key.to_be_bytes(),
        &TransactionSnapshot::committed(1),
    )
    .unwrap()
    .expect("old snapshot must still see delete-marked overflow row");
    assert_eq!(old_snapshot.row_data, payload);

    let new_snapshot = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &key.to_be_bytes(),
        &TransactionSnapshot::committed(9),
    )
    .unwrap();
    assert!(new_snapshot.is_none());
}

#[test]
fn clustered_delete_mark_respects_old_and_new_snapshots_on_split_tree() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..2_000 {
        let row_len = 180 + (key as usize % 5) * 121;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let deleted = clustered_tree::delete_mark(
        storage.as_mut(),
        Some(root),
        &1_111u32.to_be_bytes(),
        9,
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap();
    assert!(deleted);

    let current = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &1_111u32.to_be_bytes(),
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap();
    assert!(current.is_none());

    let new_snapshot = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &1_111u32.to_be_bytes(),
        &TransactionSnapshot::committed(9),
    )
    .unwrap();
    assert!(new_snapshot.is_none());

    let old_snapshot = clustered_tree::lookup(
        storage.as_ref(),
        Some(root),
        &1_111u32.to_be_bytes(),
        &TransactionSnapshot::committed(1),
    )
    .unwrap()
    .expect("older snapshot must still see delete-marked row");
    assert_eq!(old_snapshot.row_data, row_bytes(1_111, 301));
    assert_eq!(old_snapshot.row_header.txn_id_created, 1);
    assert_eq!(old_snapshot.row_header.txn_id_deleted, 9);
    assert_eq!(old_snapshot.row_header.row_version, 0);

    let current_rows: Vec<_> = clustered_tree::range(
        storage.as_ref(),
        Some(root),
        Bound::Included(1_110u32.to_be_bytes().to_vec()),
        Bound::Included(1_112u32.to_be_bytes().to_vec()),
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap()
    .map(|row| row.unwrap())
    .collect();
    assert_eq!(current_rows.len(), 2);
    assert_eq!(current_rows[0].key, 1_110u32.to_be_bytes());
    assert_eq!(current_rows[1].key, 1_112u32.to_be_bytes());

    let old_rows: Vec<_> = clustered_tree::range(
        storage.as_ref(),
        Some(root),
        Bound::Included(1_110u32.to_be_bytes().to_vec()),
        Bound::Included(1_112u32.to_be_bytes().to_vec()),
        &TransactionSnapshot::committed(1),
    )
    .unwrap()
    .map(|row| row.unwrap())
    .collect();
    assert_eq!(old_rows.len(), 3);
    assert_eq!(old_rows[1].key, 1_111u32.to_be_bytes());
    assert_eq!(old_rows[1].row_header.txn_id_deleted, 9);
}

#[test]
fn clustered_update_with_relocation_preserves_order_on_split_tree() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..2_000 {
        let row_len = 180 + (key as usize % 5) * 121;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let root_after = clustered_tree::update_with_relocation(
        storage.as_mut(),
        root,
        &1_111u32.to_be_bytes(),
        &vec![7u8; 8_000],
        9,
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap()
    .expect("relocation update must succeed");

    let row = clustered_tree::lookup(
        storage.as_ref(),
        Some(root_after),
        &1_111u32.to_be_bytes(),
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap()
    .expect("updated row must exist");
    assert_eq!(row.row_data, vec![7u8; 8_000]);
    assert_eq!(row.row_header.txn_id_created, 9);
    assert_eq!(row.row_header.row_version, 1);

    let old_snapshot = clustered_tree::lookup(
        storage.as_ref(),
        Some(root_after),
        &1_111u32.to_be_bytes(),
        &TransactionSnapshot::committed(1),
    )
    .unwrap();
    assert!(
        old_snapshot.is_none(),
        "39.8 relocation still rewrites the current inline version only"
    );

    let keys = collect_leaf_chain_keys(storage.as_ref(), root_after).unwrap();
    assert_eq!(keys.len(), 2_000);
    for (idx, key) in keys.iter().enumerate() {
        assert_eq!(key.as_slice(), &(idx as u32).to_be_bytes());
    }

    let rows: Vec<_> = clustered_tree::range(
        storage.as_ref(),
        Some(root_after),
        Bound::Included(1_110u32.to_be_bytes().to_vec()),
        Bound::Included(1_112u32.to_be_bytes().to_vec()),
        &TransactionSnapshot::active(9, 1),
    )
    .unwrap()
    .map(|row| row.unwrap())
    .collect();
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[1].key, 1_111u32.to_be_bytes());
    assert_eq!(rows[1].row_data, vec![7u8; 8_000]);
}
