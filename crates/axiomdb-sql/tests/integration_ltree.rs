mod common;

use axiomdb_sql::QueryResult;
use axiomdb_types::Value;

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected rows, got {other:?}"),
    }
}

fn scalar(result: QueryResult) -> Value {
    let mut r = rows(result);
    assert_eq!(r.len(), 1, "expected exactly 1 row");
    assert_eq!(r[0].len(), 1, "expected exactly 1 column");
    r.remove(0).remove(0)
}

fn run(sql: &str) -> Value {
    let (mut storage, mut txn) = common::setup();
    scalar(common::run(sql, &mut storage, &mut txn))
}

fn run_ok(sql: &str) -> QueryResult {
    let (mut storage, mut txn) = common::setup();
    common::run(sql, &mut storage, &mut txn)
}

fn run_err(sql: &str) -> axiomdb_core::error::DbError {
    let (mut storage, mut txn) = common::setup();
    common::run_result(sql, &mut storage, &mut txn).expect_err("expected an error")
}

// ── DDL: CREATE TABLE with LTREE column ──────────────────────────────────────

#[test]
fn create_table_with_ltree_column() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE paths (id INT, path LTREE)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO paths VALUES (1, 'org.eng.backend')",
        &mut storage,
        &mut txn,
    );
    let result = common::run(
        "SELECT path FROM paths WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    let r = rows(result);
    assert_eq!(r[0][0], Value::Ltree("org.eng.backend".into()));
}

// ── Literal / coerce ──────────────────────────────────────────────────────────

#[test]
fn ltree_literal_select() {
    assert_eq!(
        run("SELECT 'org.eng.backend'::LTREE"),
        Value::Ltree("org.eng.backend".into())
    );
}

#[test]
fn ltree_display_as_text() {
    // LTREE → TEXT coercion via CAST
    assert_eq!(
        run("SELECT CAST('a.b.c'::LTREE AS TEXT)"),
        Value::Text("a.b.c".into())
    );
}

#[test]
fn ltree_invalid_path_rejected() {
    let e = run_err("SELECT 'a..b'::LTREE");
    let msg = format!("{e:?}");
    assert!(
        msg.contains("LTREE") || msg.contains("invalid") || msg.contains("label"),
        "{msg}"
    );
}

// ── Operators ─────────────────────────────────────────────────────────────────

#[test]
fn ltree_equality() {
    assert_eq!(
        run("SELECT 'a.b.c'::LTREE = 'a.b.c'::LTREE"),
        Value::Bool(true)
    );
    assert_eq!(
        run("SELECT 'a.b.c'::LTREE = 'a.b.d'::LTREE"),
        Value::Bool(false)
    );
}

#[test]
fn ltree_less_than_ordering() {
    assert_eq!(run("SELECT 'a.b'::LTREE < 'a.c'::LTREE"), Value::Bool(true));
}

#[test]
fn ltree_ancestor_operator() {
    // 'a.b' @> 'a.b.c' → true
    assert_eq!(
        run("SELECT 'a.b'::LTREE @> 'a.b.c'::LTREE"),
        Value::Bool(true)
    );
    // 'a.b.c' @> 'a.b' → false
    assert_eq!(
        run("SELECT 'a.b.c'::LTREE @> 'a.b'::LTREE"),
        Value::Bool(false)
    );
}

#[test]
fn ltree_descendant_operator() {
    // 'a.b.c' <@ 'a.b' → true
    assert_eq!(
        run("SELECT 'a.b.c'::LTREE <@ 'a.b'::LTREE"),
        Value::Bool(true)
    );
    // 'a.b' <@ 'a.b.c' → false
    assert_eq!(
        run("SELECT 'a.b'::LTREE <@ 'a.b.c'::LTREE"),
        Value::Bool(false)
    );
}

#[test]
fn ltree_lquery_match_operator() {
    // exact match via wildcard
    assert_eq!(run("SELECT 'a.b.c'::LTREE ~ 'a.*.c'"), Value::Bool(true));
    assert_eq!(run("SELECT 'a.b.c'::LTREE ~ 'a.b'"), Value::Bool(false));
}

#[test]
fn ltree_concat_operator() {
    assert_eq!(
        run("SELECT 'a.b'::LTREE || 'c.d'::LTREE"),
        Value::Ltree("a.b.c.d".into())
    );
}

// ── Scalar functions ──────────────────────────────────────────────────────────

