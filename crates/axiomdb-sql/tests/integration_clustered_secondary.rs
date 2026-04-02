use std::sync::atomic::{AtomicU64, Ordering};

use axiomdb_catalog::{IndexColumnDef, IndexDef, SortOrder};
use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
use axiomdb_sql::clustered_secondary::{ClusteredSecondaryLayout, ClusteredSecondaryUpdateOutcome};
use axiomdb_storage::{MemoryStorage, Page, PageType, StorageEngine};
use axiomdb_types::Value;

fn idx_col(col_idx: u16) -> IndexColumnDef {
    IndexColumnDef {
        col_idx,
        order: SortOrder::Asc,
    }
}

fn primary_idx(cols: &[u16]) -> IndexDef {
    IndexDef {
        index_id: 1,
        table_id: 42,
        name: "events_pkey".into(),
        root_page_id: 10,
        is_unique: true,
        is_primary: true,
        columns: cols.iter().copied().map(idx_col).collect(),
        predicate: None,
        fillfactor: 90,
        is_fk_index: false,
        include_columns: vec![],
        index_type: 0,
        pages_per_range: 128,
    }
}

fn secondary_idx(name: &str, unique: bool, cols: &[u16]) -> IndexDef {
    IndexDef {
        index_id: 2,
        table_id: 42,
        name: name.into(),
        root_page_id: 11,
        is_unique: unique,
        is_primary: false,
        columns: cols.iter().copied().map(idx_col).collect(),
        predicate: None,
        fillfactor: 90,
        is_fk_index: false,
        include_columns: vec![],
        index_type: 0,
        pages_per_range: 128,
    }
}

fn alloc_index_root(storage: &mut MemoryStorage) -> u64 {
    let root = storage.alloc_page(PageType::Index).unwrap();
    let mut page = Page::new(PageType::Index, root);
    let leaf = cast_leaf_mut(&mut page);
    leaf.is_leaf = 1;
    leaf.set_num_keys(0);
    leaf.set_next_leaf(NULL_PAGE);
    page.update_checksum();
    storage.write_page(root, &page).unwrap();
    root
}

#[test]
fn clustered_secondary_scan_returns_primary_key_bookmarks_for_duplicate_logical_keys() {
    let mut storage = MemoryStorage::new();
    let root = AtomicU64::new(alloc_index_root(&mut storage));
    let layout = ClusteredSecondaryLayout::derive(
        &secondary_idx("idx_status", false, &[2]),
        &primary_idx(&[0, 1]),
    )
    .unwrap();

    let rows = vec![
        vec![Value::Int(10), Value::Int(1), Value::Text("open".into())],
        vec![Value::Int(10), Value::Int(2), Value::Text("open".into())],
        vec![Value::Int(11), Value::Int(1), Value::Text("open".into())],
        vec![Value::Int(12), Value::Int(1), Value::Text("closed".into())],
    ];

    for row in &rows {
        assert!(layout.insert_row(&mut storage, &root, row).unwrap());
    }

    let open_entries = layout
        .scan_prefix(
            &storage,
            root.load(Ordering::Acquire),
            &[Value::Text("open".into())],
        )
        .unwrap();

    let open_bookmarks: Vec<Vec<Value>> = open_entries.into_iter().map(|e| e.primary_key).collect();
    assert_eq!(
        open_bookmarks,
        vec![
            vec![Value::Int(10), Value::Int(1)],
            vec![Value::Int(10), Value::Int(2)],
            vec![Value::Int(11), Value::Int(1)],
        ]
    );
}

#[test]
fn clustered_secondary_delete_and_relocation_update_preserve_bookmark_semantics() {
    let mut storage = MemoryStorage::new();
    let root = AtomicU64::new(alloc_index_root(&mut storage));
    let layout = ClusteredSecondaryLayout::derive(
        &secondary_idx("idx_status", false, &[2]),
        &primary_idx(&[0, 1]),
    )
    .unwrap();

    let row_a = vec![Value::Int(20), Value::Int(1), Value::Text("open".into())];
    let row_b = vec![Value::Int(20), Value::Int(2), Value::Text("open".into())];

    assert!(layout.insert_row(&mut storage, &root, &row_a).unwrap());
    assert!(layout.insert_row(&mut storage, &root, &row_b).unwrap());

    assert!(layout.delete_row(&mut storage, &root, &row_a).unwrap());

    let remaining = layout
        .scan_prefix(
            &storage,
            root.load(Ordering::Acquire),
            &[Value::Text("open".into())],
        )
        .unwrap();
    assert_eq!(remaining.len(), 1);
    assert_eq!(
        remaining[0].primary_key,
        vec![Value::Int(20), Value::Int(2)]
    );

    let relocated_row_b = vec![
        Value::Int(20),
        Value::Int(2),
        Value::Text("open".into()),
        Value::Text("payload moved".into()),
    ];

    assert_eq!(
        layout
            .update_row(&mut storage, &root, &row_b, &relocated_row_b)
            .unwrap(),
        ClusteredSecondaryUpdateOutcome::Unchanged
    );

    let after_reloc = layout
        .scan_prefix(
            &storage,
            root.load(Ordering::Acquire),
            &[Value::Text("open".into())],
        )
        .unwrap();
    assert_eq!(after_reloc.len(), 1);
    assert_eq!(
        after_reloc[0].primary_key,
        vec![Value::Int(20), Value::Int(2)]
    );
}
