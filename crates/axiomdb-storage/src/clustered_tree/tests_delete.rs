#[test]
fn delete_mark_empty_tree_returns_false() {
    let mut storage = MemoryStorage::new();
    let changed = delete_mark(&mut storage, None, b"missing", 9, &committed_snapshot(4)).unwrap();
    assert!(!changed);
}

#[test]
fn delete_mark_missing_key_returns_false() {
    let mut storage = MemoryStorage::new();
    let mut root = None;
    for key in [b"alpha".as_slice(), b"bravo", b"charlie"] {
        root = Some(insert(&mut storage, root, key, &row_header(1), b"row").unwrap());
    }

    let changed = delete_mark(&mut storage, root, b"delta", 9, &committed_snapshot(4)).unwrap();
    assert!(!changed);
}

#[test]
fn delete_mark_invisible_current_version_returns_false() {
    let mut storage = MemoryStorage::new();
    let root = insert(
        &mut storage,
        None,
        b"future",
        &row_header(9),
        b"not-committed-yet",
    )
    .unwrap();

    let changed = delete_mark(
        &mut storage,
        Some(root),
        b"future",
        12,
        &committed_snapshot(4),
    )
    .unwrap();
    assert!(!changed);

    let still_visible = lookup(&storage, Some(root), b"future", &active_snapshot(9, 4))
        .unwrap()
        .expect("original row must stay unchanged");
    assert_eq!(still_visible.row_data, b"not-committed-yet");
    assert_eq!(still_visible.row_header.txn_id_deleted, 0);
}

