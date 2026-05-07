mod common;

use axiomdb_catalog::{schema::DEFAULT_DATABASE_NAME, CatalogReader};
use axiomdb_core::DbError;
use axiomdb_types::DataType;

#[test]
fn test_create_type_as_enum_persists_catalog_row() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let enum_type = reader.get_enum_type("public", "mood").unwrap().unwrap();
    assert_eq!(enum_type.labels, vec!["sad", "ok", "happy"]);
}

#[test]
fn test_create_table_enum_column_persists_declared_type_name() {
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
        "CREATE TABLE tasks (state mood NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let mut reader = CatalogReader::new(&storage, txn.snapshot()).unwrap();
    let table = reader
        .get_table_in_database(DEFAULT_DATABASE_NAME, "public", "tasks")
        .unwrap()
        .unwrap();
    let columns = reader.list_columns(table.id).unwrap();
    assert_eq!(
        columns[0].col_type,
        axiomdb_catalog::schema::ColumnType::Text
    );
    assert_eq!(columns[0].enum_type_name.as_deref(), Some("public.mood"));
}

#[test]
fn test_create_table_enum_column_requires_existing_type() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();

    let err = common::run_ctx(
        "CREATE TABLE tasks (state mood NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn test_insert_rejects_label_not_in_enum() {
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
        "CREATE TABLE tasks (state mood NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "INSERT INTO tasks VALUES ('ok')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let err = common::run_ctx(
        "INSERT INTO tasks VALUES ('angry')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn test_update_rejects_label_not_in_enum() {
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
        "CREATE TABLE tasks (state mood NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO tasks VALUES ('sad')",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let err = common::run_ctx(
        "UPDATE tasks SET state = 'angry'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { .. }));
}

#[test]
fn test_show_metadata_uses_declared_enum_type() {
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
        "CREATE TABLE tasks (state mood NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let show_columns = common::run_ctx(
        "SHOW COLUMNS FROM tasks",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let rows = common::rows(show_columns);
    assert_eq!(rows[0][1], axiomdb_types::Value::Text("public.mood".into()));

    let show_create = common::run_ctx(
        "SHOW CREATE TABLE tasks",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let rows = common::rows(show_create);
    match &rows[0][1] {
        axiomdb_types::Value::Text(sql) => assert!(sql.contains("`state` public.mood NOT NULL")),
        other => panic!("expected SHOW CREATE SQL text, got {other:?}"),
    }
}

#[test]
fn test_builtin_text_column_does_not_become_enum() {
    let ct = match axiomdb_sql::parse("CREATE TABLE tasks (state TEXT)", None).unwrap() {
        axiomdb_sql::ast::Stmt::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    };
    assert_eq!(ct.columns[0].data_type, DataType::Text);
    assert!(ct.columns[0].declared_type_name.is_none());
}
