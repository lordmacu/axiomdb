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

fn int_ids(result: Vec<Vec<Value>>) -> Vec<i32> {
    result
        .into_iter()
        .map(|row| match row.into_iter().next().unwrap() {
            Value::Int(n) => n,
            other => panic!("expected integer id, got {other:?}"),
        })
        .collect()
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

#[test]
fn test_json_and_jsonb_realistic_event_filters_match() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE checkout_events (id INT PRIMARY KEY, raw_payload JSON, bin_payload JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO checkout_events VALUES (1001, '{"tenant":"acme","event":"checkout","cart":{"currency":"USD","total":128},"flags":{"vip":true}}', '{"tenant":"acme","event":"checkout","cart":{"currency":"USD","total":128},"flags":{"vip":true}}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO checkout_events VALUES (1002, '{"tenant":"globex","event":"refund","cart":{"currency":"EUR","total":89},"flags":{"vip":false}}', '{"tenant":"globex","event":"refund","cart":{"currency":"EUR","total":89},"flags":{"vip":false}}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO checkout_events VALUES (1003, '{"tenant":"acme","event":"checkout","cart":{"currency":"USD","total":59},"flags":{"vip":false}}', '{"tenant":"acme","event":"checkout","cart":{"currency":"USD","total":59},"flags":{"vip":false}}')"#,
        &mut storage,
        &mut txn,
    );

    let json_ids = int_ids(rows(
        "SELECT id FROM checkout_events WHERE JSON_EXTRACT(raw_payload, '$.tenant') = 'acme' AND JSON_EXTRACT(raw_payload, '$.event') = 'checkout' ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    let jsonb_ids = int_ids(rows(
        "SELECT id FROM checkout_events WHERE bin_payload->>'tenant' = 'acme' AND bin_payload->>'event' = 'checkout' ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(json_ids, vec![1001, 1003]);
    assert_eq!(jsonb_ids, json_ids);

    assert_eq!(
        int_ids(rows(
            "SELECT id FROM checkout_events WHERE JSON_EXTRACT(bin_payload, '$.flags.vip') = TRUE",
            &mut storage,
            &mut txn,
        )),
        vec![1001]
    );
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

// ── GIN index (Phase 11.17) ──────────────────────────────────────────────────

fn setup_gin_table(
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
) {
    common::run(
        "CREATE TABLE docs (id INT PRIMARY KEY, data JSONB)",
        storage,
        txn,
    );
    common::run(
        r#"CREATE INDEX idx_docs_gin ON docs USING GIN (data)"#,
        storage,
        txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, '{"name":"Alice","role":"admin","active":true}')"#,
        storage,
        txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (2, '{"name":"Bob","role":"user","active":false}')"#,
        storage,
        txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (3, '{"name":"Carol","role":"admin","score":42}')"#,
        storage,
        txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (4, '{"tags":["rust","database"],"version":1}')"#,
        storage,
        txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (5, '{"name":"Dave","nested":{"city":"Madrid"}}')"#,
        storage,
        txn,
    );
}

#[test]
fn test_gin_index_created() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(&storage, snap).unwrap();
    let table = reader.get_table("public", "docs").unwrap().unwrap();
    let indexes = reader.list_indexes(table.id).unwrap();
    let gin_idx = indexes.iter().find(|i| i.name == "idx_docs_gin").unwrap();
    assert_eq!(gin_idx.index_type, 4);
}

#[test]
fn test_gin_at_contains_key_present() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    // Rows 1 and 3 have role=admin
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"role":"admin"}'"#,
        &mut storage,
        &mut txn,
    );
    let mut ids: Vec<i32> = result
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 3]);
}

#[test]
fn test_gin_at_contains_no_match() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"role":"superuser"}'"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 0);
}

#[test]
fn test_gin_at_contains_multiple_terms() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    // Only row 1 has both name=Alice AND active=true
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"name":"Alice","active":true}'"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
}

