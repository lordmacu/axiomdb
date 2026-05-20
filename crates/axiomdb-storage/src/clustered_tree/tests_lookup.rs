#[test]
fn lookup_empty_tree_returns_none() {
    let storage = MemoryStorage::new();
    let got = lookup(&storage, None, b"missing", &committed_snapshot(100)).unwrap();
    assert!(got.is_none());
}

#[test]
fn lookup_root_leaf_hit_returns_inline_row() {
    let storage = MemoryStorage::new();
    let root = insert(&storage, None, b"pk-1", &row_header(3), b"row-inline").unwrap();

    let got = lookup(&storage, Some(root), b"pk-1", &committed_snapshot(10))
        .unwrap()
        .expect("row must exist");

    assert_eq!(got.key, b"pk-1");
    assert_eq!(got.row_data, b"row-inline");
    assert_eq!(got.row_header.txn_id_created, 3);
}

#[test]
fn lookup_missing_key_returns_none() {
    let storage = MemoryStorage::new();
    let mut root = None;
    for key in [b"alpha".as_slice(), b"bravo", b"charlie"] {
        root = Some(insert(&storage, root, key, &row_header(1), b"row").unwrap());
    }

    let got = lookup(&storage, root, b"delta", &committed_snapshot(10)).unwrap();
    assert!(got.is_none());
}

#[test]
fn lookup_invisible_current_version_returns_none() {
    let storage = MemoryStorage::new();
    let root = insert(
        &storage,
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
    let storage = MemoryStorage::new();
    let mut root = None;

    for key in 0u32..128 {
        root = Some(
            insert(
                &storage,
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

// ── Attack 5 / Step 5.1: lookup_with_hint + LeafCursorHint ────────────────

const TEST_TABLE_ID: u32 = 42;
const TEST_SCHEMA_VERSION: u64 = 1;

fn build_small_tree(keys: &[u32]) -> (MemoryStorage, u64) {
    let storage = MemoryStorage::new();
    let mut root = None;
    for &k in keys {
        root = Some(
            insert(
                &storage,
                root,
                &k.to_be_bytes(),
                &row_header(1),
                &row_bytes(k, 16),
            )
            .unwrap(),
        );
    }
    (storage, root.unwrap())
}

#[test]
fn lookup_with_hint_misses_when_empty_then_populates() {
    let (storage, root) = build_small_tree(&[1, 5, 10]);
    let mut hint: Option<LeafCursorHint> = None;

    let row = lookup_with_hint(
        &storage,
        Some(root),
        &5u32.to_be_bytes(),
        TEST_TABLE_ID,
        TEST_SCHEMA_VERSION,
        &mut hint,
    )
    .unwrap();
    assert!(row.is_some());

    let h = hint.as_ref().expect("hint populated after descent");
    assert_eq!(h.table_id, TEST_TABLE_ID);
    assert_eq!(h.root_page_id, root);
    assert_eq!(h.schema_version, TEST_SCHEMA_VERSION);
    assert_eq!(h.min_key, 1u32.to_be_bytes());
    assert_eq!(h.max_key, 10u32.to_be_bytes());
}

#[test]
fn lookup_with_hint_hits_when_key_in_range() {
    let (storage, root) = build_small_tree(&[1, 5, 10, 15, 20]);
    let mut hint: Option<LeafCursorHint> = None;

    // First call — populates hint.
    let _ = lookup_with_hint(
        &storage,
        Some(root),
        &10u32.to_be_bytes(),
        TEST_TABLE_ID,
        TEST_SCHEMA_VERSION,
        &mut hint,
    )
    .unwrap();
    let leaf_after_first = hint.as_ref().unwrap().leaf_page_id;

    // Second call — same leaf range. MUST reuse (hint.leaf_page_id stays).
    let _ = lookup_with_hint(
        &storage,
        Some(root),
        &15u32.to_be_bytes(),
        TEST_TABLE_ID,
        TEST_SCHEMA_VERSION,
        &mut hint,
    )
    .unwrap();
    assert_eq!(
        hint.as_ref().unwrap().leaf_page_id,
        leaf_after_first,
        "same leaf reused on second lookup with key in range"
    );
}

#[test]
fn lookup_with_hint_descends_on_key_out_of_range() {
    // Build a tree large enough to span multiple leaves (cells ~16 bytes
    // payload + key, so ~hundreds per leaf). Use enough keys to force a
    // split.
    let mut keys: Vec<u32> = (0..2000).collect();
    keys.sort_unstable();
    let (storage, root) = build_small_tree(&keys);
    let mut hint: Option<LeafCursorHint> = None;

    // First lookup — populates hint with whatever leaf holds key 10.
    let _ = lookup_with_hint(
        &storage,
        Some(root),
        &10u32.to_be_bytes(),
        TEST_TABLE_ID,
        TEST_SCHEMA_VERSION,
        &mut hint,
    )
    .unwrap();
    let leaf_for_low = hint.as_ref().unwrap().leaf_page_id;

    // Second lookup at the far end — different leaf, hint must update.
    let _ = lookup_with_hint(
        &storage,
        Some(root),
        &1990u32.to_be_bytes(),
        TEST_TABLE_ID,
        TEST_SCHEMA_VERSION,
        &mut hint,
    )
    .unwrap();
    assert_ne!(
        hint.as_ref().unwrap().leaf_page_id,
        leaf_for_low,
        "different leaf after out-of-range key descent"
    );
}

#[test]
fn lookup_with_hint_invalidates_on_root_mismatch() {
    let (storage, root) = build_small_tree(&[1, 5, 10]);
    // Pre-seed a stale hint pointing at a bogus root.
    let mut hint = Some(LeafCursorHint {
        table_id: TEST_TABLE_ID,
        root_page_id: 9999, // wrong
        leaf_page_id: 7,
        min_key: vec![0],
        max_key: vec![255],
        schema_version: TEST_SCHEMA_VERSION,
    });

    let _ = lookup_with_hint(
        &storage,
        Some(root),
        &5u32.to_be_bytes(),
        TEST_TABLE_ID,
        TEST_SCHEMA_VERSION,
        &mut hint,
    )
    .unwrap();

    assert_eq!(
        hint.as_ref().unwrap().root_page_id,
        root,
        "stale root_page_id replaced with the real root after descent"
    );
}

