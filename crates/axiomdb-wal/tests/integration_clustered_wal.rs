use std::ops::Bound;

use axiomdb_storage::{
    clustered_tree::{self, ClusteredRow},
    MemoryStorage, RowHeader,
};
use axiomdb_wal::{ClusteredRowImage, TxnManager};
use tempfile::TempDir;

const TABLE_ID: u32 = 39;

fn temp_wal() -> (TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.wal");
    (dir, path)
}

fn row_header(txn_id: u64) -> RowHeader {
    RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: 0,
        _flags: 0,
    }
}

fn prime_max_committed(mgr: &mut TxnManager) {
    mgr.begin().expect("begin prime txn");
    mgr.commit().expect("commit prime txn");
}

fn row_image(root_pid: u64, row: &ClusteredRow) -> ClusteredRowImage {
    ClusteredRowImage::new(root_pid, row.row_header, &row.row_data)
}

fn collect_rows(
    storage: &MemoryStorage,
    root_pid: u64,
    snapshot: &axiomdb_core::TransactionSnapshot,
) -> Vec<ClusteredRow> {
    clustered_tree::range(
        storage,
        Some(root_pid),
        Bound::Unbounded,
        Bound::Unbounded,
        snapshot,
    )
    .expect("build clustered range")
    .collect::<Result<Vec<_>, _>>()
    .expect("collect clustered range")
}

#[test]
fn clustered_insert_rollback_handles_root_changes() {
    let (_dir, wal_path) = temp_wal();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");
    let mut storage = MemoryStorage::new();

    let txn_id = mgr.begin().expect("begin clustered txn");
    let mut root = None;

    for key in 0u32..128 {
        let key_bytes = key.to_be_bytes();
        let payload = vec![key as u8; 300];
        let header = row_header(txn_id);
        root = Some(
            clustered_tree::insert(&mut storage, root, &key_bytes, &header, &payload)
                .expect("clustered insert"),
        );
        let image = ClusteredRowImage::new(root.unwrap(), header, &payload);
        mgr.record_clustered_insert(TABLE_ID, &key_bytes, &image)
            .expect("record clustered insert");
    }

    mgr.rollback(&mut storage)
        .expect("rollback clustered inserts");

    let root_after = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root after rollback");
    let rows = collect_rows(&storage, root_after, &mgr.snapshot());
    assert!(
        rows.is_empty(),
        "rolled back clustered insert set must be empty"
    );
}

#[test]
fn clustered_delete_mark_rollback_restores_old_row() {
    let (_dir, wal_path) = temp_wal();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");
    let mut storage = MemoryStorage::new();
    prime_max_committed(&mut mgr);

    let key = b"pk-delete";
    let payload = b"live-row".to_vec();
    let root = clustered_tree::insert(&mut storage, None, key, &row_header(1), &payload)
        .expect("seed clustered row");

    let txn_id = mgr.begin().expect("begin delete-mark txn");
    let snapshot = mgr.active_snapshot().expect("active snapshot");
    let old_row = clustered_tree::lookup(&storage, Some(root), key, &snapshot)
        .expect("lookup old row")
        .expect("old row exists");

    assert!(
        clustered_tree::delete_mark(&mut storage, Some(root), key, txn_id, &snapshot)
            .expect("delete mark"),
        "delete mark must change the row"
    );

    let old_image = row_image(root, &old_row);
    let new_image = ClusteredRowImage::new(
        root,
        RowHeader {
            txn_id_created: old_row.row_header.txn_id_created,
            txn_id_deleted: txn_id,
            row_version: old_row.row_header.row_version,
            _flags: old_row.row_header._flags,
        },
        &old_row.row_data,
    );
    mgr.record_clustered_delete_mark(TABLE_ID, key, &old_image, &new_image)
        .expect("record clustered delete-mark");

    mgr.rollback(&mut storage)
        .expect("rollback clustered delete-mark");

    let root_after = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root after rollback");
    let restored = clustered_tree::lookup(&storage, Some(root_after), key, &mgr.snapshot())
        .expect("lookup restored row")
        .expect("row must exist after rollback");
    assert_eq!(restored.row_data, payload);
    assert_eq!(restored.row_header.txn_id_created, 1);
    assert_eq!(restored.row_header.txn_id_deleted, 0);
    assert_eq!(restored.row_header.row_version, 0);
}