#[test]
fn test_gin_rechecks_nested_containment_false_positive_terms() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE audit_events (id INT PRIMARY KEY, payload JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "CREATE INDEX idx_audit_payload_gin ON audit_events USING GIN (payload)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO audit_events VALUES (1, '{"tenant":"acme","actor":{"id":42,"role":"admin"},"tags":["billing","manual"],"metadata":{"source":"api","region":"us-east"}}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO audit_events VALUES (2, '{"tenant":"acme","actor":{"id":43,"role":"user"},"tags":["billing","admin"],"metadata":{"source":"api","region":"us-east"}}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO audit_events VALUES (3, '{"tenant":"globex","actor":{"id":42,"role":"admin"},"tags":["billing"],"metadata":{"source":"api","region":"eu-west"}}')"#,
        &mut storage,
        &mut txn,
    );

    let result = rows(
        r#"SELECT id FROM audit_events WHERE payload @> '{"tenant":"acme","actor":{"role":"admin"},"tags":["billing"]}' ORDER BY id"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(int_ids(result), vec![1]);
}

#[test]
fn test_jsonb_gin_super_complex_payload_recheck() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE complex_events (id INT PRIMARY KEY, payload JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "CREATE INDEX idx_complex_events_payload_gin ON complex_events USING GIN (payload)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO complex_events VALUES (10, '{"tenant":"acme","env":"prod","user":{"id":"u-100","roles":["admin","approver"],"profile":{"country":"CO","flags":{"mfa":true,"beta":false}}},"order":{"id":"ord-9001","total":1532.75,"currency":"USD","items":[{"sku":"db-pro","qty":2,"attrs":{"tier":"enterprise","regions":["us-east","eu-west"]}},{"sku":"support","qty":1,"attrs":{"tier":"gold","regions":["global"]}}]},"risk":{"score":12,"signals":["velocity","geo"]},"audit":{"trace_id":"tr-aaa","source":"api","checkpoints":[{"name":"created","ok":true},{"name":"approved","ok":true}]},"deleted_at":null}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO complex_events VALUES (11, '{"tenant":"acme","env":"prod","user":{"id":"u-101","roles":["analyst"],"profile":{"country":"CO","flags":{"mfa":true,"beta":true}}},"order":{"id":"ord-9002","total":1532.75,"currency":"USD","items":[{"sku":"db-pro","qty":2,"attrs":{"tier":"starter","regions":["us-east"]}}]},"risk":{"score":12,"signals":["velocity"]},"audit":{"trace_id":"tr-bbb","source":"api","notes":["admin","enterprise","eu-west"]},"deleted_at":null}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO complex_events VALUES (12, '{"tenant":"globex","env":"prod","user":{"id":"u-200","roles":["admin"],"profile":{"country":"US","flags":{"mfa":true,"beta":false}}},"order":{"id":"ord-9003","total":1532.75,"currency":"USD","items":[{"sku":"db-pro","qty":2,"attrs":{"tier":"enterprise","regions":["eu-west"]}}]},"risk":{"score":12,"signals":["geo"]},"audit":{"trace_id":"tr-ccc","source":"api"},"deleted_at":null}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO complex_events VALUES (13, '{"tenant":"acme","env":"staging","user":{"id":"u-300","roles":["admin"],"profile":{"country":"CO","flags":{"mfa":true,"beta":false}}},"order":{"id":"ord-9004","total":1532.75,"currency":"USD","items":[{"sku":"db-pro","qty":2,"attrs":{"tier":"enterprise","regions":["eu-west"]}}]},"risk":{"score":12,"signals":["velocity","geo"]},"audit":{"trace_id":"tr-ddd","source":"api"},"deleted_at":null}')"#,
        &mut storage,
        &mut txn,
    );

    let matched = int_ids(rows(
        r#"SELECT id FROM complex_events WHERE payload @> '{"tenant":"acme","env":"prod","user":{"roles":["admin"],"profile":{"flags":{"mfa":true}}},"order":{"items":[{"sku":"db-pro","attrs":{"tier":"enterprise","regions":["eu-west"]}}]},"risk":{"score":12,"signals":["velocity","geo"]}}' ORDER BY id"#,
        &mut storage,
        &mut txn,
    ));
    assert_eq!(matched, vec![10]);

    assert_eq!(
        scalar(
            "SELECT payload->'order'->'items'->0->'attrs'->'regions'->1 FROM complex_events WHERE id = 10",
            &mut storage,
            &mut txn,
        ),
        Value::Text("eu-west".into())
    );
    assert_eq!(
        int_ids(rows(
            "SELECT id FROM complex_events WHERE JSON_EXTRACT(payload, '$.user.profile.flags.mfa') = TRUE ORDER BY id",
            &mut storage,
            &mut txn,
        )),
        vec![10, 11, 12, 13]
    );
}

