/// Correctness verification for the ClusteredInsertBatch fast path.
///
/// These tests confirm that the catalog-probe optimizations introduced in
/// Phase 40 (insert_heap_ctx.rs fast path + trigger.rs cache read) do not
/// silently drop rows, corrupt values, or break rollback semantics.
///
/// Strategy: insert rows with fully deterministic values derived from the row
/// index, then verify every observable aggregate and spot-checked value
/// against its pre-computed expected result.
mod common;

use axiomdb_sql::QueryResult;
use axiomdb_types::Value;
use common::{rows, run_ctx, setup_ctx};

// ── helpers ──────────────────────────────────────────────────────────────────

fn ok(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> QueryResult {
    run_ctx(sql, s, txn, bloom, ctx).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn scalar_int(result: QueryResult) -> i64 {
    match rows(result)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
    {
        Some(Value::Int(v)) => v as i64,
        Some(Value::BigInt(v)) => v,
        other => panic!("expected integer scalar, got {other:?}"),
    }
}

fn scalar_real(result: QueryResult) -> f64 {
    match rows(result)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
    {
        Some(Value::Real(v)) => v,
        other => panic!("expected real scalar, got {other:?}"),
    }
}

/// Generate the INSERT SQL for row `i` (1-based), same formula as the bench.
fn insert_sql(i: usize) -> String {
    let active = if i.is_multiple_of(2) { "TRUE" } else { "FALSE" };
    format!(
        "INSERT INTO bench_users VALUES ({i}, 'user_{i:06}', {age}, {active}, {score:.1}, 'u{i}@b.local')",
        age   = 18 + (i % 62),
        score = 100.0 + (i % 1000) as f64 * 0.1,
    )
}

/// Create the bench_users table (same schema as the benchmark).
fn create_table(
    s: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) {
    ok(
        "CREATE TABLE bench_users (
            id     INT  NOT NULL,
            name   TEXT NOT NULL,
            age    INT  NOT NULL,
            active BOOL NOT NULL,
            score  REAL NOT NULL,
            email  TEXT NOT NULL,
            PRIMARY KEY (id)
        )",
        s,
        txn,
        bloom,
        ctx,
    );
}

// ── tests ─────────────────────────────────────────────────────────────────────

/// Insert N rows in one explicit transaction and verify every aggregate.
///
/// If the fast path silently dropped rows, COUNT(*) would be < N.
/// If it corrupted values, SUM/MIN/MAX would deviate from the formula.
#[test]
fn insert_batch_count_and_aggregates_are_exact() {
    const N: usize = 5_000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    create_table(&mut s, &mut txn, &mut bloom, &mut ctx);

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=N {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);
    }
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    // COUNT(*) must be exactly N.
    let count = scalar_int(ok(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(count, N as i64, "COUNT(*) mismatch after COMMIT");

    // SUM(id) = N*(N+1)/2 (Gauss formula).
    let expected_sum = (N as i64) * (N as i64 + 1) / 2;
    let sum_id = scalar_int(ok(
        "SELECT SUM(id) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(sum_id, expected_sum, "SUM(id) mismatch");

    // MIN(age): age = 18 + (i % 62). For i=62, age=18 → minimum.
    let min_age = scalar_int(ok(
        "SELECT MIN(age) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(min_age, 18, "MIN(age) should be 18");

    // MAX(age): age = 18 + 61 = 79.
    let max_age = scalar_int(ok(
        "SELECT MAX(age) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(max_age, 79, "MAX(age) should be 79");

    // Even/odd split: active=TRUE for even ids.
    let active_count = scalar_int(ok(
        "SELECT COUNT(*) FROM bench_users WHERE active = TRUE",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(active_count, (N / 2) as i64, "active count should be N/2");

    // MIN(score): score = 100.0 + (i % 1000)*0.1. For i=1000, score=100.0 → minimum.
    let min_score = scalar_real(ok(
        "SELECT MIN(score) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert!(
        (min_score - 100.0).abs() < 0.001,
        "MIN(score) should be ~100.0, got {min_score}"
    );

    // MAX(score): i=999 → 100.0 + 999*0.1 = 199.9.
    let max_score = scalar_real(ok(
        "SELECT MAX(score) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert!(
        (max_score - 199.9).abs() < 0.001,
        "MAX(score) should be ~199.9, got {max_score}"
    );
}

/// Spot-check specific rows to verify values are not scrambled.
///
/// If the batch incorrectly mapped value positions or silently swapped rows,
/// point-lookup values would not match the formula for that id.
#[test]
fn insert_batch_spot_check_row_values() {
    const N: usize = 2_000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    create_table(&mut s, &mut txn, &mut bloom, &mut ctx);

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=N {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);
    }
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Spot-check ids: 1, 100, 500, 999, 1000, 1001, 1999.
    for &id in &[1usize, 100, 500, 999, 1000, 1001, 1999] {
        let result = ok(
            &format!("SELECT id, name, age, active, score, email FROM bench_users WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
        let r = rows(result);
        assert_eq!(r.len(), 1, "expected 1 row for id={id}");
        let row = &r[0];

        assert_eq!(row[0], Value::Int(id as i32), "id mismatch at id={id}");
        assert_eq!(
            row[1],
            Value::Text(format!("user_{id:06}")),
            "name mismatch at id={id}"
        );
        assert_eq!(
            row[2],
            Value::Int((18 + id % 62) as i32),
            "age mismatch at id={id}"
        );
        assert_eq!(
            row[3],
            Value::Bool(id % 2 == 0),
            "active mismatch at id={id}"
        );

        let expected_score = 100.0 + (id % 1000) as f64 * 0.1;
        if let Value::Real(got) = row[4] {
            assert!(
                (got - expected_score).abs() < 0.001,
                "score mismatch at id={id}: expected {expected_score}, got {got}"
            );
        } else {
            panic!("expected Real for score at id={id}, got {:?}", row[4]);
        }

        assert_eq!(
            row[5],
            Value::Text(format!("u{id}@b.local")),
            "email mismatch at id={id}"
        );
    }
}

/// ROLLBACK must leave zero rows — the batch discard path must not persist data.
#[test]
fn insert_batch_rollback_leaves_zero_rows() {
    const N: usize = 3_000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    create_table(&mut s, &mut txn, &mut bloom, &mut ctx);

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=N {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);
    }
    ok("ROLLBACK", &mut s, &mut txn, &mut bloom, &mut ctx);

    let count = scalar_int(ok(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(count, 0, "ROLLBACK must leave 0 rows, got {count}");
}

/// ROLLBACK TO SAVEPOINT must preserve only rows inserted before the savepoint.
#[test]
fn insert_batch_rollback_to_savepoint_preserves_pre_savepoint_rows() {
    const BEFORE: usize = 1_000;
    const AFTER: usize = 500;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    create_table(&mut s, &mut txn, &mut bloom, &mut ctx);

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Insert BEFORE rows, create savepoint.
    for i in 1..=BEFORE {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);
    }
    ok("SAVEPOINT sp", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Insert AFTER more rows, then roll back to savepoint.
    for i in (BEFORE + 1)..=(BEFORE + AFTER) {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);
    }
    ok(
        "ROLLBACK TO SAVEPOINT sp",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Only BEFORE rows must survive.
    let count = scalar_int(ok(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        count, BEFORE as i64,
        "expected {BEFORE} rows after rollback to savepoint, got {count}"
    );

    // The last pre-savepoint row must exist.
    let last = scalar_int(ok(
        "SELECT MAX(id) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        last, BEFORE as i64,
        "MAX(id) should be {BEFORE}, got {last}"
    );

    // No post-savepoint row must exist.
    let beyond = scalar_int(ok(
        &format!("SELECT COUNT(*) FROM bench_users WHERE id > {BEFORE}"),
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(beyond, 0, "rows beyond savepoint must not exist");
}

/// Commit, then insert more in a second transaction and commit again.
/// Both batches must be visible and correct.
#[test]
fn insert_batch_two_consecutive_transactions_accumulate_correctly() {
    const FIRST: usize = 2_000;
    const SECOND: usize = 1_000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    create_table(&mut s, &mut txn, &mut bloom, &mut ctx);

    // First transaction.
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    for i in 1..=FIRST {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);
    }
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Second transaction with different rows (higher ids).
    let offset = FIRST;
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    for i in (offset + 1)..=(offset + SECOND) {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);
    }
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    let total = FIRST + SECOND;
    let count = scalar_int(ok(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        count, total as i64,
        "total count after two txns should be {total}"
    );

    let expected_sum = (total as i64) * (total as i64 + 1) / 2;
    let sum_id = scalar_int(ok(
        "SELECT SUM(id) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        sum_id, expected_sum,
        "SUM(id) after two txns should be {expected_sum}"
    );
}

/// Mix of tables in the same transaction: verify the fast path does not
/// confuse rows between tables when the table changes mid-transaction.
#[test]
fn insert_batch_different_tables_in_same_txn_do_not_cross_contaminate() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE t1 (id INT PRIMARY KEY, val INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE TABLE t2 (id INT PRIMARY KEY, val INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    // Insert into t1, then t2, then back to t1.
    for i in 1..=500i32 {
        ok(
            &format!("INSERT INTO t1 VALUES ({i}, {})", i * 10),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    for i in 1..=300i32 {
        ok(
            &format!("INSERT INTO t2 VALUES ({i}, {})", i * 20),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    for i in 501..=600i32 {
        ok(
            &format!("INSERT INTO t1 VALUES ({i}, {})", i * 10),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    ok("COMMIT", &mut s, &mut txn, &mut bloom, &mut ctx);

    let c1 = scalar_int(ok(
        "SELECT COUNT(*) FROM t1",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let c2 = scalar_int(ok(
        "SELECT COUNT(*) FROM t2",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(c1, 600, "t1 must have 600 rows, got {c1}");
    assert_eq!(c2, 300, "t2 must have 300 rows, got {c2}");

    // Sum check for t1: (1..=600) * 10
    let sum1 = scalar_int(ok(
        "SELECT SUM(val) FROM t1",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected1: i64 = (1..=600i64).map(|i| i * 10).sum();
    assert_eq!(sum1, expected1, "SUM(val) for t1 mismatch");

    // Sum check for t2: (1..=300) * 20
    let sum2 = scalar_int(ok(
        "SELECT SUM(val) FROM t2",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected2: i64 = (1..=300i64).map(|i| i * 20).sum();
    assert_eq!(sum2, expected2, "SUM(val) for t2 mismatch");
}
