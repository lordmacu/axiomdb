//! Integration tests for `GROUP BY ... WITH ROLLUP` (GAP-C.5).

mod common;

use axiomdb_sql::{bloom::BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use common::*;

fn setup() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE TABLE sales (dept TEXT, item TEXT, qty INT)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    // dept=a: item=x (2 + 3) + item=y (1)  = 6
    // dept=b: item=x (4)                   = 4
    // grand total                          = 10
    run_ctx(
        "INSERT INTO sales VALUES \
         ('a','x',2),('a','x',3),('a','y',1),('b','x',4)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    (storage, txn, bloom, ctx)
}

fn run_rows(sql: &str) -> Vec<Vec<Value>> {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup();
    let QueryResult::Rows { rows, .. } =
        run_ctx(sql, &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap()
    else {
        panic!("expected rows for: {sql}");
    };
    rows
}

fn as_int(v: &Value) -> i64 {
    match v {
        Value::Int(n) => *n as i64,
        Value::BigInt(n) => *n,
        other => panic!("expected int, got {other:?}"),
    }
}

fn txt(v: &Value) -> Option<String> {
    match v {
        Value::Text(s) => Some(s.clone()),
        Value::Null => None,
        other => panic!("expected text or null, got {other:?}"),
    }
}

#[test]
fn rollup_single_column() {
    // GROUP BY dept WITH ROLLUP → one row per dept + grand total.
    let mut rows =
        run_rows("SELECT dept, SUM(qty) FROM sales GROUP BY dept WITH ROLLUP ORDER BY dept");
    // ORDER BY with NULL-first default: grand total then a, b.
    assert_eq!(rows.len(), 3);

    // Normalize order: sort leaves first (a, b), grand total last.
    rows.sort_by_key(|r| match &r[0] {
        Value::Null => (1, String::new()),
        Value::Text(s) => (0, s.clone()),
        _ => unreachable!(),
    });

    assert_eq!(txt(&rows[0][0]).as_deref(), Some("a"));
    assert_eq!(as_int(&rows[0][1]), 6);

    assert_eq!(txt(&rows[1][0]).as_deref(), Some("b"));
    assert_eq!(as_int(&rows[1][1]), 4);

    // Grand total
    assert!(matches!(rows[2][0], Value::Null));
    assert_eq!(as_int(&rows[2][1]), 10);
}

#[test]
fn rollup_two_columns_produces_all_levels() {
    // GROUP BY dept, item WITH ROLLUP → per (dept,item), per dept subtotal,
    // grand total. Expected 6 rows: (a,x), (a,y), (a,NULL)=6, (b,x), (b,NULL)=4, (NULL,NULL)=10
    let rows = run_rows("SELECT dept, item, SUM(qty) FROM sales GROUP BY dept, item WITH ROLLUP");
    assert_eq!(
        rows.len(),
        6,
        "two-level rollup emits 6 rows, got {}: {rows:?}",
        rows.len()
    );

    // Count rows by (dept_is_null, item_is_null) signature.
    let mut leaf = 0;
    let mut dept_subtotal = 0;
    let mut grand = 0;
    for r in &rows {
        let dept_null = matches!(r[0], Value::Null);
        let item_null = matches!(r[1], Value::Null);
        match (dept_null, item_null) {
            (false, false) => leaf += 1,
            (false, true) => dept_subtotal += 1,
            (true, true) => grand += 1,
            (true, false) => panic!("rollup must not emit (NULL,value)"),
        }
    }
    assert_eq!(leaf, 3, "three leaf groups: (a,x),(a,y),(b,x)");
    assert_eq!(dept_subtotal, 2, "two dept subtotals");
    assert_eq!(grand, 1, "one grand total");

    // Check specific subtotal: dept=a, item=NULL → SUM=6
    let a_subtotal = rows
        .iter()
        .find(|r| matches!(&r[0], Value::Text(s) if s == "a") && matches!(r[1], Value::Null))
        .expect("dept=a subtotal missing");
    assert_eq!(as_int(&a_subtotal[2]), 6);

    // Grand total SUM=10.
    let grand_row = rows
        .iter()
        .find(|r| matches!(r[0], Value::Null) && matches!(r[1], Value::Null))
        .unwrap();
    assert_eq!(as_int(&grand_row[2]), 10);
}

#[test]
fn rollup_with_count() {
    let rows = run_rows("SELECT dept, COUNT(*) FROM sales GROUP BY dept WITH ROLLUP");
    // 2 leaves + 1 grand.
    assert_eq!(rows.len(), 3);
    let grand = rows.iter().find(|r| matches!(r[0], Value::Null)).unwrap();
    assert_eq!(as_int(&grand[1]), 4, "COUNT(*) grand total must be 4");
}

#[test]
fn rollup_respects_limit() {
    // 3 total rows, LIMIT 2 returns first two after ORDER BY.
    let rows = run_rows(
        "SELECT dept, SUM(qty) FROM sales GROUP BY dept WITH ROLLUP ORDER BY dept LIMIT 2",
    );
    assert_eq!(rows.len(), 2);
}
