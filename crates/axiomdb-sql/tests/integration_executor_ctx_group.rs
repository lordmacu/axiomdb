//! Integration tests for ctx-path sorted GROUP BY execution.

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

// ── Phase 4.9b: Sort-Based GROUP BY ──────────────────────────────────────────

/// Convenience wrapper: run SQL through the ctx path, panic on error.
fn rctx(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx)
        .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"))
}

/// Sets up a table with a single-column index on `dept` and inserts rows.
fn setup_indexed_dept_ctx(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) {
    rctx(
        "CREATE TABLE emp_idx (id INT NOT NULL, dept TEXT NOT NULL, salary INT NOT NULL)",
        storage,
        txn,
        bloom,
        ctx,
    );
    rctx(
        "CREATE INDEX idx_dept ON emp_idx (dept)",
        storage,
        txn,
        bloom,
        ctx,
    );
    // eng × 3, hr × 1, sales × 2 — inserted in alphabetical dept order so
    // insertion order matches index order (validates presorted assumption).
    for (id, dept, salary) in [
        (1, "eng", 90000),
        (2, "eng", 80000),
        (3, "eng", 70000),
        (4, "hr", 65000),
        (5, "sales", 60000),
        (6, "sales", 55000),
    ] {
        rctx(
            &format!("INSERT INTO emp_idx VALUES ({id}, '{dept}', {salary})"),
            storage,
            txn,
            bloom,
            ctx,
        );
    }
}

/// Sets up a table with a composite index on `(region, dept)`.
fn setup_composite_index_ctx(
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) {
    rctx(
        "CREATE TABLE emp_comp (id INT NOT NULL, region TEXT NOT NULL, dept TEXT NOT NULL, salary INT NOT NULL)",
        storage,
        txn,
        bloom,
        ctx,
    );
    rctx(
        "CREATE INDEX idx_region_dept ON emp_comp (region, dept)",
        storage,
        txn,
        bloom,
        ctx,
    );
    for (id, region, dept, salary) in [
        (1, "east", "eng", 90000),
        (2, "east", "eng", 85000),
        (3, "east", "sales", 70000),
        (4, "west", "eng", 80000),
        (5, "west", "hr", 65000),
        (6, "west", "hr", 60000),
    ] {
        rctx(
            &format!("INSERT INTO emp_comp VALUES ({id}, '{region}', '{dept}', {salary})"),
            storage,
            txn,
            bloom,
            ctx,
        );
    }
}

#[test]
fn test_sorted_group_by_equality_lookup() {
    // IndexLookup equality path: GROUP BY on the same column → sorted strategy.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    // dept = 'eng' uses IndexLookup (non-unique → treated as range), GROUP BY dept.
    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_idx WHERE dept = 'eng' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Only one group (eng).
    assert_eq!(r.len(), 1, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("eng".into()), "rows={r:?}");
    assert_eq!(r[0][1], Value::BigInt(3));
}

#[test]
fn test_sorted_group_by_raw_scan_order() {
    // Verify that the dept index actually returns rows in alphabetical dept order.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept FROM emp_idx WHERE dept >= 'a' AND dept <= 'z'",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Must come back in index (alphabetical dept) order: eng, eng, eng, hr, sales, sales.
    assert_eq!(r.len(), 6, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("eng".into()), "rows={r:?}");
    assert_eq!(r[1][0], Value::Text("eng".into()));
    assert_eq!(r[2][0], Value::Text("eng".into()));
    assert_eq!(r[3][0], Value::Text("hr".into()));
    assert_eq!(r[4][0], Value::Text("sales".into()));
    assert_eq!(r[5][0], Value::Text("sales".into()));
}

#[test]
fn test_sorted_group_by_single_index_count() {
    // Index on (dept) + two-sided range triggers IndexRange → sorted strategy
    // (strategy only chosen in choose_group_by_strategy_ctx, i.e. the ctx path).
    // The planner requires AND for range detection; single-sided >= falls to Scan.
    // Sorted path emits groups in index key order (alphabetical for TEXT).
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Sorted path emits groups in index (alphabetical) order: eng, hr, sales.
    assert_eq!(r.len(), 3, "rows={r:?}");
    assert_eq!(r[0][0], Value::Text("eng".into()), "rows={r:?}");
    assert_eq!(r[0][1], Value::BigInt(3));
    assert_eq!(r[1][0], Value::Text("hr".into()));
    assert_eq!(r[1][1], Value::BigInt(1));
    assert_eq!(r[2][0], Value::Text("sales".into()));
    assert_eq!(r[2][1], Value::BigInt(2));
}

#[test]
fn test_sorted_group_by_single_index_sum() {
    // SUM under the sorted path (ctx path) must aggregate correctly.
    // Sorted path emits groups in index key order: eng, hr, sales.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, SUM(salary) FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // eng: 90000+80000+70000=240000; hr: 65000; sales: 60000+55000=115000
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::Int(240000));
    assert_eq!(r[1][0], Value::Text("hr".into()));
    assert_eq!(r[1][1], Value::Int(65000));
    assert_eq!(r[2][0], Value::Text("sales".into()));
    assert_eq!(r[2][1], Value::Int(115000));
}

