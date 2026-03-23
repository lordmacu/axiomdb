//! Integration tests for SchemaResolver (subphase 3.14).

use axiomdb_catalog::{
    bootstrap::CatalogBootstrap,
    resolver::SchemaResolver,
    schema::{ColumnDef, ColumnType, IndexDef},
    writer::CatalogWriter,
};
use axiomdb_core::DbError;
use axiomdb_storage::MemoryStorage;
use axiomdb_wal::TxnManager;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("test.wal");
    let txn = TxnManager::create(&wal_path).unwrap();
    std::mem::forget(dir);
    (storage, txn)
}

// ── resolve_table ─────────────────────────────────────────────────────────────

#[test]
fn test_resolve_table_by_default_schema() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "users").unwrap()
    };
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();
    let resolved = resolver.resolve_table(None, "users").unwrap();

    assert_eq!(resolved.def.id, table_id);
    assert_eq!(resolved.def.schema_name, "public");
    assert_eq!(resolved.def.table_name, "users");
}

#[test]
fn test_resolve_table_by_explicit_schema() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("analytics", "events").unwrap();
    }
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();
    let resolved = resolver.resolve_table(Some("analytics"), "events").unwrap();
    assert_eq!(resolved.def.table_name, "events");
    assert_eq!(resolved.def.schema_name, "analytics");
}

#[test]
fn test_resolve_table_not_found_returns_table_not_found_error() {
    let (storage, txn) = setup();
    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();

    let err = resolver.resolve_table(None, "ghost").unwrap_err();
    assert!(
        matches!(err, DbError::TableNotFound { ref name } if name.contains("ghost")),
        "expected TableNotFound with 'ghost' in name, got: {err}"
    );
}

#[test]
fn test_resolve_table_not_found_error_message_is_qualified() {
    let (storage, txn) = setup();
    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "myschema").unwrap();

    let err = resolver.resolve_table(None, "t").unwrap_err();
    // Error message must contain the qualified name "myschema.t".
    let msg = err.to_string();
    assert!(
        msg.contains("myschema") && msg.contains("t"),
        "qualified name missing from error: {msg}"
    );
}

#[test]
fn test_resolve_table_columns_sorted_by_col_idx() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "users").unwrap();
        // Insert columns in reverse order.
        for (idx, name) in [(2u16, "email"), (0u16, "id"), (1u16, "username")] {
            w.create_column(ColumnDef {
                table_id: tid,
                col_idx: idx,
                name: name.to_string(),
                col_type: ColumnType::Text,
                nullable: idx != 0,
            })
            .unwrap();
        }
        tid
    };
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();
    let resolved = resolver.resolve_table(None, "users").unwrap();

    assert_eq!(resolved.columns.len(), 3);
    assert_eq!(resolved.columns[0].col_idx, 0);
    assert_eq!(resolved.columns[0].name, "id");
    assert_eq!(resolved.columns[1].col_idx, 1);
    assert_eq!(resolved.columns[1].name, "username");
    assert_eq!(resolved.columns[2].col_idx, 2);
    assert_eq!(resolved.columns[2].name, "email");
    let _ = table_id;
}

#[test]
fn test_resolve_table_includes_indexes() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "orders").unwrap();
        w.create_index(IndexDef {
            index_id: 0,
            table_id: tid,
            name: "orders_pkey".into(),
            root_page_id: 10,
            is_unique: true,
            is_primary: true,
        })
        .unwrap();
        w.create_index(IndexDef {
            index_id: 0,
            table_id: tid,
            name: "orders_user_idx".into(),
            root_page_id: 11,
            is_unique: false,
            is_primary: false,
        })
        .unwrap();
        tid
    };
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();
    let resolved = resolver.resolve_table(None, "orders").unwrap();

    assert_eq!(resolved.def.id, table_id);
    assert_eq!(resolved.indexes.len(), 2);
    let names: Vec<&str> = resolved.indexes.iter().map(|i| i.name.as_str()).collect();
    assert!(names.contains(&"orders_pkey"));
    assert!(names.contains(&"orders_user_idx"));
}

