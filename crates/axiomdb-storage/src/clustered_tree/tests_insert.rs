#[test]
fn insert_bootstraps_empty_tree() {
    let storage = MemoryStorage::new();
    let root = insert(&storage, None, b"pk-1", &row_header(11), b"row-1").unwrap();
    let page = storage.read_page(root).unwrap();

    assert_eq!(clustered_page_type(&page).unwrap(), PageType::ClusteredLeaf);
    assert_eq!(clustered_leaf::num_cells(&page), 1);
    let cell = clustered_leaf::read_cell(&page, 0).unwrap();
    assert_eq!(cell.key, b"pk-1");
    assert_eq!(cell.row_data, b"row-1");
    assert_eq!(cell.row_header.txn_id_created, 11);
}

#[test]
fn duplicate_key_is_rejected() {
    let storage = MemoryStorage::new();
    let root = insert(&storage, None, b"dup", &row_header(1), b"a").unwrap();
    let err = insert(&storage, Some(root), b"dup", &row_header(2), b"b").unwrap_err();
    assert!(matches!(err, DbError::DuplicateKey));
}

#[test]
fn non_split_leaf_insert_preserves_sorted_order() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in [b"charlie".as_slice(), b"alpha", b"bravo"] {
        root = Some(insert(&storage, root, key, &row_header(1), b"row").unwrap());
    }

    let keys = collect_leaf_chain_keys(&storage, root.unwrap()).unwrap();
    assert_eq!(
        keys,
        vec![b"alpha".to_vec(), b"bravo".to_vec(), b"charlie".to_vec()]
    );
}

#[test]
fn defrag_happens_before_split() {
    let storage = MemoryStorage::new();
    let root_pid = storage.alloc_page(PageType::ClusteredLeaf).unwrap();
    let mut root = Page::new(PageType::ClusteredLeaf, root_pid);
    clustered_leaf::init_clustered_leaf(&mut root);

    let hdr = row_header(1);
    let filler = vec![7u8; 2_000];
    for key in 1u32..=7 {
        let pos = clustered_leaf::num_cells(&root) as usize;
        clustered_leaf::insert_cell(&mut root, pos, &key.to_be_bytes(), &hdr, &filler).unwrap();
    }
    clustered_leaf::remove_cell(&mut root, 3).unwrap();
    clustered_leaf::remove_cell(&mut root, 1).unwrap();
    root.update_checksum();
    storage.write_page(root_pid, &root).unwrap();

    let gap_before = {
        let page = storage.read_page(root_pid).unwrap();
        let free = clustered_leaf::free_space(&page);
        let mut page = page.into_page();
        match clustered_leaf::insert_cell(
            &mut page,
            1,
            &4u32.to_be_bytes(),
            &hdr,
            &vec![9u8; 3_000],
        ) {
            Ok(()) => panic!("test setup should require defragmentation"),
            Err(DbError::HeapPageFull { .. }) => {}
            Err(err) => panic!("unexpected setup error: {err}"),
        }
        free
    };
    assert!(gap_before >= clustered_leaf::cell_footprint(4, 3_000));

    let root_after = insert(
        &storage,
        Some(root_pid),
        &4u32.to_be_bytes(),
        &hdr,
        &vec![9u8; 3_000],
    )
    .unwrap();
    assert_eq!(root_after, root_pid, "defrag should avoid a split");

    let page = storage.read_page(root_pid).unwrap();
    assert_eq!(clustered_page_type(&page).unwrap(), PageType::ClusteredLeaf);
    assert_eq!(clustered_leaf::num_cells(&page), 6);
    let keys: Vec<Vec<u8>> = (0..clustered_leaf::num_cells(&page))
        .map(|idx| clustered_leaf::read_cell(&page, idx).unwrap().key.to_vec())
        .collect();
    assert_eq!(
        keys,
        vec![
            1u32.to_be_bytes().to_vec(),
            3u32.to_be_bytes().to_vec(),
            4u32.to_be_bytes().to_vec(),
            5u32.to_be_bytes().to_vec(),
            6u32.to_be_bytes().to_vec(),
            7u32.to_be_bytes().to_vec(),
        ]
    );
}