#[test]
fn test_gin_at_contains_bool_false() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    // Only row 2 has active=false
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"active":false}'"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(2));
}

#[test]
fn test_gin_at_contains_number_value() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    // Only row 3 has score=42
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"score":42}'"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(3));
}

#[test]
fn test_gin_insert_updates_index() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    // Insert a new row after index creation
    common::run(
        r#"INSERT INTO docs VALUES (6, '{"name":"Eve","role":"admin","score":99}')"#,
        &mut storage,
        &mut txn,
    );
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"role":"admin"}'"#,
        &mut storage,
        &mut txn,
    );
    let mut ids: Vec<i32> = result
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    ids.sort();
    assert_eq!(ids, vec![1, 3, 6]);
}

#[test]
fn test_gin_create_after_existing_clustered_rows() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs_late_gin (id INT PRIMARY KEY, data JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs_late_gin VALUES (1, '{"role":"admin"}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs_late_gin VALUES (2, '{"role":"user"}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        "CREATE INDEX idx_late_gin ON docs_late_gin USING GIN (data)",
        &mut storage,
        &mut txn,
    );

    let result = rows(
        r#"SELECT id FROM docs_late_gin WHERE data @> '{"role":"admin"}'"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
}

#[test]
fn test_gin_delete_updates_index() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    // Delete row 1 (Alice, admin)
    common::run("DELETE FROM docs WHERE id = 1", &mut storage, &mut txn);
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"role":"admin"}'"#,
        &mut storage,
        &mut txn,
    );
    let ids: Vec<i32> = result
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    assert!(
        !ids.contains(&1),
        "deleted row 1 should not appear in GIN results"
    );
    assert!(ids.contains(&3), "row 3 should still be found via GIN");
}

#[test]
fn test_gin_update_updates_index() {
    let (mut storage, mut txn) = common::setup();
    setup_gin_table(&mut storage, &mut txn);
    // Change row 2 from user to admin
    common::run(
        r#"UPDATE docs SET data = '{"name":"Bob","role":"admin","active":false}' WHERE id = 2"#,
        &mut storage,
        &mut txn,
    );
    let result = rows(
        r#"SELECT id FROM docs WHERE data @> '{"role":"admin"}'"#,
        &mut storage,
        &mut txn,
    );
    let mut ids: Vec<i32> = result
        .iter()
        .map(|r| match r[0] {
            Value::Int(n) => n,
            _ => panic!(),
        })
        .collect();
    ids.sort();
    // Rows 1, 2, and 3 should now all be admin
    assert!(
        ids.contains(&2),
        "updated row 2 should now appear in GIN results for admin"
    );
}

#[test]
fn test_gin_at_op_without_index_scan() {
    // @> should work as a fallback full-scan even without GIN index
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE nodocs (id INT PRIMARY KEY, data JSONB)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO nodocs VALUES (1, '{"x":1}')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO nodocs VALUES (2, '{"y":2}')"#,
        &mut storage,
        &mut txn,
    );
    let result = rows(
        r#"SELECT id FROM nodocs WHERE data @> '{"x":1}'"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
}

#[test]
fn test_gin_explain_shows_gin_type() {
    let (mut storage, mut txn, mut bloom, mut ctx) = common::setup_ctx();
    setup_gin_table(&mut storage, &mut txn);
    let result = common::rows(
        common::run_ctx(
            r#"EXPLAIN SELECT id FROM docs WHERE data @> '{"role":"admin"}'"#,
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert!(!result.is_empty());
    // The "type" column (index 3) should be "gin"
    assert_eq!(result[0][3], Value::Text("gin".into()));
    // The "key" column (index 5) should name the GIN index
    assert_eq!(result[0][5], Value::Text("idx_docs_gin".into()));
}
