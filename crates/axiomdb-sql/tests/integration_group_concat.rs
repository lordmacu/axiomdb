//! Integration tests for GROUP_CONCAT / string_agg aggregate (subfase 4.9e).
//!
//! Covers: basic concatenation, custom separator, NULL handling, ORDER BY,
//! DISTINCT, DISTINCT+ORDER BY, string_agg alias, GROUP BY queries,
//! ungrouped (implicit single-group) queries, and HAVING with GROUP_CONCAT.

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── helpers ───────────────────────────────────────────────────────────────────

fn setup() -> (MemoryStorage, TxnManager) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.into_path().join("test.wal");
    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    (storage, txn)
}

fn exec(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    let analyzed = analyze(stmt, storage, snap)?;
    execute(analyzed, storage, txn)
}

fn exec_ok(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> QueryResult {
    exec(sql, storage, txn).unwrap_or_else(|e| panic!("SQL failed: {sql}\n{e:?}"))
}

/// Execute and return the first row of results as a `Vec<Value>`.
fn first_row(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> Vec<Value> {
    match exec_ok(sql, storage, txn) {
        QueryResult::Rows { rows, .. } => rows.into_iter().next().expect("no rows"),
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Execute and return all rows.
fn all_rows(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> Vec<Vec<Value>> {
    match exec_ok(sql, storage, txn) {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

/// Scalar: single-row single-column result.
fn scalar(sql: &str, storage: &mut MemoryStorage, txn: &mut TxnManager) -> Value {
    first_row(sql, storage, txn)
        .into_iter()
        .next()
        .expect("no columns")
}

// ── fixture setup ─────────────────────────────────────────────────────────────

/// Create `post_tags(post_id INT, tag TEXT)` and insert test rows.
///
/// post_id=1: 'rust', 'db', 'async'    (3 distinct tags)
/// post_id=2: 'rust', 'web'             (2 tags, 'rust' shared with post 1)
/// post_id=3: NULL, NULL                (all NULLs)
/// (no rows for post_id=4 → empty group)
fn setup_post_tags(storage: &mut MemoryStorage, txn: &mut TxnManager) {
    exec_ok(
        "CREATE TABLE post_tags (post_id INT NOT NULL, tag TEXT)",
        storage,
        txn,
    );
    for sql in [
        "INSERT INTO post_tags VALUES (1, 'rust')",
        "INSERT INTO post_tags VALUES (1, 'db')",
        "INSERT INTO post_tags VALUES (1, 'async')",
        "INSERT INTO post_tags VALUES (2, 'rust')",
        "INSERT INTO post_tags VALUES (2, 'web')",
        "INSERT INTO post_tags VALUES (3, NULL)",
        "INSERT INTO post_tags VALUES (3, NULL)",
    ] {
        exec_ok(sql, storage, txn);
    }
}

// ── basic GROUP_CONCAT ────────────────────────────────────────────────────────

#[test]
fn group_concat_basic_comma_separator() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    // Ungrouped: all tags from post 1 joined (order not guaranteed, just check count).
    let v = scalar(
        "SELECT GROUP_CONCAT(tag) FROM post_tags WHERE post_id = 1",
        &mut st,
        &mut txn,
    );
    match v {
        Value::Text(s) => {
            // 3 values joined by comma → 2 commas
            let parts: Vec<&str> = s.split(',').collect();
            assert_eq!(parts.len(), 3, "expected 3 parts in: {s}");
            assert!(s.contains("rust"), "missing 'rust' in: {s}");
            assert!(s.contains("db"), "missing 'db' in: {s}");
            assert!(s.contains("async"), "missing 'async' in: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn group_concat_custom_separator() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(tag ORDER BY tag ASC SEPARATOR ' | ') FROM post_tags WHERE post_id = 1",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("async | db | rust".into()));
}

#[test]
fn group_concat_empty_separator() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(tag ORDER BY tag ASC SEPARATOR '') FROM post_tags WHERE post_id = 1",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("asyncdbrust".into()));
}

// ── NULL handling ─────────────────────────────────────────────────────────────

#[test]
fn group_concat_null_values_skipped() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    // post_id=1 has 3 non-NULL tags and no NULLs; verify NULLs from other rows aren't here.
    // Insert a mixed row with a NULL.
    exec_ok("INSERT INTO post_tags VALUES (1, NULL)", &mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(tag ORDER BY tag ASC) FROM post_tags WHERE post_id = 1",
        &mut st,
        &mut txn,
    );
    // Should still be exactly 3 values (NULL skipped).
    match v {
        Value::Text(s) => {
            let parts: Vec<&str> = s.split(',').collect();
            assert_eq!(parts.len(), 3, "NULL should be skipped, got: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

#[test]
fn group_concat_all_null_group_returns_null() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    // post_id=3 has only NULL tags.
    let v = scalar(
        "SELECT GROUP_CONCAT(tag) FROM post_tags WHERE post_id = 3",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Null);
}

#[test]
fn group_concat_empty_group_returns_null() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    // No rows match post_id = 99.
    let v = scalar(
        "SELECT GROUP_CONCAT(tag) FROM post_tags WHERE post_id = 99",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Null);
}

#[test]
fn group_concat_single_value_no_separator() {
    let (mut st, mut txn) = setup();
    exec_ok("CREATE TABLE names (name TEXT)", &mut st, &mut txn);
    exec_ok("INSERT INTO names VALUES ('alice')", &mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(name SEPARATOR ', ') FROM names",
        &mut st,
        &mut txn,
    );
    // Single value: no separator added.
    assert_eq!(v, Value::Text("alice".into()));
}

// ── ORDER BY inside GROUP_CONCAT ──────────────────────────────────────────────

#[test]
fn group_concat_order_by_asc() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(tag ORDER BY tag ASC) FROM post_tags WHERE post_id = 1",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("async,db,rust".into()));
}

#[test]
fn group_concat_order_by_desc() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(tag ORDER BY tag DESC) FROM post_tags WHERE post_id = 1",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("rust,db,async".into()));
}

#[test]
fn group_concat_order_by_multi_column() {
    let (mut st, mut txn) = setup();
    exec_ok("CREATE TABLE emp (dept INT, name TEXT)", &mut st, &mut txn);
    for (dept, name) in [(1, "charlie"), (2, "alice"), (1, "bob"), (2, "dave")] {
        exec_ok(
            &format!("INSERT INTO emp VALUES ({dept}, '{name}')"),
            &mut st,
            &mut txn,
        );
    }

    // ORDER BY dept ASC, name DESC: dept 1 first (bob then charlie because DESC), then dept 2
    let v = scalar(
        "SELECT GROUP_CONCAT(name ORDER BY dept ASC, name DESC SEPARATOR ',') FROM emp",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("charlie,bob,dave,alice".into()));
}

// ── DISTINCT ──────────────────────────────────────────────────────────────────

#[test]
fn group_concat_distinct() {
    let (mut st, mut txn) = setup();
    exec_ok("CREATE TABLE tags (tag TEXT)", &mut st, &mut txn);
    exec_ok("INSERT INTO tags VALUES ('rust')", &mut st, &mut txn);
    exec_ok("INSERT INTO tags VALUES ('db')", &mut st, &mut txn);
    exec_ok("INSERT INTO tags VALUES ('rust')", &mut st, &mut txn); // duplicate
    exec_ok("INSERT INTO tags VALUES ('async')", &mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(DISTINCT tag ORDER BY tag ASC) FROM tags",
        &mut st,
        &mut txn,
    );
    // 'rust' should appear only once.
    assert_eq!(v, Value::Text("async,db,rust".into()));
}

#[test]
fn group_concat_distinct_all_same_returns_single() {
    let (mut st, mut txn) = setup();
    exec_ok("CREATE TABLE t (v TEXT)", &mut st, &mut txn);
    exec_ok("INSERT INTO t VALUES ('x')", &mut st, &mut txn);
    exec_ok("INSERT INTO t VALUES ('x')", &mut st, &mut txn);
    exec_ok("INSERT INTO t VALUES ('x')", &mut st, &mut txn);

    let v = scalar("SELECT GROUP_CONCAT(DISTINCT v) FROM t", &mut st, &mut txn);
    assert_eq!(v, Value::Text("x".into()));
}

#[test]
fn group_concat_distinct_with_order_by() {
    let (mut st, mut txn) = setup();
    exec_ok("CREATE TABLE t2 (v TEXT)", &mut st, &mut txn);
    exec_ok("INSERT INTO t2 VALUES ('c')", &mut st, &mut txn);
    exec_ok("INSERT INTO t2 VALUES ('a')", &mut st, &mut txn);
    exec_ok("INSERT INTO t2 VALUES ('b')", &mut st, &mut txn);
    exec_ok("INSERT INTO t2 VALUES ('a')", &mut st, &mut txn); // dup

    let v = scalar(
        "SELECT GROUP_CONCAT(DISTINCT v ORDER BY v ASC) FROM t2",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("a,b,c".into()));
}

// ── string_agg alias ──────────────────────────────────────────────────────────

#[test]
fn string_agg_alias() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    let v = scalar(
        "SELECT string_agg(tag, ', ') FROM post_tags WHERE post_id = 2",
        &mut st,
        &mut txn,
    );
    // post_id=2 has 'rust' and 'web'. Order is accumulation order.
    match v {
        Value::Text(s) => {
            assert!(s.contains("rust"), "missing rust in: {s}");
            assert!(s.contains("web"), "missing web in: {s}");
            assert!(s.contains(", "), "wrong separator in: {s}");
        }
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── GROUP BY query ────────────────────────────────────────────────────────────

#[test]
fn group_concat_in_group_by_query() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    let rows = all_rows(
        "SELECT post_id, GROUP_CONCAT(tag ORDER BY tag ASC) FROM post_tags GROUP BY post_id ORDER BY post_id ASC",
        &mut st,
        &mut txn,
    );

    // post_id=1: async,db,rust; post_id=2: rust,web; post_id=3: NULL
    assert_eq!(rows.len(), 3);

    // post_id=1
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Text("async,db,rust".into()));

    // post_id=2
    assert_eq!(rows[1][0], Value::Int(2));
    assert_eq!(rows[1][1], Value::Text("rust,web".into()));

    // post_id=3 (all NULLs)
    assert_eq!(rows[2][0], Value::Int(3));
    assert_eq!(rows[2][1], Value::Null);
}

// ── Ungrouped query (implicit single group) ───────────────────────────────────

#[test]
fn group_concat_ungrouped_single_group() {
    let (mut st, mut txn) = setup();
    exec_ok("CREATE TABLE users (name TEXT)", &mut st, &mut txn);
    exec_ok("INSERT INTO users VALUES ('alice')", &mut st, &mut txn);
    exec_ok("INSERT INTO users VALUES ('bob')", &mut st, &mut txn);
    exec_ok("INSERT INTO users VALUES ('carol')", &mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(name ORDER BY name ASC SEPARATOR ', ') FROM users",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("alice, bob, carol".into()));
}

// ── HAVING with GROUP_CONCAT ──────────────────────────────────────────────────

#[test]
fn group_concat_in_having() {
    let (mut st, mut txn) = setup();
    setup_post_tags(&mut st, &mut txn);

    // Only return groups whose concatenated tags contain 'rust'.
    let rows = all_rows(
        "SELECT post_id, GROUP_CONCAT(tag ORDER BY tag ASC) \
         FROM post_tags \
         GROUP BY post_id \
         HAVING GROUP_CONCAT(tag ORDER BY tag ASC) LIKE '%rust%' \
         ORDER BY post_id ASC",
        &mut st,
        &mut txn,
    );

    // post_id=1 and post_id=2 have 'rust'; post_id=3 has all NULLs → HAVING NULL → excluded.
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Int(2));
}

// ── Integer values coerced to text ────────────────────────────────────────────

#[test]
fn group_concat_integer_column() {
    let (mut st, mut txn) = setup();
    exec_ok("CREATE TABLE nums (n INT)", &mut st, &mut txn);
    exec_ok("INSERT INTO nums VALUES (3)", &mut st, &mut txn);
    exec_ok("INSERT INTO nums VALUES (1)", &mut st, &mut txn);
    exec_ok("INSERT INTO nums VALUES (2)", &mut st, &mut txn);

    let v = scalar(
        "SELECT GROUP_CONCAT(n ORDER BY n ASC) FROM nums",
        &mut st,
        &mut txn,
    );
    assert_eq!(v, Value::Text("1,2,3".into()));
}
