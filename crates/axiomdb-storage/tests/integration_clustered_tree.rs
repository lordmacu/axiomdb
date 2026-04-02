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
