//! Integration tests for Phase 11.16 — Binary JSONB + JSONPath + new JSON functions.

mod common;

use axiomdb_catalog::{CatalogReader, ColumnType};
use axiomdb_types::Value;

fn scalar(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
) -> Value {
    let rows = common::rows(common::run(sql, storage, txn));
    rows.into_iter().next().unwrap().into_iter().next().unwrap()
}

fn rows(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
) -> Vec<Vec<Value>> {
    common::rows(common::run(sql, storage, txn))
}

// ── JSONB column type ────────────────────────────────────────────────────────

#[test]
fn test_create_table_jsonb_column() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSONB)",
        &mut storage,
        &mut txn,
    );

    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "docs").unwrap().unwrap();
    let columns = reader.list_columns(table.id).unwrap();
    let data = columns.iter().find(|c| c.name == "data").unwrap();
    assert_eq!(data.col_type, ColumnType::Jsonb);
}

#[test]
fn test_jsonb_insert_and_select() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"name":"Alice","age":30}')"#,
        &mut storage,
        &mut txn,
    );
    let result = rows(
        "SELECT id, data FROM docs WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
    // data is stored as JSONB (binary), display should be valid JSON
    match &result[0][1] {
        Value::Jsonb(_) => {} // expected
        other => panic!("expected Jsonb, got {other:?}"),
    }
}

// ── -> operator ──────────────────────────────────────────────────────────────

#[test]
fn test_json_sub_operator_on_jsonb() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"name":"Alice","tags":["a","b"],"nested":{"x":42}}')"#,
        &mut storage,
        &mut txn,
    );

    // -> with string key on object returns the value
    let v = scalar("SELECT data->'name' FROM docs", &mut storage, &mut txn);
    assert_eq!(v, Value::Text("Alice".into()));

    // -> with string key on nested container returns Jsonb
    let v = scalar("SELECT data->'nested' FROM docs", &mut storage, &mut txn);
    match v {
        Value::Jsonb(_) => {} // container returned as JSONB
        other => panic!("expected Jsonb for nested, got {other:?}"),
    }

    // -> with integer key on array
    let v = scalar("SELECT data->'tags'->0 FROM docs", &mut storage, &mut txn);
    assert_eq!(v, Value::Text("a".into()));
}

// ── JSON_EXTRACT on JSONB ────────────────────────────────────────────────────

#[test]
fn test_json_extract_on_jsonb() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"name":"Bob","score":99.5,"active":true}')"#,
        &mut storage,
        &mut txn,
    );

    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.name') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Text("Bob".into())
    );
    assert_eq!(
        scalar(
            "SELECT JSON_EXTRACT(data, '$.score') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Real(99.5)
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
            "SELECT JSON_EXTRACT(data, '$.missing') FROM docs",
            &mut storage,
            &mut txn
        ),
        Value::Null
    );
}

// ── JSON_KEYS / JSON_TYPE / JSON_VALID on JSONB ──────────────────────────────

#[test]
fn test_metadata_functions_on_jsonb() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, data JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"b":2,"a":1}')"#,
        &mut storage,
        &mut txn,
    );

    assert_eq!(
        scalar("SELECT JSON_TYPE(data) FROM docs", &mut storage, &mut txn),
        Value::Text("OBJECT".into())
    );
    assert_eq!(
        scalar("SELECT JSON_VALID(data) FROM docs", &mut storage, &mut txn),
        Value::Int(1)
    );
    // JSON_KEYS returns a JSON array
    let keys_val = scalar("SELECT JSON_KEYS(data) FROM docs", &mut storage, &mut txn);
    match keys_val {
        Value::Json(s) => {
            let parsed: serde_json::Value = serde_json::from_str(&s).unwrap();
            assert!(parsed.is_array());
            let arr = parsed.as_array().unwrap();
            assert_eq!(arr.len(), 2);
        }
        other => panic!("expected Json string from JSON_KEYS, got {other:?}"),
    }
}

// ── JSON_MERGE_PATCH ─────────────────────────────────────────────────────────

