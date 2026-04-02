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
fn crash_recovery_undoes_uncommitted_clustered_insert() {
    let (_dir, wal_path) = temp_wal();
    let mut storage = MemoryStorage::new();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");

    let txn1 = mgr.begin().expect("begin seed txn");
    let seed_key = 10u32.to_be_bytes();
    let seed_payload = b"seed".to_vec();
    let mut root = Some(
        clustered_tree::insert(
            &mut storage,
            None,
            &seed_key,
            &row_header(txn1),
            &seed_payload,
        )
        .expect("seed clustered row"),
    );
    let seed_image =
        ClusteredRowImage::new(root.expect("seed root"), row_header(txn1), &seed_payload);
    mgr.record_clustered_insert(TABLE_ID, &seed_key, &seed_image)
        .expect("record seed insert");
    mgr.commit().expect("commit seed txn");

    let txn2 = mgr.begin().expect("begin crash txn");
    for key in 0u32..64 {
        let key_bytes = (1000 + key).to_be_bytes();
        let payload = vec![key as u8; 300];
        let header = row_header(txn2);
        root = Some(
            clustered_tree::insert(&mut storage, root, &key_bytes, &header, &payload)
                .expect("insert crash row"),
        );
        let image =
            ClusteredRowImage::new(root.expect("root after crash insert"), header, &payload);
        mgr.record_clustered_insert(TABLE_ID, &key_bytes, &image)
            .expect("record crash insert");
    }
    drop(mgr);

    let (mgr2, result) =
        TxnManager::open_with_recovery(&mut storage, &wal_path).expect("recover clustered inserts");
    let recovered_root = result
        .clustered_roots
        .get(&TABLE_ID)
        .copied()
        .expect("recovered clustered root");
    assert_eq!(mgr2.clustered_root(TABLE_ID), Some(recovered_root));

    let seed_row =
        clustered_tree::lookup(&storage, Some(recovered_root), &seed_key, &mgr2.snapshot())
            .expect("lookup seed row")
            .expect("seed row survives crash recovery");
    assert_eq!(seed_row.row_data, seed_payload);

    for key in 0u32..64 {
        let key_bytes = (1000 + key).to_be_bytes();
        let row =
            clustered_tree::lookup(&storage, Some(recovered_root), &key_bytes, &mgr2.snapshot())
                .expect("lookup crash row");
        assert!(
            row.is_none(),
            "uncommitted clustered insert {key} must be removed by crash recovery"
        );
    }
}

#[test]
fn crash_recovery_restores_clustered_delete_mark() {
    let (_dir, wal_path) = temp_wal();
    let mut storage = MemoryStorage::new();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");

    let txn1 = mgr.begin().expect("begin seed txn");
    let key = b"pk-delete";
    let payload = b"live-row".to_vec();
    let root = clustered_tree::insert(&mut storage, None, key, &row_header(txn1), &payload)
        .expect("seed clustered row");
    let image = ClusteredRowImage::new(root, row_header(txn1), &payload);
    mgr.record_clustered_insert(TABLE_ID, key, &image)
        .expect("record seed insert");
    mgr.commit().expect("commit seed txn");

    let txn2 = mgr.begin().expect("begin crash delete txn");
    let snapshot = mgr.active_snapshot().expect("active snapshot");
    let old_row = clustered_tree::lookup(&storage, Some(root), key, &snapshot)
        .expect("lookup old row")
        .expect("old row exists");
    assert!(
        clustered_tree::delete_mark(&mut storage, Some(root), key, txn2, &snapshot)
            .expect("delete mark"),
        "delete mark must succeed"
    );
    let old_image = row_image(root, &old_row);
    let new_image = ClusteredRowImage::new(
        root,
        RowHeader {
            txn_id_created: old_row.row_header.txn_id_created,
            txn_id_deleted: txn2,
            row_version: old_row.row_header.row_version,
            _flags: old_row.row_header._flags,
        },
        &old_row.row_data,
    );
    mgr.record_clustered_delete_mark(TABLE_ID, key, &old_image, &new_image)
        .expect("record clustered delete-mark");
    drop(mgr);

    let (mgr2, result) = TxnManager::open_with_recovery(&mut storage, &wal_path)
        .expect("recover clustered delete-mark");
    let recovered_root = result
        .clustered_roots
        .get(&TABLE_ID)
        .copied()
        .expect("recovered clustered root");
    let restored = clustered_tree::lookup(&storage, Some(recovered_root), key, &mgr2.snapshot())
        .expect("lookup restored row")
        .expect("row restored after crash recovery");
    assert_eq!(restored.row_data, payload);
    assert_eq!(restored.row_header.txn_id_created, txn1);
    assert_eq!(restored.row_header.txn_id_deleted, 0);
}

