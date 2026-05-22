#[test]
fn update_in_place_empty_tree_returns_false() {
    let storage = MemoryStorage::new();
    let changed = update_in_place(
        &storage,
        None,
        b"missing",
        b"new-row",
        9,
        &committed_snapshot(4),
    )
    .unwrap();
    assert!(!changed);
}

#[test]
fn update_in_place_missing_key_returns_false() {
    let storage = MemoryStorage::new();
    let mut root = None;
    for key in [b"alpha".as_slice(), b"bravo", b"charlie"] {
        root = Some(insert(&storage, root, key, &row_header(1), b"row").unwrap());
    }

    let changed = update_in_place(
        &storage,
        root,
        b"delta",
        b"updated",
        9,
        &committed_snapshot(4),
    )
    .unwrap();
    assert!(!changed);
}

#[test]
fn update_in_place_invisible_current_version_returns_false() {
    let storage = MemoryStorage::new();
    let root = insert(
        &storage,
        None,
        b"future",
        &row_header(9),
        b"not-committed-yet",
    )
    .unwrap();

    let changed = update_in_place(
        &storage,
        Some(root),
        b"future",
        b"replacement",
        12,
        &committed_snapshot(4),
    )
    .unwrap();
    assert!(!changed);

    let still_old = lookup(&storage, Some(root), b"future", &active_snapshot(9, 4))
        .unwrap()
        .expect("original row must stay unchanged");
    assert_eq!(still_old.row_data, b"not-committed-yet");
    assert_eq!(still_old.row_header.txn_id_created, 9);
}

#[test]
fn update_in_place_root_leaf_growth_rewrites_row_and_bumps_version() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..4 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 400],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let changed = update_in_place(
        &storage,
        Some(root),
        &2u32.to_be_bytes(),
        &vec![7u8; 2_000],
        9,
        &active_snapshot(9, 1),
    )
    .unwrap();
    assert!(changed);

    let row = lookup(
        &storage,
        Some(root),
        &2u32.to_be_bytes(),
        &active_snapshot(9, 1),
    )
    .unwrap()
    .expect("updated row must be visible to updater");
    assert_eq!(row.key, 2u32.to_be_bytes());
    assert_eq!(row.row_data, vec![7u8; 2_000]);
    assert_eq!(row.row_header.txn_id_created, 9);
    assert_eq!(row.row_header.row_version, 1);

    let old_snapshot = lookup(
        &storage,
        Some(root),
        &2u32.to_be_bytes(),
        &committed_snapshot(1),
    )
    .unwrap();
    assert!(old_snapshot.is_none());
}

#[test]
fn update_in_place_on_split_tree_preserves_leaf_identity_and_next_link() {
    let storage = MemoryStorage::new();
    let mut root = None;

    // Shuffled (coprime-stride) build so the 50/50 splits leave leaves partly
    // free: this test asserts the in-place grow keeps the row in the SAME leaf,
    // which needs slack. A pure-append build now packs leaves full via the
    // balance_quick append split, where a grow correctly relocates instead.
    for i in 0u32..128 {
        let key = (i * 53) % 128;
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 300],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let before_leaf = descend_to_leaf(&storage, root, &63u32.to_be_bytes())
        .unwrap()
        .header()
        .page_id;
    let before_next = {
        let page = storage.read_page(before_leaf).unwrap();
        clustered_leaf::next_leaf(&page)
    };

    let changed = update_in_place(
        &storage,
        Some(root),
        &63u32.to_be_bytes(),
        &vec![9u8; 700],
        11,
        &active_snapshot(11, 1),
    )
    .unwrap();
    assert!(changed);

    let after_leaf = descend_to_leaf(&storage, root, &63u32.to_be_bytes())
        .unwrap()
        .header()
        .page_id;
    let after_next = {
        let page = storage.read_page(after_leaf).unwrap();
        clustered_leaf::next_leaf(&page)
    };

    assert_eq!(after_leaf, before_leaf);
    assert_eq!(after_next, before_next);

    let row = lookup(
        &storage,
        Some(root),
        &63u32.to_be_bytes(),
        &active_snapshot(11, 1),
    )
    .unwrap()
    .expect("updated row must remain reachable");
    assert_eq!(row.row_data, vec![9u8; 700]);
    assert_eq!(row.row_header.row_version, 1);
}

#[test]
fn update_in_place_returns_heap_page_full_when_growth_cannot_stay_in_leaf() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..7 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 2_100],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let err = update_in_place(
        &storage,
        Some(root),
        &0u32.to_be_bytes(),
        &vec![9u8; 8_000],
        8,
        &active_snapshot(8, 1),
    )
    .unwrap_err();
    assert!(matches!(err, DbError::HeapPageFull { .. }));

    let row = lookup(
        &storage,
        Some(root),
        &0u32.to_be_bytes(),
        &committed_snapshot(1),
    )
    .unwrap()
    .expect("failed update must leave old row intact");
    assert_eq!(row.row_data, vec![0u8; 2_100]);
    assert_eq!(row.row_header.txn_id_created, 1);
}
