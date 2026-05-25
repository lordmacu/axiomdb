//! Integration tests for G5 DML extensions.
//!
//! Covers:
//!   G5.1 — CALL / DO as no-ops
//!   G5.2 — DELETE ORDER BY + LIMIT
//!   G5.3 — UPDATE ORDER BY + LIMIT
//!   G5.4 — INSERT IGNORE (heap + clustered, VALUES + SELECT)
//!   G5.5 — CREATE TABLE LIKE
//!   G5.6 — CREATE TABLE AS SELECT (CTAS)

mod common;
use axiomdb_types::Value;

// ── helpers ───────────────────────────────────────────────────────────────────

fn ok(
    sql: &str,
    storage: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
) -> axiomdb_sql::QueryResult {
    common::run(sql, storage, txn)
}

fn rows_of(r: axiomdb_sql::QueryResult) -> Vec<Vec<Value>> {
    common::rows(r)
}

fn affected(r: axiomdb_sql::QueryResult) -> u64 {
    common::affected_count(r)
}

// ─── G5.1 — DO as no-op ──────────────────────────────────────────────────────
// (CALL is no longer a no-op: it executes a stored procedure as of Phase 16.7;
//  an unknown procedure errors. See integration_stored_procedures.rs.)

#[test]
fn test_do_noop() {
    let (mut s, mut t) = common::setup();
    let res = ok("DO SLEEP(0)", &mut s, &mut t);
    assert!(matches!(res, axiomdb_sql::QueryResult::Empty));
}

// ─── G5.2 — DELETE ORDER BY + LIMIT ─────────────────────────────────────────

#[test]
fn test_delete_order_by_limit_heap() {
    let (mut s, mut t) = common::setup();
    ok("CREATE TABLE scores (id INT, val INT)", &mut s, &mut t);
    ok(
        "INSERT INTO scores VALUES (1, 30), (2, 10), (3, 20), (4, 40), (5, 5)",
        &mut s,
        &mut t,
    );

    // Delete the 2 rows with the lowest val.
    let n = affected(ok(
        "DELETE FROM scores ORDER BY val ASC LIMIT 2",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 2);

    let r = rows_of(ok("SELECT val FROM scores ORDER BY val", &mut s, &mut t));
    // vals 5 and 10 deleted, remaining: 20, 30, 40
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(20));
    assert_eq!(r[1][0], Value::Int(30));
    assert_eq!(r[2][0], Value::Int(40));
}

#[test]
fn test_delete_limit_without_order_by() {
    let (mut s, mut t) = common::setup();
    ok("CREATE TABLE t (id INT)", &mut s, &mut t);
    ok("INSERT INTO t VALUES (1),(2),(3),(4),(5)", &mut s, &mut t);
    let n = affected(ok("DELETE FROM t LIMIT 3", &mut s, &mut t));
    assert_eq!(n, 3);
    let r = rows_of(ok("SELECT COUNT(*) FROM t", &mut s, &mut t));
    assert_eq!(r[0][0], Value::BigInt(2));
}

#[test]
fn test_delete_order_by_limit_clustered() {
    let (mut s, mut t) = common::setup();
    ok(
        "CREATE TABLE cpri (id INT PRIMARY KEY, val INT)",
        &mut s,
        &mut t,
    );
    ok(
        "INSERT INTO cpri VALUES (1,100),(2,50),(3,200),(4,10),(5,75)",
        &mut s,
        &mut t,
    );

    // Delete the 3 rows with the highest val.
    let n = affected(ok(
        "DELETE FROM cpri ORDER BY val DESC LIMIT 3",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 3);

    let r = rows_of(ok("SELECT id FROM cpri ORDER BY id", &mut s, &mut t));
    // vals 200, 100, 75 deleted → ids 2 and 4 remain (val 50 and 10)
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(2));
    assert_eq!(r[1][0], Value::Int(4));
}

// ─── G5.3 — UPDATE ORDER BY + LIMIT ─────────────────────────────────────────