#[test]
fn crash_recovery_restores_overflow_backed_clustered_update() {
    let (_dir, wal_path) = temp_wal();
    let mut storage = MemoryStorage::new();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");

    let txn1 = mgr.begin().expect("begin seed txn");
    let key = b"pk-overflow";
    let old_payload = vec![7u8; 12_000];
    let root = clustered_tree::insert(&mut storage, None, key, &row_header(txn1), &old_payload)
        .expect("seed overflow-backed row");
    let image = ClusteredRowImage::new(root, row_header(txn1), &old_payload);
    mgr.record_clustered_insert(TABLE_ID, key, &image)
        .expect("record seed insert");
    mgr.commit().expect("commit seed txn");

    let txn2 = mgr.begin().expect("begin crash update txn");
    let snapshot = mgr.active_snapshot().expect("active snapshot");
    let old_row = clustered_tree::lookup(&storage, Some(root), key, &snapshot)
        .expect("lookup old row")
        .expect("old row exists");
    let new_payload = b"tiny".to_vec();
    assert!(
        clustered_tree::update_in_place(
            &mut storage,
            Some(root),
            key,
            &new_payload,
            txn2,
            &snapshot
        )
        .expect("update in place"),
        "overflow-backed row must update in place"
    );
    let old_image = row_image(root, &old_row);
    let new_image = ClusteredRowImage::new(
        root,
        RowHeader {
            txn_id_created: txn2,
            txn_id_deleted: 0,
            row_version: old_row.row_header.row_version + 1,
            _flags: old_row.row_header._flags,
        },
        &new_payload,
    );
    mgr.record_clustered_update(TABLE_ID, key, &old_image, &new_image)
        .expect("record clustered update");
    drop(mgr);

    let (mgr2, result) =
        TxnManager::open_with_recovery(&mut storage, &wal_path).expect("recover clustered update");
    let recovered_root = result
        .clustered_roots
        .get(&TABLE_ID)
        .copied()
        .expect("recovered clustered root");
    let restored = clustered_tree::lookup(&storage, Some(recovered_root), key, &mgr2.snapshot())
        .expect("lookup restored overflow row")
        .expect("overflow row restored");
    assert_eq!(restored.row_data, old_payload);
    assert_eq!(restored.row_header.txn_id_created, txn1);
    assert_eq!(restored.row_header.txn_id_deleted, 0);
    assert_eq!(restored.row_header.row_version, 0);
}

