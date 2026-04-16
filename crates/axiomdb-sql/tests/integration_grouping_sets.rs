//! Integration tests for `GROUP BY ROLLUP / CUBE / GROUPING SETS` (Phase 21.21).
//!
//! Tests are ordered: parse-smoke (Step 2), executor (Step 3), GROUPING() (Step 4).

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE gs_sales (region TEXT, yr INT, amount INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // region=N, yr=2022: 10+20=30
    // region=N, yr=2023: 15
    // region=S, yr=2022: 5
    // region=S, yr=2023: 25+10=35
    run_ctx(
        "INSERT INTO gs_sales VALUES \
         ('N',2022,10),('N',2022,20),('N',2023,15), \
         ('S',2022,5),('S',2023,25),('S',2023,10)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

fn run(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> QueryResult {
    run_ctx(sql, storage, txn, bloom, ctx).unwrap()
}

fn rows(res: QueryResult) -> Vec<Vec<Value>> {
    match res {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {:?}", other),
    }
}

// ── Step 2 smoke: parser accepts ROLLUP/CUBE/GROUPING SETS syntax ────────────

#[test]
fn test_parse_rollup_accepted() {
    let (mut s, mut t, mut b, mut c) = setup();
    // Just check it parses without error (executor returns NotImplemented for now)
    let res = run_ctx(
        "SELECT region, yr, SUM(amount) FROM gs_sales GROUP BY ROLLUP(region, yr)",
        &mut s, &mut t, &mut b, &mut c,
    );
    // NotImplemented is acceptable in Step 2; what matters is no ParseError
    match res {
        Err(e) => {
            let msg = format!("{e}");
            assert!(
                !msg.contains("syntax error"),
                "should not be a parse error, got: {msg}"
            );
        }
        Ok(_) => {} // executor already works (after Step 3)
    }
}

#[test]
fn test_parse_cube_accepted() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = run_ctx(
        "SELECT region, yr, SUM(amount) FROM gs_sales GROUP BY CUBE(region, yr)",
        &mut s, &mut t, &mut b, &mut c,
    );
    match res {
        Err(e) => {
            let msg = format!("{e}");
            assert!(!msg.contains("syntax error"), "parse error: {msg}");
        }
        Ok(_) => {}
    }
}

#[test]
fn test_parse_grouping_sets_accepted() {
    let (mut s, mut t, mut b, mut c) = setup();
    let res = run_ctx(
        "SELECT region, yr, SUM(amount) FROM gs_sales \
         GROUP BY GROUPING SETS((region, yr), (region), ())",
        &mut s, &mut t, &mut b, &mut c,
    );
    match res {
        Err(e) => {
            let msg = format!("{e}");
            assert!(!msg.contains("syntax error"), "parse error: {msg}");
        }
        Ok(_) => {}
    }
}

#[test]
fn test_parse_mixed_plain_and_rollup() {
    let (mut s, mut t, mut b, mut c) = setup();
    // GROUP BY a, ROLLUP(b) — cross-product → 2 sets: {a,b} and {a}
    let res = run_ctx(
        "SELECT region, yr, SUM(amount) FROM gs_sales GROUP BY region, ROLLUP(yr)",
        &mut s, &mut t, &mut b, &mut c,
    );
    match res {
        Err(e) => {
            let msg = format!("{e}");
            assert!(!msg.contains("syntax error"), "parse error: {msg}");
        }
        Ok(_) => {}
    }
}

#[test]
fn test_mysql_with_rollup_still_works_regression() {
    // MySQL-style WITH ROLLUP must still work after Step 1/2 refactor
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, SUM(amount) FROM gs_sales GROUP BY region WITH ROLLUP",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // 2 region groups + 1 grand total = 3 rows
    assert_eq!(r.len(), 3, "expected 3 rows (2 groups + grand total), got {:?}", r);
    // Grand total row has NULL region
    let grand_total = r.iter().find(|row| row[0] == Value::Null);
    assert!(grand_total.is_some(), "missing grand total row with NULL region");
    let total_sum = grand_total.unwrap()[1].clone();
    assert_eq!(total_sum, Value::Int(85), "grand total SUM should be 85, got {:?}", total_sum);
}

// ── Step 3 tests: executor results ──────────────────────────────────────────
// These tests require execute_select_grouped_sets (Step 3) to be implemented.
// They are marked #[ignore] until then; Step 3 will remove the attribute.

#[test]
#[ignore = "requires Step 3 executor (execute_select_grouped_sets)"]
fn test_rollup_two_columns() {
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, yr, SUM(amount) FROM gs_sales GROUP BY ROLLUP(region, yr) ORDER BY region, yr",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // Expected rows (order may vary but we request ORDER BY):
    // ('N', 2022, 30), ('N', 2023, 15), ('N', NULL, 45),   -- N subtotals
    // ('S', 2022, 5),  ('S', 2023, 35), ('S', NULL, 40),   -- S subtotals
    // (NULL, NULL, 85)                                       -- grand total
    assert_eq!(r.len(), 7, "ROLLUP(region, yr) should produce 7 rows, got {:?}", r);
    // Verify grand total
    let grand = r.iter().find(|row| row[0] == Value::Null && row[1] == Value::Null);
    assert!(grand.is_some(), "missing grand total row");
    assert_eq!(grand.unwrap()[2], Value::Int(85));
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_rollup_single_column() {
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, SUM(amount) FROM gs_sales GROUP BY ROLLUP(region)",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // {N: 45}, {S: 40}, grand-total {NULL: 85} = 3 rows
    assert_eq!(r.len(), 3, "ROLLUP(region) should produce 3 rows, got {:?}", r);
    let grand = r.iter().find(|row| row[0] == Value::Null);
    assert!(grand.is_some());
    assert_eq!(grand.unwrap()[1], Value::Int(85));
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_cube_two_columns() {
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, yr, SUM(amount) FROM gs_sales GROUP BY CUBE(region, yr)",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // CUBE(region, yr): 4 sets: {region,yr}, {region}, {yr}, {}
    // {region,yr}: 4 rows (N/2022, N/2023, S/2022, S/2023)
    // {region}: 2 rows (N, S)
    // {yr}: 2 rows (2022, 2023)
    // {}: 1 row (grand total)
    // Total: 4+2+2+1 = 9 rows
    assert_eq!(r.len(), 9, "CUBE(region, yr) should produce 9 rows, got {:?}", r);
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_grouping_sets_explicit() {
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, yr, SUM(amount) FROM gs_sales \
         GROUP BY GROUPING SETS((region, yr), (region), ())",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // (region,yr): 4 rows; (region): 2 rows; (): 1 row = 7 total
    assert_eq!(r.len(), 7, "GROUPING SETS((region,yr),(region),()) should give 7 rows, got {:?}", r);
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_grand_total_row_nulls() {
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, SUM(amount) FROM gs_sales GROUP BY ROLLUP(region)",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // Grand total row has NULL in all group keys
    let grand = r.iter().find(|row| row[0] == Value::Null);
    assert!(grand.is_some(), "no grand total row");
    assert_eq!(grand.unwrap()[1], Value::Int(85), "grand total should be 85");
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_having_per_pass() {
    let (mut s, mut t, mut b, mut c) = setup();
    // HAVING SUM(amount) > 40 should filter within each pass
    let result = run(
        "SELECT region, SUM(amount) FROM gs_sales \
         GROUP BY ROLLUP(region) HAVING SUM(amount) > 40",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // N: 45 (passes), S: 40 (fails), grand: 85 (passes) → 2 rows
    assert_eq!(r.len(), 2, "HAVING SUM > 40 should give 2 rows, got {:?}", r);
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_mixed_plain_and_rollup_cross_product() {
    // GROUP BY region, ROLLUP(yr) → cross-product: {region, yr}, {region}
    // Same as GROUP BY ROLLUP(yr) per region group
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, yr, SUM(amount) FROM gs_sales GROUP BY region, ROLLUP(yr)",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // {region,yr}: 4 rows; {region}: 2 rows = 6 total (no grand total — region is always present)
    assert_eq!(r.len(), 6, "GROUP BY region, ROLLUP(yr) should give 6 rows, got {:?}", r);
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_order_by_post_union() {
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, SUM(amount) FROM gs_sales GROUP BY ROLLUP(region) ORDER BY SUM(amount) ASC",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    assert_eq!(r.len(), 3);
    // S=40, N=45, grand=85 — in ascending order of SUM
    let sums: Vec<i64> = r.iter().map(|row| match &row[1] {
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        other => panic!("unexpected value: {:?}", other),
    }).collect();
    assert!(sums.windows(2).all(|w| w[0] <= w[1]), "should be sorted ASC: {:?}", sums);
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_limit_post_union() {
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, SUM(amount) FROM gs_sales GROUP BY ROLLUP(region) LIMIT 2",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    assert_eq!(r.len(), 2, "LIMIT 2 should give 2 rows, got {:?}", r);
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_grouping_sets_duplicate_sets() {
    // GROUPING SETS((region),(region)) → duplicate sets → 2x region rows
    let (mut s, mut t, mut b, mut c) = setup();
    let result = run(
        "SELECT region, SUM(amount) FROM gs_sales GROUP BY GROUPING SETS((region),(region))",
        &mut s, &mut t, &mut b, &mut c,
    );
    let r = rows(result);
    // 2 region groups × 2 sets = 4 rows (SQL standard: duplicate sets produce duplicate rows)
    assert_eq!(r.len(), 4, "duplicate sets should produce 4 rows, got {:?}", r);
}

#[test]
#[ignore = "requires Step 3 executor"]
fn test_real_null_not_confused() {
    // If original data has NULL in group-by column, it groups with other NULLs
    // but GROUPING() should return 0 (it's a real NULL, not a rolled-up NULL).
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE t_null_grp (a INT, v INT)",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    ).unwrap();
    run_ctx(
        "INSERT INTO t_null_grp VALUES (NULL, 5), (NULL, 10), (1, 20)",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    ).unwrap();
    let result = run(
        "SELECT a, SUM(v) FROM t_null_grp GROUP BY ROLLUP(a)",
        &mut storage, &mut txn, &mut bloom, &mut ctx,
    );
    let r = rows(result);
    // {a}: NULL group (15) + group 1 (20) = 2 rows
    // grand total: 35
    // Total: 3 rows
    assert_eq!(r.len(), 3, "should have 3 rows, got {:?}", r);
}