#[test]
fn test_update_order_by_limit_heap() {
    let (mut s, mut t) = common::setup();
    ok("CREATE TABLE prices (id INT, price INT)", &mut s, &mut t);
    ok(
        "INSERT INTO prices VALUES (1,100),(2,200),(3,300),(4,400)",
        &mut s,
        &mut t,
    );

    // Increase price for the 2 cheapest rows.
    let n = affected(ok(
        "UPDATE prices SET price = price + 1000 ORDER BY price ASC LIMIT 2",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 2);

    let r = rows_of(ok(
        "SELECT price FROM prices ORDER BY price",
        &mut s,
        &mut t,
    ));
    assert_eq!(r[0][0], Value::Int(300));
    assert_eq!(r[1][0], Value::Int(400));
    assert_eq!(r[2][0], Value::Int(1100));
    assert_eq!(r[3][0], Value::Int(1200));
}

#[test]
fn test_update_limit_without_order_by() {
    let (mut s, mut t) = common::setup();
    ok("CREATE TABLE t2 (id INT, flag INT)", &mut s, &mut t);
    ok(
        "INSERT INTO t2 VALUES (1,0),(2,0),(3,0),(4,0)",
        &mut s,
        &mut t,
    );
    let n = affected(ok("UPDATE t2 SET flag = 1 LIMIT 2", &mut s, &mut t));
    assert_eq!(n, 2);
    let r = rows_of(ok("SELECT COUNT(*) FROM t2 WHERE flag = 1", &mut s, &mut t));
    assert_eq!(r[0][0], Value::BigInt(2));
}

#[test]
fn test_update_order_by_limit_clustered() {
    let (mut s, mut t) = common::setup();
    ok(
        "CREATE TABLE cpr2 (id INT PRIMARY KEY, score INT)",
        &mut s,
        &mut t,
    );
    ok(
        "INSERT INTO cpr2 VALUES (1,10),(2,20),(3,30),(4,40),(5,50)",
        &mut s,
        &mut t,
    );

    // Zero out the 3 highest-scoring rows.
    let n = affected(ok(
        "UPDATE cpr2 SET score = 0 ORDER BY score DESC LIMIT 3",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 3);

    let r = rows_of(ok("SELECT score FROM cpr2 ORDER BY id", &mut s, &mut t));
    assert_eq!(r[0][0], Value::Int(10));
    assert_eq!(r[1][0], Value::Int(20));
    assert_eq!(r[2][0], Value::Int(0));
    assert_eq!(r[3][0], Value::Int(0));
    assert_eq!(r[4][0], Value::Int(0));
}

// ─── G5.4 — INSERT IGNORE ────────────────────────────────────────────────────

#[test]
fn test_insert_ignore_heap_unique_violation() {
    let (mut s, mut t) = common::setup();
    ok(
        "CREATE TABLE uniq_t (id INT UNIQUE, val INT)",
        &mut s,
        &mut t,
    );
    ok("INSERT INTO uniq_t VALUES (1, 10)", &mut s, &mut t);

    // Second row (id=1) is a duplicate — must be silently skipped.
    let n = affected(ok(
        "INSERT IGNORE INTO uniq_t VALUES (1, 20), (2, 30)",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 1); // only (2,30) inserted

    let r = rows_of(ok("SELECT val FROM uniq_t ORDER BY id", &mut s, &mut t));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(10));
    assert_eq!(r[1][0], Value::Int(30));
}

#[test]
fn test_insert_ignore_clustered_pk_duplicate() {
    let (mut s, mut t) = common::setup();
    ok(
        "CREATE TABLE cpk (id INT PRIMARY KEY, name TEXT)",
        &mut s,
        &mut t,
    );
    ok("INSERT INTO cpk VALUES (1, 'Alice')", &mut s, &mut t);

    // Row with id=1 already exists.
    let n = affected(ok(
        "INSERT IGNORE INTO cpk VALUES (1, 'Bob'), (2, 'Carol')",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 1); // only (2,'Carol') inserted

    let r = rows_of(ok("SELECT name FROM cpk ORDER BY id", &mut s, &mut t));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("Alice".to_string()));
    assert_eq!(r[1][0], Value::Text("Carol".to_string()));
}

#[test]
fn test_insert_ignore_no_duplicates_inserts_all() {
    let (mut s, mut t) = common::setup();
    ok(
        "CREATE TABLE cpk2 (id INT PRIMARY KEY, v INT)",
        &mut s,
        &mut t,
    );
    let n = affected(ok(
        "INSERT IGNORE INTO cpk2 VALUES (1,1),(2,2),(3,3)",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 3);
    let r = rows_of(ok("SELECT COUNT(*) FROM cpk2", &mut s, &mut t));
    assert_eq!(r[0][0], Value::BigInt(3));
}

#[test]
fn test_insert_ignore_via_select() {
    let (mut s, mut t) = common::setup();
    ok("CREATE TABLE src (id INT, v INT)", &mut s, &mut t);
    ok(
        "INSERT INTO src VALUES (1,10),(2,20),(3,30)",
        &mut s,
        &mut t,
    );
    ok("CREATE TABLE dst (id INT UNIQUE, v INT)", &mut s, &mut t);
    ok("INSERT INTO dst VALUES (2, 99)", &mut s, &mut t); // id=2 pre-exists

    // INSERT IGNORE SELECT: id=2 row must be skipped.
    let n = affected(ok(
        "INSERT IGNORE INTO dst SELECT id, v FROM src",
        &mut s,
        &mut t,
    ));
    assert_eq!(n, 2);

    let r = rows_of(ok("SELECT id FROM dst ORDER BY id", &mut s, &mut t));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(3));
    // id=2 value stays 99 (not overwritten)
    let r2 = rows_of(ok("SELECT v FROM dst WHERE id = 2", &mut s, &mut t));
    assert_eq!(r2[0][0], Value::Int(99));
}

// ─── G5.5 — CREATE TABLE LIKE ────────────────────────────────────────────────

#[test]
fn test_create_table_like_heap() {
    let (mut s, mut t) = common::setup();
    ok(
        "CREATE TABLE orig (id INT, name TEXT, score FLOAT)",
        &mut s,
        &mut t,
    );
    ok("CREATE TABLE copy LIKE orig", &mut s, &mut t);

    // Insert into copy — schema must be identical.
    let n = affected(ok("INSERT INTO copy VALUES (1, 'x', 3.14)", &mut s, &mut t));
    assert_eq!(n, 1);

    let r = rows_of(ok("SELECT id, name FROM copy", &mut s, &mut t));
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[0][1], Value::Text("x".to_string()));
}

#[test]
fn test_create_table_like_clustered() {
    let (mut s, mut t) = common::setup();
    ok(
        "CREATE TABLE orig_c (id INT PRIMARY KEY, val INT)",
        &mut s,
        &mut t,
    );
    ok("INSERT INTO orig_c VALUES (1, 100)", &mut s, &mut t);
    ok("CREATE TABLE copy_c LIKE orig_c", &mut s, &mut t);

    // copy_c is a fresh empty clustered table — original rows not copied.
    let r = rows_of(ok("SELECT COUNT(*) FROM copy_c", &mut s, &mut t));
    assert_eq!(r[0][0], Value::BigInt(0));

    // Must be able to insert with the same PK.
    let n = affected(ok("INSERT INTO copy_c VALUES (1, 200)", &mut s, &mut t));
    assert_eq!(n, 1);

    let r2 = rows_of(ok("SELECT val FROM copy_c WHERE id = 1", &mut s, &mut t));
    assert_eq!(r2[0][0], Value::Int(200));
}

#[test]
fn test_create_table_like_if_not_exists() {
    let (mut s, mut t) = common::setup();
    ok("CREATE TABLE src_t (a INT)", &mut s, &mut t);
    ok("CREATE TABLE dst_t LIKE src_t", &mut s, &mut t);
    // Should not error because of IF NOT EXISTS.
    ok(
        "CREATE TABLE IF NOT EXISTS dst_t LIKE src_t",
        &mut s,
        &mut t,
    );
}

// ─── G5.6 — CREATE TABLE AS SELECT (CTAS) ───────────────────────────────────

#[test]
fn test_ctas_basic() {
    let (mut s, mut t, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TABLE items (id INT, price INT)",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO items VALUES (1, 100), (2, 200), (3, 300)",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    common::run_ctx(
        "CREATE TABLE cheap AS SELECT id, price FROM items WHERE price < 250",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = common::run_ctx(
        "SELECT COUNT(*) FROM cheap",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let rows = common::rows(r);
    assert_eq!(rows[0][0], Value::BigInt(2));
}

#[test]
fn test_ctas_preserves_column_names() {
    let (mut s, mut t, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TABLE base (x INT, y TEXT)",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "INSERT INTO base VALUES (1, 'hello')",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TABLE derived AS SELECT x, y FROM base",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Can select by column name from derived.
    let r = common::run_ctx(
        "SELECT x, y FROM derived",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let rows = common::rows(r);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Text("hello".to_string()));
}

#[test]
fn test_ctas_empty_select() {
    let (mut s, mut t, mut bloom, mut ctx) = common::setup_ctx();

    common::run_ctx(
        "CREATE TABLE mt (id INT)",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    common::run_ctx(
        "CREATE TABLE mt_copy AS SELECT id FROM mt",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    let r = common::run_ctx(
        "SELECT COUNT(*) FROM mt_copy",
        &mut s,
        &mut t,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    let rows = common::rows(r);
    assert_eq!(rows[0][0], Value::BigInt(0));
}
