#[test]
fn lookup_empty_tree_returns_none() {
    let storage = MemoryStorage::new();
    let got = lookup(&storage, None, b"missing", &committed_snapshot(100)).unwrap();
    assert!(got.is_none());
}

#[test]
fn lookup_root_leaf_hit_returns_inline_row() {
    let mut storage = MemoryStorage::new();
    let root = insert(&mut storage, None, b"pk-1", &row_header(3), b"row-inline").unwrap();

    let got = lookup(&storage, Some(root), b"pk-1", &committed_snapshot(10))
        .unwrap()
        .expect("row must exist");

    assert_eq!(got.key, b"pk-1");
    assert_eq!(got.row_data, b"row-inline");
    assert_eq!(got.row_header.txn_id_created, 3);
}

#[test]
fn lookup_missing_key_returns_none() {
    let mut storage = MemoryStorage::new();
    let mut root = None;
    for key in [b"alpha".as_slice(), b"bravo", b"charlie"] {
        root = Some(insert(&mut storage, root, key, &row_header(1), b"row").unwrap());
    }

    let got = lookup(&storage, root, b"delta", &committed_snapshot(10)).unwrap();
    assert!(got.is_none());
}

#[test]
fn lookup_invisible_current_version_returns_none() {
    let mut storage = MemoryStorage::new();
    let root = insert(
        &mut storage,
        None,
        b"future",
        &row_header(9),
        b"not-committed-yet",
    )
    .unwrap();

    let invisible = lookup(&storage, Some(root), b"future", &committed_snapshot(4)).unwrap();
    assert!(invisible.is_none());

    let visible_to_self = lookup(&storage, Some(root), b"future", &active_snapshot(9, 4))
        .unwrap()
        .expect("own write must be visible");
    assert_eq!(visible_to_self.row_data, b"not-committed-yet");
}

#[test]
fn lookup_after_internal_splits_finds_exact_row() {
    let mut storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..128 {
        root = Some(
            insert(
                &mut storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &row_bytes(key, 3_000 + (key as usize % 3) * 97),
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let got = lookup(
        &storage,
        Some(root),
        &93u32.to_be_bytes(),
        &committed_snapshot(10),
    )
    .unwrap()
    .expect("row must exist after splits");

    assert_eq!(got.key, 93u32.to_be_bytes());
    assert_eq!(got.row_data, row_bytes(93, 3_000));
}

