#[test]
fn range_empty_tree_returns_no_rows() {
    let storage = MemoryStorage::new();
    let rows = collect_range_rows(
        range(
            &storage,
            None,
            Bound::Unbounded,
            Bound::Unbounded,
            &committed_snapshot(100),
        )
        .unwrap(),
    )
    .unwrap();
    assert!(rows.is_empty());
}

#[test]
fn range_full_scan_returns_rows_in_primary_key_order() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..128 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header((key % 17) as u64 + 1),
                &row_bytes(key, 512 + (key as usize % 5) * 71),
            )
            .unwrap(),
        );
    }

    let rows = collect_range_rows(
        range(
            &storage,
            root,
            Bound::Unbounded,
            Bound::Unbounded,
            &committed_snapshot(10_000),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(rows.len(), 128);
    for (idx, row) in rows.iter().enumerate() {
        assert_eq!(row.key, (idx as u32).to_be_bytes());
    }
}

#[test]
fn range_included_and_excluded_bounds_are_respected() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..32 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &row_bytes(key, 64),
            )
            .unwrap(),
        );
    }

    let inclusive = collect_range_rows(
        range(
            &storage,
            root,
            Bound::Included(10u32.to_be_bytes().to_vec()),
            Bound::Included(15u32.to_be_bytes().to_vec()),
            &committed_snapshot(10),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(inclusive.len(), 6);
    assert_eq!(inclusive.first().unwrap().key, 10u32.to_be_bytes());
    assert_eq!(inclusive.last().unwrap().key, 15u32.to_be_bytes());

    let exclusive = collect_range_rows(
        range(
            &storage,
            root,
            Bound::Excluded(10u32.to_be_bytes().to_vec()),
            Bound::Excluded(15u32.to_be_bytes().to_vec()),
            &committed_snapshot(10),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(exclusive.len(), 4);
    assert_eq!(exclusive.first().unwrap().key, 11u32.to_be_bytes());
    assert_eq!(exclusive.last().unwrap().key, 14u32.to_be_bytes());
}

#[test]
fn range_skips_invisible_current_versions() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..8 {
        let created_by = if key % 2 == 0 { 9 } else { 2 };
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(created_by),
                &row_bytes(key, 96),
            )
            .unwrap(),
        );
    }

    let rows = collect_range_rows(
        range(
            &storage,
            root,
            Bound::Unbounded,
            Bound::Unbounded,
            &committed_snapshot(4),
        )
        .unwrap(),
    )
    .unwrap();

    let keys: Vec<Vec<u8>> = rows.into_iter().map(|row| row.key).collect();
    let expected: Vec<Vec<u8>> = [1u32, 3, 5, 7]
        .into_iter()
        .map(|key| key.to_be_bytes().to_vec())
        .collect();
    assert_eq!(keys, expected);
}

#[test]
fn bounded_range_starts_from_non_leftmost_leaf_when_possible() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..256 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 2_400],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let leftmost = leftmost_leaf_pid(&storage, root).unwrap();
    let (start_pid, slot_idx) = find_start_position(
        &storage,
        root,
        &Bound::Included(200u32.to_be_bytes().to_vec()),
    )
    .unwrap();

    assert_ne!(
        start_pid, leftmost,
        "bounded range should descend to the first relevant leaf"
    );

    let page = storage.read_page(start_pid).unwrap();
    let cell = clustered_leaf::read_cell(&page, slot_idx as u16).unwrap();
    assert_eq!(cell.key, 200u32.to_be_bytes());
}

#[test]
fn range_prefetches_when_advancing_to_next_leaf() {
    let prefetches = Arc::new(AtomicUsize::new(0));
    let storage = CountingPrefetchStorage {
        inner: MemoryStorage::new(),
        prefetches: Arc::clone(&prefetches),
    };
    let mut root = None;

    for key in 0u32..64 {
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 3_000],
            )
            .unwrap(),
        );
    }

    let rows = collect_range_rows(
        range(
            &storage,
            root,
            Bound::Unbounded,
            Bound::Unbounded,
            &committed_snapshot(10),
        )
        .unwrap(),
    )
    .unwrap();

    assert_eq!(rows.len(), 64);
    assert!(
        prefetches.load(Ordering::Relaxed) > 0,
        "cross-leaf scans must issue prefetch hints"
    );
}

#[test]
fn ten_thousand_mixed_rows_stay_sorted() {
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..10_000 {
        let row_len = 64 + (key as usize % 7) * 113;
        root = Some(
            insert(
                &storage,
                root,
                &key.to_be_bytes(),
                &row_header((key % 17) as u64 + 1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let keys = collect_leaf_chain_keys(&storage, root.unwrap()).unwrap();
    assert_eq!(keys.len(), 10_000);
    for (idx, key) in keys.iter().enumerate() {
        assert_eq!(key.as_slice(), &(idx as u32).to_be_bytes());
    }
}