#[test]
fn test_json_merge_patch() {
    let (mut storage, mut txn) = common::setup();

    // Merge two objects — patch adds and overwrites keys
    let v = scalar(
        r#"SELECT JSON_TYPE(JSON_MERGE_PATCH('{"a":1,"b":2}', '{"b":3,"c":4}'))"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(v, Value::Text("OBJECT".into()));

    // Null values in patch delete keys
    let v = scalar(
        r#"SELECT JSON_EXTRACT(CAST(JSON_MERGE_PATCH('{"a":1,"b":2}', '{"b":null}') AS JSON), '$.b')"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(v, Value::Null);

    // NULL args propagate
    let v = scalar(
        r#"SELECT JSON_MERGE_PATCH(NULL, '{"a":1}')"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(v, Value::Null);
}

// ── JSON_CONTAINS ────────────────────────────────────────────────────────────

#[test]
fn test_json_contains() {
    let (mut storage, mut txn) = common::setup();

    // Object containment
    assert_eq!(
        scalar(
            r#"SELECT JSON_CONTAINS('{"a":1,"b":2,"c":3}', '{"a":1,"c":3}')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Int(1)
    );

    // Not contained
    assert_eq!(
        scalar(
            r#"SELECT JSON_CONTAINS('{"a":1}', '{"a":2}')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Int(0)
    );

    // Array containment
    assert_eq!(
        scalar(
            r#"SELECT JSON_CONTAINS('[1,2,3,4]', '[2,4]')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Int(1)
    );
}

// ── JSON_OVERLAPS ────────────────────────────────────────────────────────────

#[test]
fn test_json_overlaps() {
    let (mut storage, mut txn) = common::setup();

    assert_eq!(
        scalar(
            r#"SELECT JSON_OVERLAPS('[1,2,3]', '[3,4,5]')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Int(1)
    );

    assert_eq!(
        scalar(
            r#"SELECT JSON_OVERLAPS('[1,2]', '[3,4]')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Int(0)
    );
}

// ── JSON_ARRAY_LENGTH ────────────────────────────────────────────────────────

#[test]
fn test_json_array_length() {
    let (mut storage, mut txn) = common::setup();

    assert_eq!(
        scalar(
            r#"SELECT JSON_ARRAY_LENGTH('[1,2,3]')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Int(3)
    );

    // Non-array returns NULL
    assert_eq!(
        scalar(
            r#"SELECT JSON_ARRAY_LENGTH('{"a":1}')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Null
    );

    // On JSONB column
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE arr (id INT, data JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO arr VALUES (1, '[10,20,30,40]')"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(
        scalar(
            "SELECT JSON_ARRAY_LENGTH(data) FROM arr",
            &mut storage,
            &mut txn,
        ),
        Value::Int(4)
    );
}

// ── JSON_DEPTH ───────────────────────────────────────────────────────────────

#[test]
fn test_json_depth() {
    let (mut storage, mut txn) = common::setup();

    // Scalar depth = 1
    assert_eq!(
        scalar(r#"SELECT JSON_DEPTH('42')"#, &mut storage, &mut txn),
        Value::Int(1)
    );

    // Flat object = 2
    assert_eq!(
        scalar(r#"SELECT JSON_DEPTH('{"a":1}')"#, &mut storage, &mut txn,),
        Value::Int(2)
    );

    // Nested = 3
    assert_eq!(
        scalar(
            r#"SELECT JSON_DEPTH('{"a":{"b":1}}')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Int(3)
    );
}

// ── JSON_PRETTY ──────────────────────────────────────────────────────────────

#[test]
fn test_json_pretty() {
    let (mut storage, mut txn) = common::setup();

    let v = scalar(r#"SELECT JSON_PRETTY('{"a":1}')"#, &mut storage, &mut txn);
    match v {
        Value::Text(s) => {
            assert!(s.contains('\n'), "pretty output should be multi-line");
            assert!(s.contains("\"a\""), "should contain key");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── TO_JSONB / JSONB ─────────────────────────────────────────────────────────

#[test]
fn test_to_jsonb() {
    let (mut storage, mut txn) = common::setup();

    let v = scalar(r#"SELECT TO_JSONB('{"x":1}')"#, &mut storage, &mut txn);
    match v {
        Value::Jsonb(_) => {} // success
        other => panic!("expected Jsonb, got {other:?}"),
    }

    // Also works on arrays
    let v = scalar(r#"SELECT TO_JSONB('[1,2,3]')"#, &mut storage, &mut txn);
    match v {
        Value::Jsonb(_) => {}
        other => panic!("expected Jsonb for array, got {other:?}"),
    }
}

// ── JSONPath functions ───────────────────────────────────────────────────────

#[test]
fn test_json_path_exists() {
    let (mut storage, mut txn) = common::setup();

    assert_eq!(
        scalar(
            r#"SELECT JSON_PATH_EXISTS('{"a":{"b":1}}', '$.a.b')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Bool(true)
    );

    assert_eq!(
        scalar(
            r#"SELECT JSON_PATH_EXISTS('{"a":1}', '$.b')"#,
            &mut storage,
            &mut txn,
        ),
        Value::Bool(false)
    );
}

#[test]
fn test_json_path_query() {
    let (mut storage, mut txn) = common::setup();

    // Wildcard returns all values
    let v = scalar(
        r#"SELECT JSON_PATH_QUERY('{"a":1,"b":2,"c":3}', '$.*')"#,
        &mut storage,
        &mut txn,
    );
    match v {
        Value::Json(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s).unwrap();
            assert_eq!(arr.as_array().unwrap().len(), 3);
        }
        other => panic!("expected Json, got {other:?}"),
    }

    // Array wildcard
    let v = scalar(
        r#"SELECT JSON_PATH_QUERY('[10,20,30]', '$[*]')"#,
        &mut storage,
        &mut txn,
    );
    match v {
        Value::Json(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s).unwrap();
            assert_eq!(arr.as_array().unwrap().len(), 3);
        }
        other => panic!("expected Json, got {other:?}"),
    }
}

#[test]
fn test_json_path_query_first() {
    let (mut storage, mut txn) = common::setup();

    let v = scalar(
        r#"SELECT JSON_PATH_QUERY_FIRST('[10,20,30]', '$[*]')"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(v, Value::Int(10));

    // No match returns NULL
    let v = scalar(
        r#"SELECT JSON_PATH_QUERY_FIRST('{"a":1}', '$.b')"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(v, Value::Null);
}

#[test]
fn test_json_path_filter() {
    let (mut storage, mut txn) = common::setup();

    // Filter expression: find items where price > 10
    let v = scalar(
        r#"SELECT JSON_PATH_QUERY('[{"name":"a","price":5},{"name":"b","price":15},{"name":"c","price":25}]', '$[?(@.price > 10)]')"#,
        &mut storage,
        &mut txn,
    );
    match v {
        Value::Json(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s).unwrap();
            let items = arr.as_array().unwrap();
            assert_eq!(items.len(), 2);
            assert_eq!(items[0]["name"], "b");
            assert_eq!(items[1]["name"], "c");
        }
        other => panic!("expected Json, got {other:?}"),
    }
}

#[test]
fn test_json_path_recursive() {
    let (mut storage, mut txn) = common::setup();

    // Recursive descent: find all "id" keys at any depth
    let v = scalar(
        r#"SELECT JSON_PATH_QUERY('{"id":1,"child":{"id":2,"nested":{"id":3}}}', '$..id')"#,
        &mut storage,
        &mut txn,
    );
    match v {
        Value::Json(s) => {
            let arr: serde_json::Value = serde_json::from_str(&s).unwrap();
            let items = arr.as_array().unwrap();
            assert_eq!(items.len(), 3);
        }
        other => panic!("expected Json, got {other:?}"),
    }
}

// ── CAST between JSON and JSONB ──────────────────────────────────────────────

#[test]
fn test_cast_json_jsonb() {
    let (mut storage, mut txn) = common::setup();

    // CAST JSON to JSONB
    let v = scalar(r#"SELECT CAST('{"a":1}' AS JSONB)"#, &mut storage, &mut txn);
    match v {
        Value::Jsonb(_) => {}
        other => panic!("expected Jsonb from CAST, got {other:?}"),
    }
}

// ── JSONB in WHERE clause ────────────────────────────────────────────────────

#[test]
fn test_jsonb_where_clause() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE events (id INT, payload JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO events VALUES (1, '{"type":"click","x":100}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO events VALUES (2, '{"type":"scroll","y":200}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO events VALUES (3, '{"type":"click","x":300}')"#,
        &mut storage,
        &mut txn,
    );

    // Filter using ->> (text extract) in WHERE
    let result = rows(
        "SELECT id FROM events WHERE payload->>'type' = 'click' ORDER BY id",
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[1][0], Value::Int(3));
}

// ── NULL semantics ───────────────────────────────────────────────────────────

#[test]
fn test_jsonb_null_semantics() {
    let (mut storage, mut txn) = common::setup();

    assert_eq!(
        scalar(
            "SELECT JSON_MERGE_PATCH(NULL, '{}')",
            &mut storage,
            &mut txn
        ),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_CONTAINS(NULL, '{}')", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_OVERLAPS(NULL, '[]')", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_ARRAY_LENGTH(NULL)", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_DEPTH(NULL)", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_PRETTY(NULL)", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT TO_JSONB(NULL)", &mut storage, &mut txn),
        Value::Null
    );
    assert_eq!(
        scalar("SELECT JSON_PATH_EXISTS(NULL, '$')", &mut storage, &mut txn),
        Value::Null
    );
}
