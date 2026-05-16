mod common;

use axiomdb_catalog::CatalogReader;
use axiomdb_core::DbError;
use axiomdb_types::Value;

// ── CREATE TYPE AS (…) ───────────────────────────────────────────────────────

#[test]
fn test_create_composite_type_persists_catalog_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let def = reader
        .get_composite_type("public", "address")
        .unwrap()
        .unwrap();
    assert_eq!(def.schema_name, "public");
    assert_eq!(def.name, "address");
    assert_eq!(def.fields.len(), 2);
    assert_eq!(def.fields[0].name, "city");
    assert_eq!(def.fields[0].type_name.to_ascii_uppercase(), "TEXT");
    assert_eq!(def.fields[1].name, "zip");
    assert_eq!(def.fields[1].type_name.to_ascii_uppercase(), "INT");
}

#[test]
fn test_create_composite_type_rejects_empty_field_list() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    let err = common::run_ctx(
        "CREATE TYPE empty_type AS ()",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(
        matches!(
            err,
            DbError::ParseError { .. } | DbError::InvalidValue { .. }
        ),
        "expected ParseError or InvalidValue, got {err:?}"
    );
}

#[test]
fn test_create_composite_type_duplicate_fails() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE point AS (x INT, y INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "CREATE TYPE point AS (a TEXT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
}

// ── DROP TYPE ────────────────────────────────────────────────────────────────

#[test]
fn test_drop_composite_type_removes_catalog_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "DROP TYPE address",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    assert!(reader
        .get_composite_type("public", "address")
        .unwrap()
        .is_none());
}

#[test]
fn test_drop_type_if_exists_succeeds_for_nonexistent() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "DROP TYPE IF EXISTS nonexistent_type",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
}

#[test]
fn test_drop_type_without_if_exists_fails_for_nonexistent() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    let err = common::run_ctx(
        "DROP TYPE nonexistent_type",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
}

#[test]
fn test_drop_type_also_drops_enum_type() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "DROP TYPE mood",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    assert!(reader.get_enum_type("public", "mood").unwrap().is_none());
}

// ── CREATE TABLE with composite column ───────────────────────────────────────

#[test]
fn test_create_table_with_composite_column() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE orders (id INT, home address)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader
        .get_table_in_database("axiomdb", "public", "orders")
        .unwrap()
        .unwrap();
    let cols = reader.list_columns(table.id).unwrap();
    let home_col = cols.iter().find(|c| c.name == "home").unwrap();
    assert_eq!(home_col.col_type, axiomdb_catalog::ColumnType::Composite);
    assert_eq!(home_col.enum_type_name.as_deref(), Some("public.address"));
}

#[test]
fn test_create_table_with_undefined_composite_type_fails() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    let err = common::run_ctx(
        "CREATE TABLE t (id INT, col undefined_type)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }), "got {err:?}");
}

// ── INSERT + SELECT with ROW constructor ─────────────────────────────────────

#[test]
fn test_insert_and_select_composite_value() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE orders (id INT, home address)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "INSERT INTO orders VALUES (1, ROW('NYC', 10001))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT id, home FROM orders",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = common::rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(1));
    assert!(
        matches!(&rows[0][1], Value::Composite(fields) if fields.len() == 2),
        "expected Composite(2 fields), got {:?}",
        rows[0][1]
    );
}

#[test]
fn test_insert_multiple_composite_rows() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE orders (id INT, home address)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "INSERT INTO orders VALUES (1, ROW('NYC', 10001)), (2, ROW('LA', 90001))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT COUNT(*) FROM orders",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = common::rows(result);
    assert_eq!(rows[0][0], Value::BigInt(2));
}

// ── Dot-notation field access ─────────────────────────────────────────────────

#[test]
fn test_field_access_dot_notation() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE orders (id INT, home address)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "INSERT INTO orders VALUES (1, ROW('NYC', 10001)), (2, ROW('LA', 90001))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT id, home.city FROM orders ORDER BY id",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = common::rows(result);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Value::Text("NYC".into()));
    assert_eq!(rows[1][1], Value::Text("LA".into()));
}

#[test]
fn test_field_access_second_field() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE orders (id INT, home address)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "INSERT INTO orders VALUES (1, ROW('NYC', 10001))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT home.zip FROM orders",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = common::rows(result);
    assert_eq!(rows[0][0], Value::Int(10001));
}

#[test]
fn test_field_access_where_clause() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE orders (id INT, home address)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "INSERT INTO orders VALUES (1, ROW('NYC', 10001)), (2, ROW('LA', 90001))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT id FROM orders WHERE home.city = 'NYC'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = common::rows(result);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(1));
}

#[test]
fn test_field_access_null_composite_returns_null() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE orders (id INT, home address)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "INSERT INTO orders VALUES (1, NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let result = common::run_ctx(
        "SELECT home.city FROM orders",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let rows = common::rows(result);
    assert_eq!(rows[0][0], Value::Null);
}

// ── Round-trip: persistence across reopen ────────────────────────────────────

#[test]
fn test_composite_type_survives_reopen() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE address AS (city TEXT, zip INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Verify the type is visible via a fresh CatalogReader (simulates re-read after commit).
    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let def = reader
        .get_composite_type("public", "address")
        .unwrap()
        .unwrap();
    assert_eq!(def.fields.len(), 2);
}
