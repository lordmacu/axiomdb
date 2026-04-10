//! Integration tests for Phase 11.4 native JSON.

mod common;

use axiomdb_catalog::{CatalogReader, ColumnType};
use axiomdb_core::error::DbError;
use axiomdb_types::Value;

fn scalar(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
) -> Value {
    let rows = common::rows(common::run(sql, storage, txn));
    rows.into_iter().next().unwrap().into_iter().next().unwrap()
}

#[test]
fn test_create_table_json_catalog_type() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSON)",
        &mut storage,
        &mut txn,
    );

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "docs").unwrap().unwrap();
    let columns = reader.list_columns(table.id).unwrap();
    let data = columns.iter().find(|c| c.name == "data").unwrap();
    assert_eq!(data.col_type, ColumnType::Json);
}

#[test]
fn test_json_insert_validates_syntax() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSON)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"name":"Alice","age":30}')"#,
        &mut storage,
        &mut txn,
    );

    let err = common::run_result(
        "INSERT INTO docs VALUES (2, '{bad')",
        &mut storage,
        &mut txn,
    )
    .unwrap_err();
    assert!(
        matches!(err, DbError::InvalidValue { .. }),
        "expected InvalidValue for invalid JSON, got: {err}"
    );
}

#[test]
fn test_json_extract_returns_sql_scalars() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSON)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"active":true,"age":30,"name":"Alice","nested":{"score":9.5},"none":null}')"#,
        &mut storage,
        &mut txn,
    );

    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.name') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Text("Alice".into())
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.age') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Int(30)
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.active') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.nested.score') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Real(9.5)
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.missing') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Null
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.none') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Null
    );
}

#[test]
fn test_json_extract_text_operator_in_select_and_where() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSON)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"name":"Alice","kind":"invoice"}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (2, '{"name":"Bob","kind":"note"}')"#,
        &mut storage,
        &mut txn,
    );

    assert_eq!(
        scalar(
            "SELECT data->>'name' FROM docs WHERE data->>'kind' = 'invoice'",
            &mut storage,
            &mut txn,
        ),
        Value::Text("Alice".into())
    );
}

#[test]
fn test_json_mutation_and_metadata_functions() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSON)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"age":30,"name":"Alice"}')"#,
        &mut storage,
        &mut txn,
    );

    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(JSON_SET(data, '$.active', TRUE), '$.active') FROM docs",
            &mut storage,
            &mut txn,
        ),
        Value::Bool(true)
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(JSON_REMOVE(data, '$.age'), '$.age') FROM docs",
            &mut storage,
            &mut txn,
        ),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_TYPE(data) FROM docs", &mut storage, &mut txn),
        Value::Text("OBJECT".into())
    );
    assert_eq!(
        scalar("SELECT JSON_VALID(data) FROM docs", &mut storage, &mut txn),
        Value::Int(1)
    );
    assert_eq!(
        scalar(
            "SELECT JSON_TYPE(JSON_KEYS(data)) FROM docs",
            &mut storage,
            &mut txn,
        ),
        Value::Text("ARRAY".into())
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(JSON_SET(data, '$', 7), '$') FROM docs",
            &mut storage,
            &mut txn,
        ),
        Value::Int(7)
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(JSON_REMOVE(data, '$'), '$') FROM docs",
            &mut storage,
            &mut txn,
        ),
        Value::Null
    );
}

#[test]
fn test_json_null_semantics() {
    let (mut storage, mut txn) = common::setup();
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(NULL, '$.name')",
            &mut storage,
            &mut txn
        ),
        Value::Null
    );
    assert_eq!(
        scalar(
            "SELECT JSON_SET(NULL, '$.name', 'Alice')",
            &mut storage,
            &mut txn
        ),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_REMOVE(NULL, '$.name')", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_KEYS(NULL)", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_TYPE(NULL)", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_VALID(NULL)", &mut storage, &mut txn),
        Value::Int(0)
    );
}