// ── resolve_column ────────────────────────────────────────────────────────────

#[test]
fn test_resolve_column_found() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "users").unwrap();
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 0,
            name: "id".into(),
            col_type: ColumnType::BigInt,
            nullable: false,
        })
        .unwrap();
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 1,
            name: "email".into(),
            col_type: ColumnType::Text,
            nullable: true,
        })
        .unwrap();
        tid
    };
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();

    let col = resolver.resolve_column(table_id, "email").unwrap();
    assert_eq!(col.col_idx, 1);
    assert_eq!(col.col_type, ColumnType::Text);
    assert!(col.nullable);
}

#[test]
fn test_resolve_column_not_found_returns_column_not_found_error() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    let table_id = {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        let tid = w.create_table("public", "users").unwrap();
        w.create_column(ColumnDef {
            table_id: tid,
            col_idx: 0,
            name: "id".into(),
            col_type: ColumnType::BigInt,
            nullable: false,
        })
        .unwrap();
        tid
    };
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();

    let err = resolver
        .resolve_column(table_id, "nonexistent")
        .unwrap_err();
    assert!(
        matches!(err, DbError::ColumnNotFound { ref name, .. } if name == "nonexistent"),
        "expected ColumnNotFound, got: {err}"
    );
    // Error message must include the table name for context.
    let msg = err.to_string();
    assert!(
        msg.contains("users"),
        "table name missing from ColumnNotFound error: {msg}"
    );
}

// ── table_exists ──────────────────────────────────────────────────────────────

#[test]
fn test_table_exists_returns_true_for_committed_table() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "products").unwrap();
    }
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();
    assert!(resolver.table_exists(None, "products").unwrap());
}

#[test]
fn test_table_exists_returns_false_for_nonexistent_table() {
    let (storage, txn) = setup();
    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();
    assert!(!resolver.table_exists(None, "ghost").unwrap());
}

#[test]
fn test_table_exists_respects_schema() {
    let (mut storage, mut txn) = setup();

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("analytics", "events").unwrap();
    }
    txn.commit().unwrap();

    let snap = txn.snapshot();
    let resolver = SchemaResolver::new(&storage, snap, "public").unwrap();

    // Exists in "analytics", not in "public".
    assert!(resolver.table_exists(Some("analytics"), "events").unwrap());
    assert!(!resolver.table_exists(None, "events").unwrap()); // default = "public"
}

// ── MVCC isolation ────────────────────────────────────────────────────────────

#[test]
fn test_mvcc_resolver_does_not_see_uncommitted_table() {
    let (mut storage, mut txn) = setup();

    // Snapshot taken before the table is created.
    let snap_before = txn.snapshot();

    txn.begin().unwrap();
    {
        let mut w = CatalogWriter::new(&mut storage, &mut txn).unwrap();
        w.create_table("public", "invisible").unwrap();
    }
    // Not committed yet — old snapshot must not see it.
    let resolver_old = SchemaResolver::new(&storage, snap_before, "public").unwrap();
    assert!(!resolver_old.table_exists(None, "invisible").unwrap());

    txn.commit().unwrap();

    // New snapshot after commit sees it.
    let snap_after = txn.snapshot();
    let resolver_new = SchemaResolver::new(&storage, snap_after, "public").unwrap();
    assert!(resolver_new.table_exists(None, "invisible").unwrap());
}

// ── Catalog not initialized ───────────────────────────────────────────────────

#[test]
fn test_catalog_not_initialized_returns_error() {
    let storage = MemoryStorage::new(); // no CatalogBootstrap::init
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.path().join("test.wal");
    let txn = TxnManager::create(&wal_path).unwrap();
    let snap = txn.snapshot();

    let err = SchemaResolver::new(&storage, snap, "public")
        .err()
        .expect("expected an error");
    assert!(
        matches!(err, DbError::CatalogNotInitialized),
        "expected CatalogNotInitialized, got: {err}"
    );
}