#[test]
fn clustered_update_rollback_restores_old_overflow_backed_row() {
    let (_dir, wal_path) = temp_wal();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");
    let mut storage = MemoryStorage::new();
    prime_max_committed(&mut mgr);

    let key = b"pk-overflow";
    let old_payload = vec![7u8; 12_000];
    let new_payload = b"tiny".to_vec();
    let root = clustered_tree::insert(&mut storage, None, key, &row_header(1), &old_payload)
        .expect("seed overflow-backed row");

    let txn_id = mgr.begin().expect("begin update txn");
    let snapshot = mgr.active_snapshot().expect("active snapshot");
    let old_row = clustered_tree::lookup(&storage, Some(root), key, &snapshot)
        .expect("lookup old row")
        .expect("overflow-backed row exists");

    assert!(
        clustered_tree::update_in_place(
            &mut storage,
            Some(root),
            key,
            &new_payload,
            txn_id,
            &snapshot,
        )
        .expect("update in place"),
        "overflow-backed row must update in place"
    );

    let old_image = row_image(root, &old_row);
    let new_image = ClusteredRowImage::new(
        root,
        RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: old_row.row_header.row_version + 1,
            _flags: old_row.row_header._flags,
        },
        &new_payload,
    );
    mgr.record_clustered_update(TABLE_ID, key, &old_image, &new_image)
        .expect("record clustered update");

    mgr.rollback(&mut storage)
        .expect("rollback clustered overflow update");

    let root_after = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root after rollback");
    let restored = clustered_tree::lookup(&storage, Some(root_after), key, &mgr.snapshot())
        .expect("lookup restored overflow row")
        .expect("row must exist after rollback");
    assert_eq!(restored.row_data, old_payload);
    assert_eq!(restored.row_header.txn_id_created, 1);
    assert_eq!(restored.row_header.txn_id_deleted, 0);
    assert_eq!(restored.row_header.row_version, 0);
}

#[test]
fn clustered_relocate_update_rollback_restores_old_row() {
    let (_dir, wal_path) = temp_wal();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");
    let mut storage = MemoryStorage::new();
    prime_max_committed(&mut mgr);

    let mut root = None;
    for key in 0u32..7 {
        root = Some(
            clustered_tree::insert(
                &mut storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 2_100],
            )
            .expect("seed split-tree row"),
        );
    }

    let root = root.expect("seed root");
    let key = 3u32.to_be_bytes();
    let txn_id = mgr.begin().expect("begin relocate-update txn");
    let snapshot = mgr.active_snapshot().expect("active snapshot");
    let old_row = clustered_tree::lookup(&storage, Some(root), &key, &snapshot)
        .expect("lookup old row")
        .expect("old row exists");

    let root_after = clustered_tree::update_with_relocation(
        &mut storage,
        Some(root),
        &key,
        &vec![9u8; 8_000],
        txn_id,
        &snapshot,
    )
    .expect("relocate update")
    .expect("row must be updated by relocation");

    let new_row = clustered_tree::lookup(&storage, Some(root_after), &key, &snapshot)
        .expect("lookup relocated row")
        .expect("relocated row exists");

    let old_image = row_image(root_after, &old_row);
    let new_image = row_image(root_after, &new_row);
    mgr.record_clustered_update(TABLE_ID, &key, &old_image, &new_image)
        .expect("record relocate update");

    mgr.rollback(&mut storage)
        .expect("rollback relocate update");

    let root_final = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root after rollback");
    let restored = clustered_tree::lookup(&storage, Some(root_final), &key, &mgr.snapshot())
        .expect("lookup restored row")
        .expect("row must exist after rollback");
    assert_eq!(restored.row_data, old_row.row_data);
    assert_eq!(restored.row_header.txn_id_created, 1);
    assert_eq!(restored.row_header.txn_id_deleted, 0);
    assert_eq!(restored.row_header.row_version, 0);

    let rows = collect_rows(&storage, root_final, &mgr.snapshot());
    assert_eq!(rows.len(), 7);
}

#[test]
fn clustered_savepoint_undoes_only_late_writes() {
    let (_dir, wal_path) = temp_wal();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");
    let mut storage = MemoryStorage::new();

    let txn_id = mgr.begin().expect("begin clustered txn");
    let mut root = None;
    let key1 = 1u32.to_be_bytes();
    let key2 = 2u32.to_be_bytes();

    root = Some(
        clustered_tree::insert(&mut storage, root, &key1, &row_header(txn_id), b"row-1")
            .expect("insert first row"),
    );
    let image1 = ClusteredRowImage::new(root.unwrap(), row_header(txn_id), b"row-1");
    mgr.record_clustered_insert(TABLE_ID, &key1, &image1)
        .expect("record first insert");

    let sp = mgr.savepoint();

    root = Some(
        clustered_tree::insert(&mut storage, root, &key2, &row_header(txn_id), b"row-2")
            .expect("insert second row"),
    );
    let image2 = ClusteredRowImage::new(root.unwrap(), row_header(txn_id), b"row-2");
    mgr.record_clustered_insert(TABLE_ID, &key2, &image2)
        .expect("record second insert");

    mgr.rollback_to_savepoint(sp, &mut storage)
        .expect("rollback to savepoint");

    let root_after = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root after savepoint");
    let snapshot = mgr.active_snapshot().expect("active snapshot");

    let first = clustered_tree::lookup(&storage, Some(root_after), &key1, &snapshot)
        .expect("lookup first row");
    let second = clustered_tree::lookup(&storage, Some(root_after), &key2, &snapshot)
        .expect("lookup second row");

    assert!(first.is_some(), "first row must survive savepoint rollback");
    assert!(
        second.is_none(),
        "second row must be undone by savepoint rollback"
    );
}