#[test]
fn leaf_split_sets_separator_and_next_leaf_chain() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..8 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 2_700],
            )
            .unwrap(),
        );
    }

    let root_pid = root.unwrap();
    let root_page = storage.read_page(root_pid).unwrap();
    assert_eq!(
        clustered_page_type(&root_page).unwrap(),
        PageType::ClusteredInternal
    );
    assert_eq!(clustered_internal::num_cells(&root_page), 1);

    let left_pid = clustered_internal::child_at(&root_page, 0).unwrap();
    let right_pid = clustered_internal::child_at(&root_page, 1).unwrap();
    let left = storage.read_page(left_pid).unwrap();
    let right = storage.read_page(right_pid).unwrap();

    assert_eq!(clustered_leaf::next_leaf(&left), right_pid);
    assert_eq!(clustered_leaf::next_leaf(&right), clustered_leaf::NULL_PAGE);
    let sep = clustered_internal::key_at(&root_page, 0).unwrap().to_vec();
    let right_first = clustered_leaf::read_cell(&right, 0).unwrap().key.to_vec();
    assert_eq!(sep, right_first);

    let keys = collect_leaf_chain_keys(&storage, root_pid).unwrap();
    let expected: Vec<Vec<u8>> = (0u32..8).map(|v| v.to_be_bytes().to_vec()).collect();
    assert_eq!(keys, expected);
}

#[test]
fn internal_split_and_root_split_keep_keys_reachable() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..64 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 3_200],
            )
            .unwrap(),
        );
    }

    let root_pid = root.unwrap();
    let root_page = storage.read_page(root_pid).unwrap();
    assert_eq!(
        clustered_page_type(&root_page).unwrap(),
        PageType::ClusteredInternal
    );
    assert!(
        clustered_internal::num_cells(&root_page) >= 2,
        "expected root to absorb multiple separators after deeper splits"
    );

    let keys = collect_leaf_chain_keys(&storage, root_pid).unwrap();
    let expected: Vec<Vec<u8>> = (0u32..64).map(|v| v.to_be_bytes().to_vec()).collect();
    assert_eq!(keys, expected);
}

#[test]
fn rows_that_need_overflow_are_materialized_and_reconstructed() {
    let storage = MemoryStorage::new();
    let key = b"overflow-pk";
    let total_len = clustered_leaf::max_inline_row_bytes(key.len()).unwrap() + 257;
    let payload = vec![0x6D; total_len];

    let root = insert(&storage, None, key, &row_header(1), &payload).unwrap();
    let page = storage.read_page(root).unwrap();
    let cell = clustered_leaf::read_cell(&page, 0).unwrap();

    assert_eq!(cell.total_row_len, total_len);
    assert_eq!(
        cell.row_data.len(),
        clustered_leaf::local_row_len(key.len(), total_len)
    );
    assert!(cell.overflow_first_page.is_some());

    let row = lookup(&storage, Some(root), key, &committed_snapshot(10))
        .unwrap()
        .expect("overflow-backed row must be reachable");
    assert_eq!(row.row_data, payload);
}

/// Phase 40.8c: many inserts grow the clustered tree past a single internal
/// level, exercising the early-X-latch-release path on every safe descent.
/// The final tree must remain functionally correct (all keys reachable in
/// sorted order via the leaf chain).
#[test]
fn many_inserts_with_safe_descent_keep_tree_consistent() {
    let storage = MemoryStorage::new();
    let mut root = None;

    let count = 2_000u32;
    for key in 0..count {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                b"row-payload",
            )
            .unwrap(),
        );
    }

    let root_pid = root.unwrap();
    let root_page = storage.read_page(root_pid).unwrap();
    assert_eq!(
        clustered_page_type(&root_page).unwrap(),
        PageType::ClusteredInternal,
        "root must be internal after {count} inserts so the descent has at least one early-release opportunity"
    );

    let keys = collect_leaf_chain_keys(&storage, root_pid).unwrap();
    let expected: Vec<Vec<u8>> = (0u32..count).map(|v| v.to_be_bytes().to_vec()).collect();
    assert_eq!(keys, expected);

    // Spot-check point lookups for the start, middle, and end keys.
    let snap = committed_snapshot(10);
    for k in [0u32, count / 2, count - 1] {
        let row = lookup(&storage, Some(root_pid), &k.to_be_bytes(), &snap)
            .unwrap()
            .unwrap_or_else(|| panic!("key {k} should be reachable after early-release inserts"));
        assert_eq!(row.row_data, b"row-payload");
    }
}