#[test]
fn crash_recovery_restores_relocate_update_logically() {
    let (_dir, wal_path) = temp_wal();
    let mut storage = MemoryStorage::new();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");

    let txn1 = mgr.begin().expect("begin seed txn");
    let mut root = None;
    for key in 0u32..7 {
        let key_bytes = key.to_be_bytes();
        let payload = vec![key as u8; 2_100];
        root = Some(
            clustered_tree::insert(&mut storage, root, &key_bytes, &row_header(txn1), &payload)
                .expect("seed split-tree row"),
        );
        let image = ClusteredRowImage::new(root.expect("seed root"), row_header(txn1), &payload);
        mgr.record_clustered_insert(TABLE_ID, &key_bytes, &image)
            .expect("record seed insert");
    }
    mgr.commit().expect("commit seed txn");

    let root = mgr
        .clustered_root(TABLE_ID)
        .expect("clustered root after seed");
    let key = 3u32.to_be_bytes();
    let txn2 = mgr.begin().expect("begin crash relocate txn");
    let snapshot = mgr.active_snapshot().expect("active snapshot");
    let old_row = clustered_tree::lookup(&storage, Some(root), &key, &snapshot)
        .expect("lookup old row")
        .expect("old row exists");
    let root_after = clustered_tree::update_with_relocation(
        &mut storage,
        Some(root),
        &key,
        &vec![9u8; 8_000],
        txn2,
        &snapshot,
    )
    .expect("relocate update")
    .expect("row must relocate");
    let new_row = clustered_tree::lookup(&storage, Some(root_after), &key, &snapshot)
        .expect("lookup relocated row")
        .expect("relocated row exists");
    let old_image = row_image(root_after, &old_row);
    let new_image = row_image(root_after, &new_row);
    mgr.record_clustered_update(TABLE_ID, &key, &old_image, &new_image)
        .expect("record relocate update");
    drop(mgr);

    let (mgr2, result) =
        TxnManager::open_with_recovery(&mut storage, &wal_path).expect("recover relocate update");
    let recovered_root = result
        .clustered_roots
        .get(&TABLE_ID)
        .copied()
        .expect("recovered clustered root");
    let restored = clustered_tree::lookup(&storage, Some(recovered_root), &key, &mgr2.snapshot())
        .expect("lookup restored row")
        .expect("row restored after relocate recovery");
    assert_eq!(restored.row_data, old_row.row_data);
    assert_eq!(restored.row_header.txn_id_created, txn1);
    assert_eq!(restored.row_header.txn_id_deleted, 0);

    let rows = collect_rows(&storage, recovered_root, &mgr2.snapshot());
    assert_eq!(rows.len(), 7);
}

#[test]
fn clean_reopen_restores_last_committed_clustered_root() {
    let (_dir, wal_path) = temp_wal();
    let mut storage = MemoryStorage::new();
    let mut mgr = TxnManager::create(&wal_path).expect("create wal");

    let txn1 = mgr.begin().expect("begin first txn");
    let key1 = 1u32.to_be_bytes();
    let payload1 = b"row-1".to_vec();
    let root1 = clustered_tree::insert(&mut storage, None, &key1, &row_header(txn1), &payload1)
        .expect("insert first committed row");
    let image1 = ClusteredRowImage::new(root1, row_header(txn1), &payload1);
    mgr.record_clustered_insert(TABLE_ID, &key1, &image1)
        .expect("record first insert");
    mgr.commit().expect("commit first txn");

    let txn2 = mgr.begin().expect("begin second txn");
    let mut root = Some(root1);
    for key in 2u32..48 {
        let key_bytes = key.to_be_bytes();
        let payload = vec![key as u8; 280];
        root = Some(
            clustered_tree::insert(&mut storage, root, &key_bytes, &row_header(txn2), &payload)
                .expect("insert committed row"),
        );
        let image =
            ClusteredRowImage::new(root.expect("root after insert"), row_header(txn2), &payload);
        mgr.record_clustered_insert(TABLE_ID, &key_bytes, &image)
            .expect("record committed insert");
    }
    mgr.commit().expect("commit second txn");
    let committed_root = mgr
        .clustered_root(TABLE_ID)
        .expect("committed clustered root");
    drop(mgr);

    let mgr2 = TxnManager::open(&wal_path).expect("clean reopen txn manager");
    let reopened_root = mgr2
        .clustered_root(TABLE_ID)
        .expect("clustered root after clean reopen");
    assert_eq!(reopened_root, committed_root);

    let rows = collect_rows(&storage, reopened_root, &mgr2.snapshot());
    assert_eq!(rows.len(), 47);
    let first = clustered_tree::lookup(&storage, Some(reopened_root), &key1, &mgr2.snapshot())
        .expect("lookup first committed row")
        .expect("first committed row exists");
    assert_eq!(first.row_data, payload1);
}