#[test]
fn delete_mark_root_leaf_hides_row_from_newer_snapshots_but_preserves_old_visibility() {
    let mut storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..4 {
        root = Some(
            insert(
                &mut storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 400],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let deleted = delete_mark(
        &mut storage,
        Some(root),
        &2u32.to_be_bytes(),
        9,
        &active_snapshot(9, 1),
    )
    .unwrap();
    assert!(deleted);

    let hidden_from_deleter = lookup(
        &storage,
        Some(root),
        &2u32.to_be_bytes(),
        &active_snapshot(9, 1),
    )
    .unwrap();
    assert!(hidden_from_deleter.is_none());

    let hidden_from_new_snapshot = lookup(
        &storage,
        Some(root),
        &2u32.to_be_bytes(),
        &committed_snapshot(9),
    )
    .unwrap();
    assert!(hidden_from_new_snapshot.is_none());

    let old_snapshot = lookup(
        &storage,
        Some(root),
        &2u32.to_be_bytes(),
        &committed_snapshot(1),
    )
    .unwrap()
    .expect("older snapshot must still see delete-marked row");
    assert_eq!(old_snapshot.key, 2u32.to_be_bytes());
    assert_eq!(old_snapshot.row_data, vec![2u8; 400]);
    assert_eq!(old_snapshot.row_header.txn_id_created, 1);
    assert_eq!(old_snapshot.row_header.txn_id_deleted, 9);
    assert_eq!(old_snapshot.row_header.row_version, 0);

    let current_rows = collect_range_rows(
        range(
            &storage,
            Some(root),
            Bound::Unbounded,
            Bound::Unbounded,
            &active_snapshot(9, 1),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(current_rows.len(), 3);
    assert!(!current_rows.iter().any(|row| row.key == 2u32.to_be_bytes()));

    let old_rows = collect_range_rows(
        range(
            &storage,
            Some(root),
            Bound::Unbounded,
            Bound::Unbounded,
            &committed_snapshot(1),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(old_rows.len(), 4);
    assert!(old_rows.iter().any(|row| row.key == 2u32.to_be_bytes()));
}

#[test]
fn delete_mark_on_split_tree_preserves_leaf_identity_and_next_link() {
    let mut storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..128 {
        root = Some(
            insert(
                &mut storage,
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

    let deleted = delete_mark(
        &mut storage,
        Some(root),
        &63u32.to_be_bytes(),
        11,
        &active_snapshot(11, 1),
    )
    .unwrap();
    assert!(deleted);

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

    let old_snapshot = lookup(
        &storage,
        Some(root),
        &63u32.to_be_bytes(),
        &committed_snapshot(1),
    )
    .unwrap()
    .expect("older snapshot must still see delete-marked row");
    assert_eq!(old_snapshot.row_header.txn_id_deleted, 11);

    let new_snapshot = lookup(
        &storage,
        Some(root),
        &63u32.to_be_bytes(),
        &committed_snapshot(11),
    )
    .unwrap();
    assert!(new_snapshot.is_none());
}

#[test]
fn delete_physical_repairs_separator_for_non_leftmost_leaf() {
    let mut storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..64 {
        root = Some(
            insert(
                &mut storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 700],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let root_page = storage.read_page(root).unwrap();
    assert_eq!(
        clustered_page_type(&root_page).unwrap(),
        PageType::ClusteredInternal
    );
    let target_leaf_pid = clustered_internal::child_at(&root_page, 1).unwrap();
    let target_leaf = storage.read_page(target_leaf_pid).unwrap();
    let old_first = clustered_leaf::read_cell(&target_leaf, 0)
        .unwrap()
        .key
        .to_vec();
    let new_first = clustered_leaf::read_cell(&target_leaf, 1)
        .unwrap()
        .key
        .to_vec();
    assert_eq!(
        clustered_internal::key_at(&root_page, 0).unwrap(),
        old_first.as_slice()
    );

    let (deleted, root_after) = delete_physical(&mut storage, root, &old_first).unwrap();
    assert!(deleted);
    assert_eq!(root_after, root);

    let root_after_page = storage.read_page(root_after).unwrap();
    assert_eq!(
        clustered_internal::key_at(&root_after_page, 0).unwrap(),
        new_first.as_slice()
    );
    assert!(lookup(
        &storage,
        Some(root_after),
        &old_first,
        &committed_snapshot(10)
    )
    .unwrap()
    .is_none());
}

#[test]
fn delete_physical_frees_overflow_chain_of_removed_row() {
    let mut storage = MemoryStorage::new();
    let key = b"overflow-delete";
    let total_len = clustered_leaf::max_inline_row_bytes(key.len()).unwrap() + 321;
    let payload = vec![0x4E; total_len];
    let root = insert(&mut storage, None, key, &row_header(1), &payload).unwrap();

    let page = storage.read_page(root).unwrap();
    let overflow_first_page = clustered_leaf::read_cell(&page, 0)
        .unwrap()
        .overflow_first_page
        .expect("row must be overflow-backed");

    let (deleted, root_after) = delete_physical(&mut storage, root, key).unwrap();
    assert!(deleted);
    assert_eq!(root_after, root);
    assert!(
        lookup(&storage, Some(root_after), key, &committed_snapshot(10))
            .unwrap()
            .is_none()
    );
    assert!(storage.read_page(overflow_first_page).is_err());
}

#[test]
fn rebalance_leaf_pair_merge_preserves_next_leaf_chain() {
    let mut storage = MemoryStorage::new();
    let left_pid = storage.alloc_page(PageType::ClusteredLeaf).unwrap();
    let right_pid = storage.alloc_page(PageType::ClusteredLeaf).unwrap();
    let tail_pid = storage.alloc_page(PageType::ClusteredLeaf).unwrap();

    let mut left = Page::new(PageType::ClusteredLeaf, left_pid);
    clustered_leaf::init_clustered_leaf(&mut left);
    clustered_leaf::insert_cell(
        &mut left,
        0,
        &1u32.to_be_bytes(),
        &row_header(1),
        &vec![1u8; 3_000],
    )
    .unwrap();
    clustered_leaf::insert_cell(
        &mut left,
        1,
        &2u32.to_be_bytes(),
        &row_header(1),
        &vec![2u8; 3_000],
    )
    .unwrap();
    clustered_leaf::set_next_leaf(&mut left, right_pid);
    write_page(&mut storage, left_pid, &mut left).unwrap();

    let mut right = Page::new(PageType::ClusteredLeaf, right_pid);
    clustered_leaf::init_clustered_leaf(&mut right);
    clustered_leaf::insert_cell(
        &mut right,
        0,
        &3u32.to_be_bytes(),
        &row_header(1),
        &vec![3u8; 3_000],
    )
    .unwrap();
    clustered_leaf::set_next_leaf(&mut right, tail_pid);
    write_page(&mut storage, right_pid, &mut right).unwrap();

    let mut parent = Page::new(PageType::ClusteredInternal, 99);
    clustered_internal::init_clustered_internal(&mut parent, left_pid);
    clustered_internal::insert_at(&mut parent, 0, &3u32.to_be_bytes(), right_pid).unwrap();

    rebalance_leaf_pair(&mut storage, &mut parent, 0, left_pid, right_pid).unwrap();

    assert_eq!(clustered_internal::num_cells(&parent), 0);
    let merged = storage.read_page(left_pid).unwrap();
    assert_eq!(clustered_leaf::num_cells(&merged), 3);
    assert_eq!(clustered_leaf::next_leaf(&merged), tail_pid);

    let keys: Vec<Vec<u8>> = (0..clustered_leaf::num_cells(&merged))
        .map(|idx| {
            clustered_leaf::read_cell(&merged, idx)
                .unwrap()
                .key
                .to_vec()
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            1u32.to_be_bytes().to_vec(),
            2u32.to_be_bytes().to_vec(),
            3u32.to_be_bytes().to_vec(),
        ]
    );
}

#[test]
fn rebalance_internal_pair_redistributes_and_preserves_children() {
    let mut storage = MemoryStorage::new();
    let left_pid = storage.alloc_page(PageType::ClusteredInternal).unwrap();
    let right_pid = storage.alloc_page(PageType::ClusteredInternal).unwrap();

    let make_sep = |byte: u8, child: u64| OwnedInternalCell {
        key: vec![byte; 4_000],
        right_child: child,
    };

    let mut left = Page::new(PageType::ClusteredInternal, left_pid);
    rebuild_internal_page(&mut left, 10, &[make_sep(10, 11), make_sep(20, 12)]).unwrap();
    write_page(&mut storage, left_pid, &mut left).unwrap();

    let mut right = Page::new(PageType::ClusteredInternal, right_pid);
    rebuild_internal_page(&mut right, 20, &[make_sep(40, 21), make_sep(50, 22)]).unwrap();
    write_page(&mut storage, right_pid, &mut right).unwrap();

    let mut parent = Page::new(PageType::ClusteredInternal, 199);
    clustered_internal::init_clustered_internal(&mut parent, left_pid);
    clustered_internal::insert_at(&mut parent, 0, &vec![30u8; 4_000], right_pid).unwrap();

    rebalance_internal_pair(&mut storage, &mut parent, 0, left_pid, right_pid).unwrap();

    assert_eq!(clustered_internal::num_cells(&parent), 1);
    let left_after = storage.read_page(left_pid).unwrap();
    let right_after = storage.read_page(right_pid).unwrap();
    assert_eq!(
        clustered_page_type(&left_after).unwrap(),
        PageType::ClusteredInternal
    );
    assert_eq!(
        clustered_page_type(&right_after).unwrap(),
        PageType::ClusteredInternal
    );

    for page in [&left_after, &right_after] {
        let num = clustered_internal::num_cells(page);
        for idx in 1..num {
            let prev = clustered_internal::key_at(page, idx - 1).unwrap();
            let next = clustered_internal::key_at(page, idx).unwrap();
            assert!(prev < next);
        }
        for child_idx in 0..=num {
            clustered_internal::child_at(page, child_idx).unwrap();
        }
    }

    let total = clustered_internal::num_cells(&left_after)
        + clustered_internal::num_cells(&right_after)
        + 1;
    assert_eq!(total, 5);
}

#[test]
fn delete_physical_collapses_root_after_leaf_merge() {
    let mut storage = MemoryStorage::new();
    let left_pid = storage.alloc_page(PageType::ClusteredLeaf).unwrap();
    let right_pid = storage.alloc_page(PageType::ClusteredLeaf).unwrap();
    let root = storage.alloc_page(PageType::ClusteredInternal).unwrap();

    let mut left = Page::new(PageType::ClusteredLeaf, left_pid);
    clustered_leaf::init_clustered_leaf(&mut left);
    clustered_leaf::insert_cell(
        &mut left,
        0,
        &0u32.to_be_bytes(),
        &row_header(1),
        &vec![0u8; 3_000],
    )
    .unwrap();
    clustered_leaf::insert_cell(
        &mut left,
        1,
        &1u32.to_be_bytes(),
        &row_header(1),
        &vec![1u8; 3_000],
    )
    .unwrap();
    clustered_leaf::set_next_leaf(&mut left, right_pid);
    write_page(&mut storage, left_pid, &mut left).unwrap();

    let mut right = Page::new(PageType::ClusteredLeaf, right_pid);
    clustered_leaf::init_clustered_leaf(&mut right);
    clustered_leaf::insert_cell(
        &mut right,
        0,
        &2u32.to_be_bytes(),
        &row_header(1),
        &vec![2u8; 3_000],
    )
    .unwrap();
    clustered_leaf::insert_cell(
        &mut right,
        1,
        &3u32.to_be_bytes(),
        &row_header(1),
        &vec![3u8; 3_000],
    )
    .unwrap();
    write_page(&mut storage, right_pid, &mut right).unwrap();

    let mut root_page = Page::new(PageType::ClusteredInternal, root);
    clustered_internal::init_clustered_internal(&mut root_page, left_pid);
    clustered_internal::insert_at(&mut root_page, 0, &2u32.to_be_bytes(), right_pid).unwrap();
    write_page(&mut storage, root, &mut root_page).unwrap();

    let (deleted, root_after) = delete_physical(&mut storage, root, &0u32.to_be_bytes()).unwrap();
    assert!(deleted);
    assert_ne!(root_after, root);

    let new_root_page = storage.read_page(root_after).unwrap();
    assert_eq!(
        clustered_page_type(&new_root_page).unwrap(),
        PageType::ClusteredLeaf
    );
    let keys = collect_leaf_chain_keys(&storage, root_after).unwrap();
    assert_eq!(
        keys,
        vec![
            1u32.to_be_bytes().to_vec(),
            2u32.to_be_bytes().to_vec(),
            3u32.to_be_bytes().to_vec(),
        ]
    );
}

#[test]
fn update_with_relocation_reinserts_row_after_same_leaf_failure() {
    let mut storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..7 {
        root = Some(
            insert(
                &mut storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; 2_100],
            )
            .unwrap(),
        );
    }

    let root = root.unwrap();
    let before_pids = collect_leaf_chain_pids(&storage, root).unwrap();

    let root_after = update_with_relocation(
        &mut storage,
        Some(root),
        &3u32.to_be_bytes(),
        &vec![9u8; 8_000],
        10,
        &active_snapshot(10, 1),
    )
    .unwrap()
    .expect("row must be updated via relocation");

    let row = lookup(
        &storage,
        Some(root_after),
        &3u32.to_be_bytes(),
        &active_snapshot(10, 1),
    )
    .unwrap()
    .expect("relocated row must be visible");
    assert_eq!(row.row_data, vec![9u8; 8_000]);
    assert_eq!(row.row_header.txn_id_created, 10);
    assert_eq!(row.row_header.row_version, 1);

    let old_snapshot = lookup(
        &storage,
        Some(root_after),
        &3u32.to_be_bytes(),
        &committed_snapshot(1),
    )
    .unwrap();
    assert!(
        old_snapshot.is_none(),
        "39.8 relocation still does not reconstruct older visible versions"
    );

    let keys = collect_leaf_chain_keys(&storage, root_after).unwrap();
    let expected: Vec<Vec<u8>> = (0u32..7).map(|key| key.to_be_bytes().to_vec()).collect();
    assert_eq!(keys, expected);

    let after_pids = collect_leaf_chain_pids(&storage, root_after).unwrap();
    assert!(
        after_pids.len() >= before_pids.len().saturating_sub(1),
        "relocation may rebalance the source path but must keep a valid leaf chain"
    );
}

/// Phase 40.8c: a delete that flows through the early-X-latch-release branch
/// of `delete_physical_subtree`. The leaves are kept comfortably above the
/// underfull byte threshold so the safe-descent predicate engages, and we
/// then verify the result is identical to the pessimistic path.
#[test]
fn delete_physical_through_safe_descent_keeps_tree_consistent() {
    let mut storage = MemoryStorage::new();
    let mut root = None;

    let count = 128u32;
    let payload_len = 800usize;
    for key in 0..count {
        root = Some(
            insert(
                &mut storage,
                root,
                &key.to_be_bytes(),
                &row_header(1),
                &vec![key as u8; payload_len],
            )
            .unwrap(),
        );
    }
    let root = root.unwrap();
    assert_eq!(
        clustered_page_type(&storage.read_page(root).unwrap()).unwrap(),
        PageType::ClusteredInternal,
        "test requires an internal root so the descent has at least one early-release opportunity"
    );

    // Delete a key well inside a populated leaf — the leaf still has many
    // hundreds of bytes used after the removal, so it stays well above the
    // underfull threshold. This makes the descent's safe-child predicate
    // hold and exercises the early-release branch.
    let target_key = (count / 2).to_be_bytes();
    let (deleted, root_after) = delete_physical(&mut storage, root, &target_key).unwrap();
    assert!(deleted, "target key must exist before delete");
    assert_eq!(root_after, root, "non-collapsing delete must keep the root pid stable");

    // Every other key still reachable, in order.
    let keys = collect_leaf_chain_keys(&storage, root_after).unwrap();
    let expected: Vec<Vec<u8>> = (0u32..count)
        .filter(|k| *k != count / 2)
        .map(|k| k.to_be_bytes().to_vec())
        .collect();
    assert_eq!(keys, expected);

    let snap = committed_snapshot(10);
    assert!(
        lookup(&storage, Some(root_after), &target_key, &snap)
            .unwrap()
            .is_none(),
        "deleted key must not be reachable"
    );
    for k in [0u32, count / 4, count - 1] {
        let row = lookup(&storage, Some(root_after), &k.to_be_bytes(), &snap)
            .unwrap()
            .unwrap_or_else(|| panic!("key {k} should still be reachable"));
        assert_eq!(row.row_data.len(), payload_len);
    }
}

