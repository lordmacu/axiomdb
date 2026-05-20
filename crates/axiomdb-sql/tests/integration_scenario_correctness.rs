/// Correctness verification for all pure-SQL benchmark scenarios.
///
/// Each test replicates the exact workload of its corresponding benchmark
/// scenario and then verifies that the resulting data matches the deterministic
/// formula used to generate it — not just that the operation ran without error.
///
/// Data formula (mirrors gen_inserts in axiomdb_bench):
///   id     = i  (1-based)
///   name   = format!("user_{i:06}")
///   age    = 18 + (i % 62)                   → [18, 79]
///   active = i % 2 == 0                       → TRUE for even ids
///   score  = 100.0 + (i % 1000) as f64 * 0.1 → [100.0, 199.9]
///   email  = format!("u{i}@b.local")
mod common;

use axiomdb_sql::QueryResult;
use axiomdb_types::Value;
use common::{rows, run_ctx, setup_ctx};

// ── shared helpers ────────────────────────────────────────────────────────────

fn ok(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> QueryResult {
    run_ctx(sql, s, txn, bloom, ctx).unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

fn scalar_int(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> i64 {
    match rows(ok(sql, s, txn, bloom, ctx))
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
    {
        Some(Value::Int(v)) => v as i64,
        Some(Value::BigInt(v)) => v,
        other => panic!("expected integer scalar from '{sql}', got {other:?}"),
    }
}

fn insert_sql(i: usize) -> String {
    let active = if i % 2 == 0 { "TRUE" } else { "FALSE" };
    format!(
        "INSERT INTO bench_users VALUES ({i}, 'user_{i:06}', {age}, {active}, {score:.1}, 'u{i}@b.local')",
        age   = 18 + (i % 62),
        score = 100.0 + (i % 1000) as f64 * 0.1,
    )
}

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

fn load_batch(
    n: usize,
    s: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) {
    create_table(s, txn, bloom, ctx);
    ok("BEGIN", s, txn, bloom, ctx);
    for i in 1..=n {
        ok(&insert_sql(i), s, txn, bloom, ctx);
    }
    ok("COMMIT", s, txn, bloom, ctx);
}

fn check_row(row: &[Value], id: usize) {
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
    match row[4] {
        Value::Real(got) => assert!(
            (got - expected_score).abs() < 0.001,
            "score mismatch at id={id}: expected {expected_score}, got {got}",
        ),
        ref other => panic!("expected Real for score at id={id}, got {other:?}"),
    }
    assert_eq!(
        row[5],
        Value::Text(format!("u{id}@b.local")),
        "email mismatch at id={id}"
    );
}

// ── 1. full_scan ─────────────────────────────────────────────────────────────

/// full_scan scenario: SELECT * returns all N rows, every row correct.
///
/// Detects: truncated scans, rows returned with wrong column order or values,
/// phantom rows, rows missing from heap.
#[test]
fn full_scan_returns_all_rows_with_correct_values() {
    const N: usize = 500;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    let result = rows(ok(
        "SELECT id, name, age, active, score, email FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(result.len(), N, "full scan must return exactly {N} rows");

    for row in &result {
        let id = match row[0] {
            Value::Int(v) => v as usize,
            ref other => panic!("id column not Int: {other:?}"),
        };
        assert!(id >= 1 && id <= N, "id {id} out of range [1, {N}]");
        check_row(row, id);
    }

    // All ages must be in formula range.
    for row in &result {
        let age = match row[2] {
            Value::Int(v) => v,
            ref o => panic!("age not Int: {o:?}"),
        };
        assert!(age >= 18 && age <= 79, "age {age} out of range [18, 79]");
    }
}

// ── 2. select_where ──────────────────────────────────────────────────────────

/// select_where scenario: WHERE active = TRUE returns exactly N/2 rows, all
/// with even ids and correct values.
///
/// Detects: predicate not applied, wrong rows included, values corrupted by
/// the filter path.
#[test]
fn select_where_active_true_returns_only_even_id_rows() {
    const N: usize = 1000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    let result = rows(ok(
        "SELECT id, name, age, active, score, email FROM bench_users WHERE active = TRUE",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        result.len(),
        N / 2,
        "select_where must return exactly N/2={} rows",
        N / 2
    );

    for row in &result {
        assert_eq!(
            row[3],
            Value::Bool(true),
            "all returned rows must have active = TRUE"
        );
        let id = match row[0] {
            Value::Int(v) => v as usize,
            ref o => panic!("id not Int: {o:?}"),
        };
        assert_eq!(
            id % 2,
            0,
            "all active=TRUE rows must have even id, got id={id}"
        );
        check_row(row, id);
    }

    // COUNT via scan must agree.
    let count = scalar_int(
        "SELECT COUNT(*) FROM bench_users WHERE active = TRUE",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(count, (N / 2) as i64);
}

// ── 3. point_lookup ──────────────────────────────────────────────────────────

/// point_lookup scenario: 100 single-row lookups by PK, each returns exactly
/// the right row with all correct values.
///
/// Detects: wrong PK range match, off-by-one in B-tree traversal, column
/// scramble on single-row result.
#[test]
fn point_lookup_returns_exact_row_for_each_sampled_id() {
    const N: usize = 2000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    // 100 evenly-spaced ids across [1, N].
    let step = N / 100;
    for id in (1..=N).step_by(step).take(100) {
        let result = rows(ok(
            &format!("SELECT id, name, age, active, score, email FROM bench_users WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(
            result.len(),
            1,
            "point lookup id={id} must return exactly 1 row"
        );
        check_row(&result[0], id);
    }

    // Boundary checks.
    for &id in &[1usize, N] {
        let result = rows(ok(
            &format!("SELECT id, name, age, active, score, email FROM bench_users WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(result.len(), 1, "boundary id={id} must exist");
        check_row(&result[0], id);
    }

    // Non-existent id must return 0 rows.
    let ghost = rows(ok(
        "SELECT id FROM bench_users WHERE id = 99999",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(ghost.len(), 0, "non-existent id must return 0 rows");
}

// ── 4. range_scan ────────────────────────────────────────────────────────────

/// range_scan scenario: WHERE id >= start AND id < end returns the exact
/// subset with correct values.  Boundary ids must not leak.
///
/// Detects: off-by-one in range predicate, wrong row count, value corruption
/// in middle of B-tree leaf chain.
#[test]
fn range_scan_returns_correct_subset_and_respects_boundaries() {
    const N: usize = 1000;
    let start = N / 4; // 250
    let end = start + N / 10; // 350
    let expected_count = end - start; // 100

    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    let result = rows(ok(
        &format!(
            "SELECT id, name, age, active, score, email \
             FROM bench_users WHERE id >= {start} AND id < {end}"
        ),
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        result.len(),
        expected_count,
        "range scan [{start}, {end}) must return exactly {expected_count} rows"
    );

    for row in &result {
        let id = match row[0] {
            Value::Int(v) => v as usize,
            ref o => panic!("id not Int: {o:?}"),
        };
        assert!(
            id >= start && id < end,
            "id {id} is outside expected range [{start}, {end})"
        );
        check_row(row, id);
    }

    // Boundary id=start-1 must not appear.
    let below = rows(ok(
        &format!("SELECT id FROM bench_users WHERE id = {}", start - 1),
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // start-1 IS in the table (id=249 was loaded), just not in the range result.
    // The range result must not contain it.
    let ids_in_result: Vec<i32> = result
        .iter()
        .map(|r| match r[0] {
            Value::Int(v) => v,
            _ => panic!(),
        })
        .collect();
    assert!(
        !ids_in_result.contains(&((start - 1) as i32)),
        "id={} must not appear in range result",
        start - 1
    );
    assert_eq!(below.len(), 1, "id={} must exist in table", start - 1);

    // Boundary id=end must not appear in range result.
    assert!(
        !ids_in_result.contains(&(end as i32)),
        "id={end} must not appear in range result"
    );
}

// ── 5. count_star ────────────────────────────────────────────────────────────

/// count_star scenario: COUNT(*) must equal the number of loaded rows.
/// After a partial DELETE, COUNT(*) must update correctly.
///
/// Detects: over-counting phantom rows, under-counting due to visibility bug,
/// stale count cache.
#[test]
fn count_star_matches_load_and_updates_after_delete() {
    const N: usize = 1000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    let count = scalar_int(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(count, N as i64, "COUNT(*) must equal {N} after batch load");

    // Delete 100 rows with even ids (active=TRUE, id 2,4,...,200).
    ok(
        "DELETE FROM bench_users WHERE id <= 200 AND active = TRUE",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let after = scalar_int(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    // Deleted: ids 2,4,...,200 = 100 rows.
    assert_eq!(
        after,
        (N - 100) as i64,
        "COUNT(*) must be {} after deleting 100 rows",
        N - 100
    );

    // COUNT of active=TRUE rows must also have dropped by 100.
    let active_after = scalar_int(
        "SELECT COUNT(*) FROM bench_users WHERE active = TRUE",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        active_after,
        (N / 2 - 100) as i64,
        "COUNT of active rows must be {}",
        N / 2 - 100
    );
}

// ── 6. group_by ──────────────────────────────────────────────────────────────

/// group_by scenario: SELECT age, COUNT(*) GROUP BY age must produce exactly
/// 62 groups (ages 18-79), each with the correct count, summing to N.
///
/// Detects: groups collapsed, wrong group sizes, aggregate applied to wrong
/// partition.
#[test]
fn group_by_age_returns_62_groups_with_correct_counts() {
    // N=1240 = 62*20 so each age bucket is exactly 20 rows.
    const N: usize = 1240;
    const AGE_CYCLE: usize = 62;
    const ROWS_PER_GROUP: i64 = (N / AGE_CYCLE) as i64; // 20

    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    let result = rows(ok(
        "SELECT age, COUNT(*) FROM bench_users GROUP BY age",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        result.len(),
        AGE_CYCLE,
        "GROUP BY age must produce exactly {AGE_CYCLE} distinct groups"
    );

    let mut total: i64 = 0;
    for row in &result {
        let age = match row[0] {
            Value::Int(v) => v,
            ref o => panic!("age not Int: {o:?}"),
        };
        assert!(age >= 18 && age <= 79, "age {age} out of range [18, 79]");

        let cnt = match row[1] {
            Value::Int(v) => v as i64,
            Value::BigInt(v) => v,
            ref o => panic!("group count not int: {o:?}"),
        };
        assert_eq!(
            cnt, ROWS_PER_GROUP,
            "age group age={age} must have {ROWS_PER_GROUP} rows, got {cnt}"
        );
        total += cnt;
    }
    assert_eq!(total, N as i64, "sum of group counts must equal {N}");
}

// ── 7. crud_flow ─────────────────────────────────────────────────────────────

/// crud_flow scenario: batch insert → full scan → delete all.
///
/// Each phase is verified individually to catch bugs that only manifest during
/// the transition between phases (e.g. DELETE corrupting B-tree state that
/// SELECT then reads).
#[test]
fn crud_flow_insert_scan_delete_leaves_correct_state() {
    const N: usize = 500;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    // Phase 1: after batch insert, count = N.
    let count = scalar_int(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(count, N as i64, "count after insert batch must be {N}");

    // Phase 2: full scan returns all N rows with correct values.
    let all = rows(ok(
        "SELECT id, name, age, active, score, email FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(all.len(), N, "full scan must return {N} rows");

    // Spot-check 5 specific rows.
    for &id in &[1usize, 50, 100, 250, 500] {
        let row = all
            .iter()
            .find(|r| r[0] == Value::Int(id as i32))
            .unwrap_or_else(|| panic!("id={id} missing from full scan result"));
        check_row(row, id);
    }

    // Phase 3: DELETE all rows.
    ok(
        "DELETE FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    let after = scalar_int(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(after, 0, "DELETE all must leave 0 rows, got {after}");

    // Full scan after delete must return 0 rows (not panic on empty heap).
    let empty = rows(ok(
        "SELECT id, name, age, active, score, email FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(empty.len(), 0, "full scan after DELETE must return 0 rows");
}

// ── 8. insert_autocommit ─────────────────────────────────────────────────────

/// insert_autocommit scenario: each INSERT runs in its own autocommit txn.
/// Every row must be immediately visible after its insert, and all values
/// must be correct after all inserts complete.
///
/// Detects: epoch fast path serving stale data, autocommit rows not flushed to
/// the readable heap, count-by-one errors in the autocommit path.
#[test]
fn autocommit_insert_each_row_immediately_visible_and_correct() {
    const N: usize = 200;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    create_table(&mut s, &mut txn, &mut bloom, &mut ctx);

    for i in 1..=N {
        ok(&insert_sql(i), &mut s, &mut txn, &mut bloom, &mut ctx);

        // After each autocommit insert the new row must be visible.
        let count = scalar_int(
            "SELECT COUNT(*) FROM bench_users",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
        assert_eq!(
            count, i as i64,
            "after autocommit insert #{i}, count must be {i} (got {count})"
        );
    }

    // Spot-check final values.
    for &id in &[1, 50, 100, 150, 200] {
        let result = rows(ok(
            &format!("SELECT id, name, age, active, score, email FROM bench_users WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(
            result.len(),
            1,
            "id={id} must exist after all autocommit inserts"
        );
        check_row(&result[0], id);
    }

    // Global aggregates.
    let sum = scalar_int(
        "SELECT SUM(id) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(sum, (N as i64) * (N as i64 + 1) / 2, "SUM(id) mismatch");

    let min_age = scalar_int(
        "SELECT MIN(age) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let max_age = scalar_int(
        "SELECT MAX(age) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(min_age, 18, "MIN(age) must be 18");
    assert_eq!(max_age, 79, "MAX(age) must be 79");
}

// ── 9. create_index ──────────────────────────────────────────────────────────

/// create_index scenario: after CREATE INDEX the data is still correct and
/// index lookups by email return the exact expected row.
///
/// Detects: CREATE INDEX corrupting the heap, index pointing to wrong page,
/// index missing entries, COUNT(*) changing after index creation.
#[test]
fn create_index_data_unchanged_and_index_lookup_correct() {
    const N: usize = 1000;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    // Count before index creation.
    let before = scalar_int(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(before, N as i64);

    // Create the index.
    ok(
        "CREATE INDEX idx_email ON bench_users (email)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Count must be unchanged after CREATE INDEX.
    let after = scalar_int(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        after, N as i64,
        "COUNT(*) must not change after CREATE INDEX"
    );

    // Query specific rows via the email index.
    for &id in &[1usize, 42, 100, 500, 999, 1000] {
        let email = format!("u{id}@b.local");
        let result = rows(ok(
            &format!(
                "SELECT id, name, age, active, score, email \
                 FROM bench_users WHERE email = '{email}'"
            ),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(
            result.len(),
            1,
            "index lookup by email='{email}' must return exactly 1 row"
        );
        check_row(&result[0], id);
    }

    // Non-existent email must return 0 rows.
    let ghost = rows(ok(
        "SELECT id FROM bench_users WHERE email = 'ghost@b.local'",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(
        ghost.len(),
        0,
        "index lookup for non-existent email must return 0 rows"
    );

    // Aggregates must still be correct after index creation.
    let sum = scalar_int(
        "SELECT SUM(id) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let expected_sum = (N as i64) * (N as i64 + 1) / 2;
    assert_eq!(
        sum, expected_sum,
        "SUM(id) must be unchanged after CREATE INDEX"
    );

    let min_age = scalar_int(
        "SELECT MIN(age) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    let max_age = scalar_int(
        "SELECT MAX(age) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(min_age, 18);
    assert_eq!(max_age, 79);
}

// ── 10. range_scan — aggregates across range ──────────────────────────────────

/// Supplementary: aggregates computed over a range must match the closed-form
/// sum for ids [start, end).
///
/// Detects: aggregate pushdown reading wrong rows, COUNT/SUM disagreeing
/// with the row-level scan.
#[test]
fn range_scan_aggregates_match_formula() {
    const N: usize = 2000;
    let start = 500usize;
    let end = 1000usize; // exclusive
    let expected_count = (end - start) as i64;
    let expected_sum = (start..end).map(|i| i as i64).sum::<i64>();

    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    let count = scalar_int(
        &format!("SELECT COUNT(*) FROM bench_users WHERE id >= {start} AND id < {end}"),
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        count, expected_count,
        "COUNT in range [{start}, {end}) must be {expected_count}"
    );

    let sum = scalar_int(
        &format!("SELECT SUM(id) FROM bench_users WHERE id >= {start} AND id < {end}"),
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        sum, expected_sum,
        "SUM(id) in range [{start}, {end}) must be {expected_sum}"
    );

    let min_id = scalar_int(
        &format!("SELECT MIN(id) FROM bench_users WHERE id >= {start} AND id < {end}"),
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(min_id, start as i64, "MIN(id) in range must be {start}");

    let max_id = scalar_int(
        &format!("SELECT MAX(id) FROM bench_users WHERE id >= {start} AND id < {end}"),
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        max_id,
        (end - 1) as i64,
        "MAX(id) in range must be {}",
        end - 1
    );
}

// ── 11. UPDATE correctness ────────────────────────────────────────────────────

/// UPDATE scenario: update a column for a subset of rows and verify that only
/// those rows changed and all other rows are intact.
///
/// Detects: UPDATE leaking writes to unaffected rows, old values not replaced,
/// heap corruption after in-place update.
#[test]
fn update_subset_changes_only_targeted_rows() {
    const N: usize = 500;
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    load_batch(N, &mut s, &mut txn, &mut bloom, &mut ctx);

    // Double the age for ids 1..=100.
    ok(
        "UPDATE bench_users SET age = age * 2 WHERE id <= 100",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Verify updated rows.
    for id in 1..=100usize {
        let result = rows(ok(
            &format!("SELECT age FROM bench_users WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(result.len(), 1, "updated id={id} must exist");
        let age = match result[0][0] {
            Value::Int(v) => v as i64,
            ref o => panic!("{o:?}"),
        };
        let expected = ((18 + id % 62) * 2) as i64;
        assert_eq!(
            age, expected,
            "age for id={id} must be doubled to {expected}"
        );
    }

    // Verify untouched rows (id=101..=500) still have original ages.
    for &id in &[101, 200, 350, 500] {
        let result = rows(ok(
            &format!("SELECT age FROM bench_users WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(result.len(), 1);
        let age = match result[0][0] {
            Value::Int(v) => v as i64,
            ref o => panic!("{o:?}"),
        };
        let expected_original = (18 + id % 62) as i64;
        assert_eq!(
            age, expected_original,
            "untouched row id={id} must have original age {expected_original}"
        );
    }

    // Overall count must not change.
    let count = scalar_int(
        "SELECT COUNT(*) FROM bench_users",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(count, N as i64, "UPDATE must not change row count");
}