#[test]
fn nlevel_function() {
    assert_eq!(run("SELECT nlevel('a.b.c'::LTREE)"), Value::Int(3));
    assert_eq!(run("SELECT nlevel('a'::LTREE)"), Value::Int(1));
}

#[test]
fn subpath_function_no_len() {
    // subpath(path, offset) returns suffix from offset
    assert_eq!(
        run("SELECT subpath('a.b.c.d'::LTREE, 1)"),
        Value::Ltree("b.c.d".into())
    );
}

#[test]
fn subpath_function_with_len() {
    assert_eq!(
        run("SELECT subpath('a.b.c.d'::LTREE, 1, 2)"),
        Value::Ltree("b.c".into())
    );
}

#[test]
fn subpath_out_of_bounds_returns_null() {
    assert_eq!(run("SELECT subpath('a.b'::LTREE, 5)"), Value::Null);
}

#[test]
fn subltree_function() {
    // subltree(path, start, end) — labels [start, end)
    assert_eq!(
        run("SELECT subltree('a.b.c.d'::LTREE, 0, 2)"),
        Value::Ltree("a.b".into())
    );
    assert_eq!(
        run("SELECT subltree('a.b.c.d'::LTREE, 1, 3)"),
        Value::Ltree("b.c".into())
    );
}

#[test]
fn index_function_found() {
    // 'a.b' starts at position 0 within 'a.b.c.a.b'
    assert_eq!(
        run("SELECT index('a.b.c.a.b'::LTREE, 'a.b'::LTREE)"),
        Value::Int(0)
    );
}

#[test]
fn index_function_not_found() {
    assert_eq!(
        run("SELECT index('a.b.c'::LTREE, 'x.y'::LTREE)"),
        Value::Int(-1)
    );
}

#[test]
fn lca_function_two_paths() {
    assert_eq!(
        run("SELECT lca('a.b.c'::LTREE, 'a.b.d'::LTREE)"),
        Value::Ltree("a.b".into())
    );
}

#[test]
fn lca_function_disjoint() {
    // No common prefix — returns empty string wrapped in Ltree
    assert_eq!(
        run("SELECT lca('x.y'::LTREE, 'a.b'::LTREE)"),
        Value::Ltree("".into())
    );
}

#[test]
fn text2ltree_function() {
    assert_eq!(
        run("SELECT text2ltree('org.eng')"),
        Value::Ltree("org.eng".into())
    );
}

#[test]
fn ltree2text_function() {
    assert_eq!(
        run("SELECT ltree2text('org.eng'::LTREE)"),
        Value::Text("org.eng".into())
    );
}

// ── Table-level: ancestor filter ──────────────────────────────────────────────

#[test]
fn ancestor_filter_in_where_clause() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE org (id INT, path LTREE)",
        &mut storage,
        &mut txn,
    );
    for (id, path) in [
        (1, "org"),
        (2, "org.eng"),
        (3, "org.eng.backend"),
        (4, "org.eng.frontend"),
        (5, "org.hr"),
    ] {
        common::run(
            &format!("INSERT INTO org VALUES ({id}, '{path}')"),
            &mut storage,
            &mut txn,
        );
    }
    // All descendants of 'org.eng'
    let result = common::run(
        "SELECT id FROM org WHERE 'org.eng'::LTREE @> path ORDER BY id",
        &mut storage,
        &mut txn,
    );
    let ids: Vec<i64> = rows(result)
        .into_iter()
        .map(|mut r| match r.remove(0) {
            Value::Int(n) => n as i64,
            Value::BigInt(n) => n,
            other => panic!("unexpected {other:?}"),
        })
        .collect();
    assert_eq!(ids, vec![2, 3, 4]);
}

// ── NULL propagation ──────────────────────────────────────────────────────────

#[test]
fn null_propagation_in_operators() {
    assert_eq!(run("SELECT NULL::LTREE @> 'a.b'::LTREE"), Value::Null);
    assert_eq!(run("SELECT 'a.b'::LTREE @> NULL::LTREE"), Value::Null);
}

#[test]
fn null_propagation_in_functions() {
    assert_eq!(run("SELECT nlevel(NULL::LTREE)"), Value::Null);
    assert_eq!(run("SELECT subpath(NULL::LTREE, 1)"), Value::Null);
    assert_eq!(run("SELECT lca(NULL::LTREE, 'a.b'::LTREE)"), Value::Null);
    assert_eq!(run("SELECT text2ltree(NULL)"), Value::Null);
    assert_eq!(run("SELECT ltree2text(NULL::LTREE)"), Value::Null);
}
