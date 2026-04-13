//! Integration tests for Phase 11.20a — `JSON_TABLE(...)` flat row source.
//!
//! Covers:
//!   - basic array shred into rows,
//!   - FOR ORDINALITY,
//!   - DEFAULT / NULL / ERROR on empty / error,
//!   - EXISTS PATH with TRUE/FALSE/UNKNOWN on error,
//!   - NULL document → zero rows,
//!   - Text document with invalid JSON → error,
//!   - JOIN base_table JOIN JSON_TABLE(...) ON true,
//!   - WHERE filter over JSON_TABLE columns,
//!   - parse errors: duplicate FOR ORDINALITY, missing COLUMNS.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    setup_ctx()
}

fn run(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Vec<Vec<Value>> {
    let res = run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
    match res {
        QueryResult::Rows { rows, .. } => rows,
        // Non-SELECT (CREATE TABLE / INSERT / …) — test just wants the statement
        // to succeed without returning rows.
        _ => Vec::new(),
    }
}

#[test]
fn shred_array_of_scalars() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        "SELECT v FROM JSON_TABLE('[1,2,3]', '$[*]' COLUMNS (v INT PATH '$')) AS t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[2][0], Value::Int(3));
}

#[test]
fn shred_array_of_objects_two_columns() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT id, name FROM JSON_TABLE(
            '[{"id":1,"name":"Ada"},{"id":2,"name":"Babbage"}]',
            '$[*]' COLUMNS (
                id   INT  PATH '$.id',
                name TEXT PATH '$.name'
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Text("Ada".into()));
    assert_eq!(rows[1][0], Value::Int(2));
    assert_eq!(rows[1][1], Value::Text("Babbage".into()));
}

#[test]
fn ordinality_counts_from_one() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT ord, v FROM JSON_TABLE('[10,20,30]', '$[*]'
            COLUMNS (ord FOR ORDINALITY, v INT PATH '$')) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::BigInt(1));
    assert_eq!(rows[1][0], Value::BigInt(2));
    assert_eq!(rows[2][0], Value::BigInt(3));
    assert_eq!(rows[2][1], Value::Int(30));
}

#[test]
fn default_on_empty_returns_default_value() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT age FROM JSON_TABLE(
            '[{"n":"Ada"},{"n":"Babbage","age":61}]',
            '$[*]' COLUMNS (age INT PATH '$.age' DEFAULT 0 ON EMPTY)
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(0));
    assert_eq!(rows[1][0], Value::Int(61));
}

#[test]
fn null_on_empty_default() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT age FROM JSON_TABLE(
            '[{"n":"Ada"}]',
            '$[*]' COLUMNS (age INT PATH '$.age')
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn error_on_empty_raises() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        r#"SELECT age FROM JSON_TABLE(
            '[{"n":"Ada"}]',
            '$[*]' COLUMNS (age INT PATH '$.age' ERROR ON EMPTY)
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(err.is_err(), "expected error, got {err:?}");
}

#[test]
fn type_mismatch_null_on_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT age FROM JSON_TABLE(
            '[{"age":"not a number"}]',
            '$[*]' COLUMNS (age INT PATH '$.age' NULL ON ERROR)
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Null);
}

#[test]
fn exists_path_true_false() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT has_a, has_b FROM JSON_TABLE(
            '[{"a":1},{"b":2}]',
            '$[*]' COLUMNS (
                has_a BOOLEAN EXISTS PATH '$.a',
                has_b BOOLEAN EXISTS PATH '$.b'
            )
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Bool(true));
    assert_eq!(rows[0][1], Value::Bool(false));
    assert_eq!(rows[1][0], Value::Bool(false));
    assert_eq!(rows[1][1], Value::Bool(true));
}

#[test]
fn null_document_yields_zero_rows() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        "SELECT v FROM JSON_TABLE(NULL, '$[*]' COLUMNS (v INT PATH '$')) AS t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 0);
}

#[test]
fn invalid_json_document_errors() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT v FROM JSON_TABLE('not-json', '$[*]' COLUMNS (v INT PATH '$')) AS t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(err.is_err(), "expected error, got {err:?}");
}

#[test]
fn join_base_table_with_json_table() {
    let (mut s, mut t, mut b, mut c) = setup();
    run(
        "CREATE TABLE u (id INT, label TEXT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run(
        "INSERT INTO u VALUES (1, 'alpha')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run(
        "INSERT INTO u VALUES (2, 'beta')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let rows = run(
        r#"SELECT u.id, jt.v FROM u
           JOIN JSON_TABLE('[10,20]', '$[*]' COLUMNS (v INT PATH '$')) AS jt
             ON TRUE
           ORDER BY u.id, jt.v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 4);
    assert_eq!(rows[0], vec![Value::Int(1), Value::Int(10)]);
    assert_eq!(rows[1], vec![Value::Int(1), Value::Int(20)]);
    assert_eq!(rows[2], vec![Value::Int(2), Value::Int(10)]);
    assert_eq!(rows[3], vec![Value::Int(2), Value::Int(20)]);
}

#[test]
fn where_predicate_on_json_table_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    let rows = run(
        r#"SELECT v FROM JSON_TABLE(
            '[1,2,3,4,5]', '$[*]' COLUMNS (v INT PATH '$')
        ) AS t WHERE v > 2 ORDER BY v"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert_eq!(rows.len(), 3);
    assert_eq!(rows[0][0], Value::Int(3));
    assert_eq!(rows[1][0], Value::Int(4));
    assert_eq!(rows[2][0], Value::Int(5));
}

#[test]
fn correlated_doc_in_join_is_rejected_11_20a() {
    // JSON_TABLE on the right of a JOIN with a correlated `doc` expression
    // (referencing the left-side row) requires LATERAL semantics — deferred
    // to 11.20d.
    let (mut s, mut t, mut b, mut c) = setup();
    run("CREATE TABLE u (tags TEXT)", &mut s, &mut t, &mut b, &mut c);
    run(
        "INSERT INTO u VALUES ('[1,2]')",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let err = run_ctx(
        r#"SELECT jt.v FROM u
           JOIN JSON_TABLE(u.tags, '$[*]' COLUMNS (v INT PATH '$')) AS jt
             ON TRUE"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    // Accept either a parse-path error or the deliberate NotImplemented
    // raised by the JOIN executor when the doc references a column.
    assert!(
        err.is_err(),
        "expected error for correlated doc, got {err:?}"
    );
}

#[test]
fn duplicate_for_ordinality_parse_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        r#"SELECT ord1, ord2 FROM JSON_TABLE('[]', '$[*]'
            COLUMNS (ord1 FOR ORDINALITY, ord2 FOR ORDINALITY)
        ) AS t"#,
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(err.is_err(), "expected parse error, got {err:?}");
}

#[test]
fn missing_columns_keyword_parse_error() {
    let (mut s, mut t, mut b, mut c) = setup();
    let err = run_ctx(
        "SELECT v FROM JSON_TABLE('[1]', '$[*]' (v INT PATH '$')) AS t",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    assert!(err.is_err(), "expected parse error, got {err:?}");
}

#[test]
fn table_named_json_table_without_parens_still_parses() {
    // Identifier rollback: a table named `json_table` used without `(...)`
    // should still resolve as a regular table reference.
    let (mut s, mut t, mut b, mut c) = setup();
    run(
        "CREATE TABLE json_table (id INT)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    run(
        "INSERT INTO json_table VALUES (42)",
        &mut s,
        &mut t,
        &mut b,
        &mut c,
    );
    let rows = run("SELECT id FROM json_table", &mut s, &mut t, &mut b, &mut c);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(42));
}
