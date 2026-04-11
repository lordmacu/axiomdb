//! Integration tests for Phases 11.6/11.7 — Full-Text Search (MATCH function).
//!
//! Tests the eval-only path: MATCH(column, query) → relevance score.
//! Covers: basic terms, boolean operators (+/-), phrases, prefix, OR.

mod common;

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

fn score(v: &Value) -> f64 {
    match v {
        Value::Real(f) => *f,
        other => panic!("expected Real, got {other:?}"),
    }
}

// ── Basic MATCH ──────────────────────────────────────────────────────────────

#[test]
fn test_match_basic_term() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE articles (id INT, body TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO articles VALUES (1, 'the quick brown fox jumps over the lazy dog')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO articles VALUES (2, 'hello world')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO articles VALUES (3, 'the dog barks at the fox')",
        &mut storage,
        &mut txn,
    );

    // MATCH returns > 0 for matching documents
    let v = scalar(
        "SELECT MATCH(body, 'fox') FROM articles WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert!(score(&v) > 0.0, "fox should match in article 1");

    // No match returns 0
    let v = scalar(
        "SELECT MATCH(body, 'fox') FROM articles WHERE id = 2",
        &mut storage,
        &mut txn,
    );
    assert_eq!(score(&v), 0.0, "fox should not match in article 2");
}

#[test]
fn test_match_in_where_clause() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, content TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (1, 'rust programming language')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (2, 'python programming language')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (3, 'cooking recipes')",
        &mut storage,
        &mut txn,
    );

    // Use MATCH in WHERE to filter
    let result = rows(
        "SELECT id FROM docs WHERE MATCH(content, 'programming') > 0 ORDER BY id",
        &mut storage,
        &mut txn,
    );
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[1][0], Value::Int(2));
}

// ── Boolean operators: +required -excluded ───────────────────────────────────

#[test]
fn test_match_required_term() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, body TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (1, 'database engine storage')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (2, 'database indexing')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (3, 'web server http')",
        &mut storage,
        &mut txn,
    );

    // +database requires "database" to be present
    let v1 = scalar(
        "SELECT MATCH(body, '+database') FROM docs WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert!(score(&v1) > 0.0);

    let v3 = scalar(
        "SELECT MATCH(body, '+database') FROM docs WHERE id = 3",
        &mut storage,
        &mut txn,
    );
    assert_eq!(score(&v3), 0.0);
}

#[test]
fn test_match_excluded_term() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, body TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (1, 'database storage engine')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (2, 'database indexing engine')",
        &mut storage,
        &mut txn,
    );

    // -storage excludes docs with "storage"
    let v1 = scalar(
        "SELECT MATCH(body, 'database -storage') FROM docs WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert_eq!(score(&v1), 0.0, "should be excluded by -storage");

    let v2 = scalar(
        "SELECT MATCH(body, 'database -storage') FROM docs WHERE id = 2",
        &mut storage,
        &mut txn,
    );
    assert!(score(&v2) > 0.0, "should match (no 'storage')");
}

// ── Phrase search ────────────────────────────────────────────────────────────

#[test]
fn test_match_phrase() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, body TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (1, 'the quick brown fox')"#,
        &mut storage,
        &mut txn,
    );
    common::run(
        r#"INSERT INTO docs VALUES (2, 'brown quick fox')"#,
        &mut storage,
        &mut txn,
    );

    // Exact phrase "quick brown" matches only doc 1 (adjacent order)
    let v1 = scalar(
        r#"SELECT MATCH(body, '"quick brown"') FROM docs WHERE id = 1"#,
        &mut storage,
        &mut txn,
    );
    assert!(score(&v1) > 0.0, "exact phrase should match in doc 1");

    let v2 = scalar(
        r#"SELECT MATCH(body, '"quick brown"') FROM docs WHERE id = 2"#,
        &mut storage,
        &mut txn,
    );
    assert_eq!(
        score(&v2),
        0.0,
        "phrase should NOT match in doc 2 (wrong order)"
    );
}

// ── Prefix search ────────────────────────────────────────────────────────────

#[test]
fn test_match_prefix() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, body TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (1, 'programming is fun')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (2, 'professional work')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (3, 'cooking dinner')",
        &mut storage,
        &mut txn,
    );

    // prog* matches "programming" and "professional"
    let v1 = scalar(
        "SELECT MATCH(body, 'pro*') FROM docs WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert!(score(&v1) > 0.0);

    let v2 = scalar(
        "SELECT MATCH(body, 'pro*') FROM docs WHERE id = 2",
        &mut storage,
        &mut txn,
    );
    assert!(score(&v2) > 0.0);

    let v3 = scalar(
        "SELECT MATCH(body, 'pro*') FROM docs WHERE id = 3",
        &mut storage,
        &mut txn,
    );
    assert_eq!(score(&v3), 0.0);
}

// ── OR operator ──────────────────────────────────────────────────────────────

#[test]
fn test_match_or() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, body TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (1, 'rust programming')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (2, 'python scripting')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (3, 'cooking recipes')",
        &mut storage,
        &mut txn,
    );

    // rust | python — matches either
    let v1 = scalar(
        "SELECT MATCH(body, 'rust | python') FROM docs WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    assert!(score(&v1) > 0.0);

    let v2 = scalar(
        "SELECT MATCH(body, 'rust | python') FROM docs WHERE id = 2",
        &mut storage,
        &mut txn,
    );
    assert!(score(&v2) > 0.0);

    let v3 = scalar(
        "SELECT MATCH(body, 'rust | python') FROM docs WHERE id = 3",
        &mut storage,
        &mut txn,
    );
    assert_eq!(score(&v3), 0.0);
}

// ── NULL propagation ─────────────────────────────────────────────────────────

#[test]
fn test_match_null_returns_zero() {
    let (mut storage, mut txn) = common::setup();

    let v = scalar("SELECT MATCH(NULL, 'test')", &mut storage, &mut txn);
    assert_eq!(score(&v), 0.0);

    let v = scalar("SELECT MATCH('hello world', NULL)", &mut storage, &mut txn);
    assert_eq!(score(&v), 0.0);
}

// ── Multiple terms ───────────────────────────────────────────────────────────

#[test]
fn test_match_multiple_optional_terms() {
    let (mut storage, mut txn) = common::setup();
    common::run(
        "CREATE TABLE docs (id INT, body TEXT)",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (1, 'database storage engine query optimizer')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (2, 'database storage')",
        &mut storage,
        &mut txn,
    );
    common::run(
        "INSERT INTO docs VALUES (3, 'unrelated text')",
        &mut storage,
        &mut txn,
    );

    // More matching terms = higher score
    let v1 = scalar(
        "SELECT MATCH(body, 'database storage engine') FROM docs WHERE id = 1",
        &mut storage,
        &mut txn,
    );
    let v2 = scalar(
        "SELECT MATCH(body, 'database storage engine') FROM docs WHERE id = 2",
        &mut storage,
        &mut txn,
    );
    let v3 = scalar(
        "SELECT MATCH(body, 'database storage engine') FROM docs WHERE id = 3",
        &mut storage,
        &mut txn,
    );

    let s1 = score(&v1);
    let s2 = score(&v2);
    let s3 = score(&v3);

    assert!(
        s1 > s2,
        "doc 1 (3 terms) should score higher than doc 2 (2 terms)"
    );
    assert!(
        s2 > s3,
        "doc 2 (2 terms) should score higher than doc 3 (0 terms)"
    );
    assert_eq!(s3, 0.0);
}