#[test]
fn test_sorted_group_by_composite_prefix_full() {
    // Index on (region, dept) + GROUP BY region, dept → full prefix match.
    // Sorted path emits groups in (region, dept) index order.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_composite_index_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT region, dept, COUNT(*) FROM emp_comp WHERE region >= 'a' AND region <= 'z' GROUP BY region, dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    // Index order: east/eng(2), east/sales(1), west/eng(1), west/hr(2)
    assert_eq!(r.len(), 4);
    assert_eq!(r[0][0], Value::Text("east".into()));
    assert_eq!(r[0][1], Value::Text("eng".into()));
    assert_eq!(r[0][2], Value::BigInt(2));
    assert_eq!(r[1][0], Value::Text("east".into()));
    assert_eq!(r[1][1], Value::Text("sales".into()));
    assert_eq!(r[1][2], Value::BigInt(1));
    assert_eq!(r[2][0], Value::Text("west".into()));
    assert_eq!(r[2][1], Value::Text("eng".into()));
    assert_eq!(r[2][2], Value::BigInt(1));
    assert_eq!(r[3][0], Value::Text("west".into()));
    assert_eq!(r[3][1], Value::Text("hr".into()));
    assert_eq!(r[3][2], Value::BigInt(2));
}

#[test]
fn test_sorted_group_by_composite_prefix_partial() {
    // Index on (region, dept) + GROUP BY region only → leading prefix match.
    // Sorted path emits groups in region index order: east, west.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_composite_index_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT region, COUNT(*) FROM emp_comp WHERE region >= 'a' AND region <= 'z' GROUP BY region",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("east".into()));
    assert_eq!(r[0][1], Value::BigInt(3));
    assert_eq!(r[1][0], Value::Text("west".into()));
    assert_eq!(r[1][1], Value::BigInt(3));
}

#[test]
fn test_hash_group_by_without_index_still_correct() {
    // Plain table scan (no index) → hash strategy; must produce correct results.
    // Hash iteration order is non-deterministic — sort manually to verify values.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    // Setup employees via plain run() — no index, will use hash strategy.
    run(
        "CREATE TABLE emp_hash (id INT NOT NULL, dept TEXT, salary INT)",
        &mut storage,
        &mut txn,
    );
    for (id, dept, salary) in [
        (1, "eng", 90000),
        (2, "eng", 80000),
        (3, "eng", 70000),
        (4, "sales", 60000),
        (5, "sales", 55000),
    ] {
        run(
            &format!("INSERT INTO emp_hash VALUES ({id}, '{dept}', {salary})"),
            &mut storage,
            &mut txn,
        );
    }

    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_hash GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2);
    let mut counts: Vec<(String, i64)> = r
        .iter()
        .map(|row| {
            let dept = match &row[0] {
                Value::Text(s) => s.clone(),
                v => panic!("expected Text, got {v:?}"),
            };
            let cnt = match row[1] {
                Value::BigInt(n) => n,
                ref v => panic!("expected BigInt, got {v:?}"),
            };
            (dept, cnt)
        })
        .collect();
    counts.sort_by_key(|(d, _)| d.clone());
    assert_eq!(counts, vec![("eng".into(), 3), ("sales".into(), 2)]);
}

#[test]
fn test_sorted_group_by_having() {
    // HAVING must filter correctly under the sorted path.
    // hr has count=1 → filtered out; eng(3) and sales(2) remain in index order.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, COUNT(*) FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept HAVING COUNT(*) > 1",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::BigInt(3));
    assert_eq!(r[1][0], Value::Text("sales".into()));
    assert_eq!(r[1][1], Value::BigInt(2));
}

#[test]
fn test_sorted_group_by_group_concat() {
    // GROUP_CONCAT must accumulate correctly under the sorted path.
    // The inner ORDER BY is within the aggregate (operates on per-group input rows).
    // Sorted path emits groups in index order: eng, hr, sales.
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    setup_indexed_dept_ctx(&mut storage, &mut txn, &mut bloom, &mut ctx);

    let r = rows(rctx(
        "SELECT dept, GROUP_CONCAT(salary ORDER BY salary DESC SEPARATOR ',') FROM emp_idx WHERE dept >= 'a' AND dept <= 'z' GROUP BY dept",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Text("eng".into()));
    assert_eq!(r[0][1], Value::Text("90000,80000,70000".into()));
    assert_eq!(r[1][0], Value::Text("hr".into()));
    assert_eq!(r[1][1], Value::Text("65000".into()));
    assert_eq!(r[2][0], Value::Text("sales".into()));
    assert_eq!(r[2][1], Value::Text("60000,55000".into()));
}
