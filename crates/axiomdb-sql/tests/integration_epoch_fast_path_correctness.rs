/// Correctness verification for the A.2 epoch fast path (autocommit).
///
/// The epoch fast path fires when `!ctx.in_explicit_txn &&
/// ctx.is_table_epoch_current(table_id)`. These tests confirm that:
///
/// 1. The fast path serves fresh data after consecutive reads (no DDL).
/// 2. The epoch mark is cleared after each DML write, so the next read
///    re-validates via catalog schema_version and picks up any root change.
/// 3. VACUUM followed by SELECT still returns correct rows.
/// 4. Many autocommit INSERTs that cause B-tree root splits leave all rows
///    retrievable with correct values.
/// 5. UPDATE and DELETE invalidation works (no stale reads after writes).
/// 6. Two tables in the same session don't cross-contaminate epoch marks.
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
    run_ctx(sql, s, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed:\n  {sql}\nError: {e:?}"))
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

fn count_rows(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    txn: &mut axiomdb_wal::TxnManager,
    bloom: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> i64 {
    scalar_int(ok(sql, s, txn, bloom, ctx))
}

// ── 1. Consecutive autocommit SELECTs return consistent values ────────────────

/// The epoch fast path fires on the second and subsequent reads.
/// Verifies the cached entry is not stale (no writes happened).
#[test]
fn epoch_fast_path_repeated_select_returns_same_count() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ep (id INT PRIMARY KEY, v INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Autocommit INSERTs — one by one.
    for i in 1i32..=50 {
        ok(
            &format!("INSERT INTO ep VALUES ({i}, {})", i * 100),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // Run the same COUNT(*) many times — epoch fast path fires from the 2nd call.
    for _ in 0..10 {
        let c = count_rows(
            "SELECT COUNT(*) FROM ep",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
        assert_eq!(c, 50, "COUNT(*) must be stable across repeated reads");
    }

    // Spot-check specific rows.
    for &id in &[1i32, 25, 50] {
        let r = rows(ok(
            &format!("SELECT v FROM ep WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(r.len(), 1);
        assert_eq!(r[0][0], Value::Int(id * 100), "value mismatch for id={id}");
    }
}

// ── 2. INSERT → SELECT: epoch mark cleared, new row visible ──────────────────

/// After each autocommit INSERT, the epoch mark is cleared. The next SELECT
/// must re-validate and return the freshly inserted row.
#[test]
fn epoch_mark_cleared_after_autocommit_insert() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ep2 (id INT PRIMARY KEY, label TEXT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    for i in 1i32..=20 {
        // INSERT (autocommit) — clears epoch mark.
        ok(
            &format!("INSERT INTO ep2 VALUES ({i}, 'row{i}')"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );

        // SELECT immediately after: must see the just-inserted row.
        let c = count_rows(
            "SELECT COUNT(*) FROM ep2",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
        assert_eq!(
            c, i as i64,
            "after insert {i}: COUNT(*) must be {i}, got {c}"
        );

        let r = rows(ok(
            &format!("SELECT label FROM ep2 WHERE id = {i}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(r.len(), 1, "row {i} must exist immediately after insert");
        assert_eq!(r[0][0], Value::Text(format!("row{i}")));
    }
}

// ── 3. UPDATE → SELECT: epoch mark cleared, updated value visible ─────────────

#[test]
fn epoch_mark_cleared_after_autocommit_update() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ep3 (id INT PRIMARY KEY, score INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    for i in 1i32..=10 {
        ok(
            &format!("INSERT INTO ep3 VALUES ({i}, 0)"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // Update each row and immediately verify the new value.
    for i in 1i32..=10 {
        let new_score = i * 999;
        ok(
            &format!("UPDATE ep3 SET score = {new_score} WHERE id = {i}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );

        let r = rows(ok(
            &format!("SELECT score FROM ep3 WHERE id = {i}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(r.len(), 1);
        assert_eq!(
            r[0][0],
            Value::Int(new_score),
            "score not updated for id={i}"
        );
    }

    // Final aggregate must reflect all updates.
    let sum = scalar_int(ok(
        "SELECT SUM(score) FROM ep3",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected: i64 = (1i64..=10).map(|i| i * 999).sum();
    assert_eq!(
        sum, expected,
        "SUM(score) after all updates: expected {expected}, got {sum}"
    );
}

// ── 4. DELETE → SELECT: epoch mark cleared, deleted row gone ─────────────────

#[test]
fn epoch_mark_cleared_after_autocommit_delete() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ep4 (id INT PRIMARY KEY, v INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    for i in 1i32..=20 {
        ok(
            &format!("INSERT INTO ep4 VALUES ({i}, {i})"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // Delete even ids one by one, verify each time.
    let mut remaining = 20i64;
    for i in (2i32..=20).step_by(2) {
        ok(
            &format!("DELETE FROM ep4 WHERE id = {i}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
        remaining -= 1;

        // Deleted row must be gone.
        let r = rows(ok(
            &format!("SELECT v FROM ep4 WHERE id = {i}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(r.len(), 0, "row {i} must be gone after DELETE");

        // Count must reflect deletion.
        let c = count_rows(
            "SELECT COUNT(*) FROM ep4",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
        assert_eq!(
            c, remaining,
            "COUNT(*) after deleting {i}: expected {remaining}, got {c}"
        );
    }

    // Odd ids must all survive.
    let odd_sum = scalar_int(ok(
        "SELECT SUM(v) FROM ep4",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected: i64 = (1i64..=20).step_by(2).sum();
    assert_eq!(odd_sum, expected, "SUM of odd ids should be {expected}");
}

// ── 5. VACUUM → SELECT: epoch marks cleared, correct data still visible ───────

#[test]
fn epoch_cleared_after_vacuum_select_returns_correct_data() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ep5 (id INT PRIMARY KEY, name TEXT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Insert rows, delete half, vacuum, verify.
    for i in 1i32..=100 {
        ok(
            &format!("INSERT INTO ep5 VALUES ({i}, 'item{i}')"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    for i in (2i32..=100).step_by(2) {
        ok(
            &format!("DELETE FROM ep5 WHERE id = {i}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // Pre-vacuum: warm the epoch cache with a SELECT.
    let pre = count_rows(
        "SELECT COUNT(*) FROM ep5",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(pre, 50);

    // VACUUM: clears epoch marks, may compact the B-tree.
    ok("VACUUM ep5", &mut s, &mut txn, &mut bloom, &mut ctx);

    // Post-vacuum: epoch marks cleared → next SELECT re-validates.
    let post = count_rows(
        "SELECT COUNT(*) FROM ep5",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(post, 50, "VACUUM must not change visible row count");

    // Odd rows must all be intact with correct names.
    for &id in &[1i32, 3, 51, 99] {
        let r = rows(ok(
            &format!("SELECT name FROM ep5 WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(r.len(), 1, "row {id} must survive vacuum");
        assert_eq!(r[0][0], Value::Text(format!("item{id}")));
    }

    // Even rows must be gone.
    let even = count_rows(
        "SELECT COUNT(*) FROM ep5 WHERE id % 2 = 0",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(even, 0, "even rows must be absent after vacuum");
}

// ── 6. Many autocommit INSERTs (potential root splits) → all rows correct ─────

/// Inserts enough rows to force B-tree root splits, then verifies COUNT,
/// SUM, and spot-check values. If the epoch fast path served a stale
/// root_page_id after a split, subsequent SELECTs would return wrong data
/// or panic with PageNotFound.
#[test]
fn many_autocommit_inserts_root_splits_all_rows_correct() {
    const N: usize = 2_000; // enough to cause multiple root splits
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ep6 (id INT PRIMARY KEY, val INT NOT NULL, tag TEXT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    for i in 1usize..=N {
        ok(
            &format!("INSERT INTO ep6 VALUES ({i}, {}, 'tag{i}')", i * 7),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // COUNT must be exactly N.
    let count = count_rows(
        "SELECT COUNT(*) FROM ep6",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        count, N as i64,
        "COUNT(*) after {N} autocommit inserts: expected {N}, got {count}"
    );

    // SUM(id) must be N*(N+1)/2.
    let sum_id = scalar_int(ok(
        "SELECT SUM(id) FROM ep6",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected_sum = (N as i64) * (N as i64 + 1) / 2;
    assert_eq!(sum_id, expected_sum, "SUM(id) mismatch after root splits");

    // SUM(val) = SUM(i*7 for i in 1..=N) = 7 * N*(N+1)/2.
    let sum_val = scalar_int(ok(
        "SELECT SUM(val) FROM ep6",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(sum_val, 7 * expected_sum, "SUM(val) mismatch");

    // Spot-check rows at boundaries and mid-points.
    for &id in &[1usize, 500, 1000, 1500, N] {
        let r = rows(ok(
            &format!("SELECT val, tag FROM ep6 WHERE id = {id}"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        ));
        assert_eq!(r.len(), 1, "row {id} must exist after root splits");
        assert_eq!(
            r[0][0],
            Value::Int((id * 7) as i32),
            "val mismatch for id={id}"
        );
        assert_eq!(
            r[0][1],
            Value::Text(format!("tag{id}")),
            "tag mismatch for id={id}"
        );
    }
}

// ── 7. Two tables: epoch marks are per-table, no cross-contamination ──────────

#[test]
fn epoch_marks_are_per_table_no_cross_contamination() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ta (id INT PRIMARY KEY, v INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    ok(
        "CREATE TABLE tb (id INT PRIMARY KEY, v INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Populate both tables.
    for i in 1i32..=30 {
        ok(
            &format!("INSERT INTO ta VALUES ({i}, {})", i * 2),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    for i in 1i32..=20 {
        ok(
            &format!("INSERT INTO tb VALUES ({i}, {})", i * 3),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }

    // Warm both epoch caches.
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM ta",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        30
    );
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM tb",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        20
    );

    // Insert into ta only — must NOT affect tb's epoch mark or cached data.
    ok(
        "INSERT INTO ta VALUES (31, 62)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // ta must reflect the new row.
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM ta",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        31
    );
    // tb must be unchanged (epoch fast path is fine here — no write to tb).
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM tb",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        20
    );

    // Now update tb — must NOT affect ta.
    ok(
        "UPDATE tb SET v = v + 1000 WHERE id = 10",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // tb row 10 must show the new value.
    let tb10 = scalar_int(ok(
        "SELECT v FROM tb WHERE id = 10",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(tb10, 3 * 10 + 1000, "tb.v for id=10 after update");

    // ta must still have 31 rows, all with correct values.
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM ta",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        31
    );
    let ta31 = scalar_int(ok(
        "SELECT v FROM ta WHERE id = 31",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(ta31, 62);

    // Sum checks.
    let sum_ta = scalar_int(ok(
        "SELECT SUM(v) FROM ta",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected_ta: i64 = (1i64..=31).map(|i| i * 2).sum();
    assert_eq!(sum_ta, expected_ta, "SUM(ta.v) mismatch");

    let sum_tb = scalar_int(ok(
        "SELECT SUM(v) FROM tb",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    let expected_tb: i64 = (1i64..=20).map(|i| i * 3).sum::<i64>() + 1000;
    assert_eq!(
        sum_tb, expected_tb,
        "SUM(tb.v) mismatch after partial update"
    );
}

// ── 8. Mixed explicit txn + autocommit: explicit txn stays on slow path ───────

/// Explicit transactions must NOT use the epoch fast path. This test
/// verifies that autocommit queries correctly update the epoch cache while
/// explicit transactions remain fully correct.
#[test]
fn explicit_txn_and_autocommit_interleave_correctly() {
    let (mut s, mut txn, mut bloom, mut ctx) = setup_ctx();
    ok(
        "CREATE TABLE ep8 (id INT PRIMARY KEY, v INT NOT NULL)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );

    // Autocommit: insert rows, warm epoch cache.
    for i in 1i32..=10 {
        ok(
            &format!("INSERT INTO ep8 VALUES ({i}, {i})"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM ep8",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        10
    );

    // Explicit txn: insert more rows.
    ok("BEGIN", &mut s, &mut txn, &mut bloom, &mut ctx);
    for i in 11i32..=20 {
        ok(
            &format!("INSERT INTO ep8 VALUES ({i}, {i})"),
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx,
        );
    }
    // Inside the txn, must see 20 rows (read-your-own-writes).
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM ep8",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        20
    );

    ok("ROLLBACK", &mut s, &mut txn, &mut bloom, &mut ctx);

    // After rollback, only the 10 autocommit rows survive.
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM ep8",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        10,
        "explicit txn rows must be gone after ROLLBACK"
    );

    // Autocommit again: add one more row.
    ok(
        "INSERT INTO ep8 VALUES (99, 99)",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    );
    assert_eq!(
        count_rows(
            "SELECT COUNT(*) FROM ep8",
            &mut s,
            &mut txn,
            &mut bloom,
            &mut ctx
        ),
        11
    );
    let max_v = scalar_int(ok(
        "SELECT MAX(v) FROM ep8",
        &mut s,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(max_v, 99);
}
