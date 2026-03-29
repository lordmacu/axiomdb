//! Integration tests for SQL expression and statement coverage beyond the core executor path.

mod common;

use axiomdb_core::error::DbError;
use axiomdb_types::Value;

use common::*;

// ── 4.16: SQL full test suite ─────────────────────────────────────────────────
// Fills coverage gaps: LIKE, BETWEEN, IN, IS NULL, constraints, expressions,
// scalar functions, NULL semantics, error cases.

// ── LIKE / NOT LIKE ───────────────────────────────────────────────────────────

#[test]
fn test_like_basic() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (name TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES ('Alice'), ('Bob'), ('Alfred')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT name FROM t WHERE name LIKE 'Al%' ORDER BY name",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Text("Alfred".into()));
    assert_eq!(r[1][0], Value::Text("Alice".into()));
}

#[test]
fn test_like_single_char_wildcard() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (v TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES ('cat'), ('car'), ('card')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT v FROM t WHERE v LIKE 'ca_' ORDER BY v",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2); // cat, car (not card — 4 chars)
}

#[test]
fn test_not_like() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (v TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES ('hello'), ('world'), ('help')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT v FROM t WHERE v NOT LIKE 'hel%' ORDER BY v",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Text("world".into()));
}

// ── BETWEEN ───────────────────────────────────────────────────────────────────

#[test]
fn test_between_inclusive() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (n INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1),(2),(3),(4),(5)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT n FROM t WHERE n BETWEEN 2 AND 4 ORDER BY n",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 3);
    assert_eq!(r[0][0], Value::Int(2));
    assert_eq!(r[2][0], Value::Int(4));
}

#[test]
fn test_not_between() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (n INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1),(3),(5)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT n FROM t WHERE n NOT BETWEEN 2 AND 4 ORDER BY n",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(5));
}

// ── IN list ───────────────────────────────────────────────────────────────────

#[test]
fn test_in_list() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, name TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1,'a'),(2,'b'),(3,'c')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id FROM t WHERE id IN (1, 3) ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(3));
}

#[test]
fn test_not_in_list() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (1),(2),(3)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT id FROM t WHERE id NOT IN (1, 3)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(2));
}

// ── IS NULL / IS NOT NULL ─────────────────────────────────────────────────────

#[test]
fn test_is_null_filter() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, val TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1, 'a'), (2, NULL), (3, NULL)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id FROM t WHERE val IS NULL ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(2));
}

#[test]
fn test_is_not_null_filter() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, val TEXT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1, 'a'), (2, NULL)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT id FROM t WHERE val IS NOT NULL",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1));
}

// ── Constraints ───────────────────────────────────────────────────────────────

#[test]
fn test_not_null_constraint_violated() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT NOT NULL, name TEXT)",
        &mut storage,
        &mut txn,
    );

    // NOT NULL: stored as `nullable=false` in catalog but not yet enforced at insert time.
    // Documents current behavior (succeeds without error).
    run(
        "INSERT INTO t VALUES (NULL, 'Alice')",
        &mut storage,
        &mut txn,
    );
    let r = rows(run("SELECT id FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Null); // NULL accepted (constraint not enforced yet)
}

#[test]
fn test_unique_constraint_enforced() {
    // UNIQUE column constraint creates a B-Tree unique index at CREATE TABLE time
    // (Phase 6.5). Duplicate values are rejected at INSERT time.
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT UNIQUE, name TEXT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 'Alice')", &mut storage, &mut txn);
    // Second insert with same id should fail with UniqueViolation.
    let err = run_result("INSERT INTO t VALUES (1, 'Bob')", &mut storage, &mut txn)
        .expect_err("expected UniqueViolation");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got: {err}"
    );
    // Only the first row should exist.
    let r = rows(run("SELECT COUNT(*) FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(1));
}

#[test]
fn test_check_constraint_parsed_but_not_yet_enforced() {
    // CHECK constraints are parsed and stored in the AST but not yet evaluated
    // at INSERT time (gap from 4.3b — enforcement deferred to a later subphase).
    // This test verifies the current behavior: INSERT with CHECK-violating values
    // succeeds silently.
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (age INT CHECK (age >= 0))",
        &mut storage,
        &mut txn,
    );
    // Succeeds (CHECK not enforced yet)
    run("INSERT INTO t VALUES (-1)", &mut storage, &mut txn);
    let r = rows(run("SELECT age FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(-1));
}

#[test]
fn test_check_constraint_passes() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (age INT CHECK (age >= 0))",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (0)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (42)", &mut storage, &mut txn);

    let r = rows(run("SELECT COUNT(*) FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(2));
}

// ── UPDATE with arithmetic expression ────────────────────────────────────────

#[test]
fn test_update_with_arithmetic() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (id INT, score INT)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (1, 10), (2, 20), (3, 30)",
        &mut storage,
        &mut txn,
    );

    run(
        "UPDATE t SET score = score + 5 WHERE id <= 2",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT score FROM t ORDER BY id",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(15));
    assert_eq!(r[1][0], Value::Int(25));
    assert_eq!(r[2][0], Value::Int(30)); // unchanged
}

#[test]
fn test_update_multiple_columns() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT, a INT, b INT)",
        &mut storage,
        &mut txn,
    );
    run("INSERT INTO t VALUES (1, 10, 20)", &mut storage, &mut txn);

    run(
        "UPDATE t SET a = 99, b = 88 WHERE id = 1",
        &mut storage,
        &mut txn,
    );

    let r = rows(run("SELECT a, b FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(99));
    assert_eq!(r[0][1], Value::Int(88));
}

// ── NULL semantics in arithmetic ─────────────────────────────────────────────

#[test]
fn test_null_arithmetic_propagation() {
    let (mut storage, mut txn) = setup();
    // NULL + anything = NULL
    let r = rows(run(
        "SELECT NULL + 1, 1 + NULL, NULL * 0",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Null);
}

#[test]
fn test_null_comparison_returns_null() {
    let (mut storage, mut txn) = setup();
    // NULL = NULL → NULL (not TRUE)
    let r = rows(run(
        "SELECT NULL = NULL, NULL <> NULL, NULL > 1",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Null);
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Null);
}

#[test]
fn test_three_valued_logic_and() {
    let (mut storage, mut txn) = setup();
    // FALSE AND NULL = FALSE (short-circuit)
    // TRUE AND NULL = NULL
    let r = rows(run(
        "SELECT FALSE AND NULL, TRUE AND NULL, NULL AND FALSE",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Bool(false));
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Bool(false));
}

#[test]
fn test_three_valued_logic_or() {
    let (mut storage, mut txn) = setup();
    // TRUE OR NULL = TRUE, FALSE OR NULL = NULL
    let r = rows(run(
        "SELECT TRUE OR NULL, FALSE OR NULL, NULL OR TRUE",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Bool(true));
    assert_eq!(r[0][1], Value::Null);
    assert_eq!(r[0][2], Value::Bool(true));
}

// ── String concatenation ─────────────────────────────────────────────────────

#[test]
fn test_string_concat() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT 'Hello' || ', ' || 'World'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("Hello, World".into()));
}

#[test]
fn test_string_concat_with_column() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (fname TEXT, lname TEXT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES ('Alice', 'Smith')",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT fname || ' ' || lname FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("Alice Smith".into()));
}

// ── CAST ─────────────────────────────────────────────────────────────────────

#[test]
fn test_cast_text_to_int_valid() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT CAST('42' AS INT)", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn test_cast_text_to_bigint() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT CAST('999' AS BIGINT)", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(999));
}

#[test]
fn test_cast_int_to_text_not_supported_in_strict() {
    // CAST uses strict mode; Int→Text conversion is not allowed in strict mode.
    // Use CAST with supported conversions (Text→Int is supported, Int→Text is not).
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT CAST(123 AS TEXT)", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::InvalidCoercion { .. })));
}

#[test]
fn test_cast_invalid_text_to_int() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT CAST('abc' AS INT)", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::InvalidCoercion { .. })));
}

// ── Scalar functions ──────────────────────────────────────────────────────────

#[test]
fn test_abs_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT ABS(-42), ABS(7), ABS(0)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(42));
    assert_eq!(r[0][1], Value::Int(7));
    assert_eq!(r[0][2], Value::Int(0));
}

#[test]
fn test_length_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT LENGTH('hello'), LENGTH(''), LENGTH(NULL)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(5)); // LENGTH returns Int
    assert_eq!(r[0][1], Value::Int(0));
    assert_eq!(r[0][2], Value::Null);
}

#[test]
fn test_upper_lower() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT UPPER('hello'), LOWER('WORLD')",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("HELLO".into()));
    assert_eq!(r[0][1], Value::Text("world".into()));
}

#[test]
fn test_substr_function() {
    let (mut storage, mut txn) = setup();
    // SUBSTR(str, start, len) — 1-based
    let r = rows(run(
        "SELECT SUBSTR('hello world', 7, 5)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Text("world".into()));
}

#[test]
fn test_trim_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT TRIM('  hello  ')", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Text("hello".into()));
}

#[test]
fn test_coalesce_returns_first_non_null() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT COALESCE(NULL, NULL, 42, 99)",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(42));
}

#[test]
fn test_coalesce_all_null() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT COALESCE(NULL, NULL)", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Null);
}

#[test]
fn test_round_function() {
    let (mut storage, mut txn) = setup();
    let r = rows(run(
        "SELECT ROUND(3.7), ROUND(3.2), ROUND(-1.5)",
        &mut storage,
        &mut txn,
    ));
    // Rounding behavior: nearest integer
    assert!(matches!(r[0][0], Value::Real(v) if (v - 4.0).abs() < 0.001));
    assert!(matches!(r[0][1], Value::Real(v) if (v - 3.0).abs() < 0.001));
}

#[test]
fn test_floor_ceil_functions() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT FLOOR(3.9), CEIL(3.1)", &mut storage, &mut txn));
    assert!(matches!(r[0][0], Value::Real(v) if (v - 3.0).abs() < 0.001));
    assert!(matches!(r[0][1], Value::Real(v) if (v - 4.0).abs() < 0.001));
}

#[test]
fn test_now_returns_non_null() {
    let (mut storage, mut txn) = setup();
    let r = rows(run("SELECT NOW()", &mut storage, &mut txn));
    assert!(!matches!(r[0][0], Value::Null));
}

// ── Division by zero ──────────────────────────────────────────────────────────

#[test]
fn test_division_by_zero_error() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT 1 / 0", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::DivisionByZero)));
}

#[test]
fn test_modulo_by_zero_error() {
    let (mut storage, mut txn) = setup();
    let err = run_result("SELECT 5 % 0", &mut storage, &mut txn);
    assert!(matches!(err, Err(DbError::DivisionByZero)));
}

// ── Complex WHERE with AND/OR/NOT ─────────────────────────────────────────────

#[test]
fn test_complex_where_and_or() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (a INT, b INT, c INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES (1,2,3),(4,5,6),(7,8,9),(10,11,12)",
        &mut storage,
        &mut txn,
    );

    // (a < 5 OR a > 8) AND b != 11
    let r = rows(run(
        "SELECT a FROM t WHERE (a < 5 OR a > 8) AND b <> 11 ORDER BY a",
        &mut storage,
        &mut txn,
    ));
    // (a < 5 OR a > 8) AND b <> 11:
    //   a=1:  (T OR F) AND 2≠11=T  → included
    //   a=4:  (T OR F) AND 5≠11=T  → included
    //   a=7:  (F OR F) AND ...=F   → excluded
    //   a=10: (F OR T) AND 11≠11=F → excluded
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][0], Value::Int(1));
    assert_eq!(r[1][0], Value::Int(4));
}

#[test]
fn test_not_operator_in_where() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (active BOOL)", &mut storage, &mut txn);
    run(
        "INSERT INTO t VALUES (TRUE), (FALSE), (TRUE)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT COUNT(*) FROM t WHERE NOT active",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::BigInt(1));
}

// ── SELECT with computed expressions ─────────────────────────────────────────

#[test]
fn test_select_arithmetic_expression() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (price INT, qty INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO t VALUES (10, 3), (5, 7)",
        &mut storage,
        &mut txn,
    );

    // ORDER BY alias not supported yet — use expression directly
    let r = rows(run(
        "SELECT price * qty FROM t ORDER BY price * qty",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    // 5*7=35, 10*3=30 → ORDER BY ascending: 30, 35
    assert_eq!(r[0][0], Value::Int(30));
    assert_eq!(r[1][0], Value::Int(35));
}

#[test]
fn test_select_with_alias() {
    let (mut storage, mut txn) = setup();
    run("CREATE TABLE t (x INT)", &mut storage, &mut txn);
    run("INSERT INTO t VALUES (5)", &mut storage, &mut txn);

    let r = rows(run(
        "SELECT x * 2 AS doubled FROM t",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(10));
}

// ── HAVING with aggregate condition ──────────────────────────────────────────

#[test]
fn test_having_count_greater_than() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE orders (customer_id INT, amount INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO orders VALUES (1,100),(1,200),(2,50),(3,75),(3,25),(3,100)",
        &mut storage,
        &mut txn,
    );

    // Customers with more than 2 orders
    let r = rows(run(
        "SELECT customer_id FROM orders GROUP BY customer_id HAVING COUNT(*) > 2 ORDER BY customer_id",
        &mut storage, &mut txn
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(3));
}

#[test]
fn test_having_sum_filter() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE sales (dept INT, amount INT)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO sales VALUES (1,100),(1,150),(2,50),(2,30)",
        &mut storage,
        &mut txn,
    );

    let r = rows(run(
        "SELECT dept FROM sales GROUP BY dept HAVING SUM(amount) > 200 ORDER BY dept",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 1);
    assert_eq!(r[0][0], Value::Int(1)); // dept 1: 100+150=250 > 200 ✓
}

// ── INSERT DEFAULT values ─────────────────────────────────────────────────────

#[test]
fn test_insert_with_column_default() {
    let (mut storage, mut txn) = setup();
    run(
        "CREATE TABLE t (id INT, status TEXT DEFAULT 'pending')",
        &mut storage,
        &mut txn,
    );
    // Insert without specifying status — should use default
    run("INSERT INTO t (id) VALUES (1)", &mut storage, &mut txn);

    let r = rows(run("SELECT id, status FROM t", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(1));
    // status defaults to NULL when not specified via col list (no DEFAULT in executor yet)
    // This test verifies the current behavior (NULL for omitted columns)
    assert_eq!(r[0][1], Value::Null);
}

// ── Full round-trip: CREATE → INSERT → SELECT → UPDATE → DELETE ──────────────

#[test]
fn test_full_sql_suite_roundtrip() {
    let (mut storage, mut txn) = setup();

    // Schema
    run("CREATE TABLE products (id INT AUTO_INCREMENT, name TEXT NOT NULL, price INT, stock INT DEFAULT 0)", &mut storage, &mut txn);
    run(
        "CREATE TABLE categories (id INT, name TEXT UNIQUE)",
        &mut storage,
        &mut txn,
    );

    // Insert
    run(
        "INSERT INTO products (name, price, stock) VALUES ('Widget', 100, 50)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products (name, price, stock) VALUES ('Gadget', 200, 25)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO products (name, price, stock) VALUES ('Doohickey', 50, 100)",
        &mut storage,
        &mut txn,
    );
    run(
        "INSERT INTO categories VALUES (1, 'Electronics')",
        &mut storage,
        &mut txn,
    );

    // Select with WHERE + ORDER
    let r = rows(run(
        "SELECT name, price FROM products WHERE price > 75 ORDER BY price",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r.len(), 2);
    assert_eq!(r[0][1], Value::Int(100));
    assert_eq!(r[1][1], Value::Int(200));

    // Aggregate
    let r = rows(run(
        "SELECT COUNT(*), AVG(price), MAX(stock) FROM products",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::BigInt(3));
    // AVG(100+200+50)/3 = 116.67
    assert!(matches!(r[0][1], Value::Real(_)));
    assert_eq!(r[0][2], Value::Int(100));

    // Update
    run(
        "UPDATE products SET price = price - 10 WHERE price > 100",
        &mut storage,
        &mut txn,
    );
    let r = rows(run(
        "SELECT price FROM products WHERE name = 'Gadget'",
        &mut storage,
        &mut txn,
    ));
    assert_eq!(r[0][0], Value::Int(190));

    // Delete
    run(
        "DELETE FROM products WHERE stock > 75",
        &mut storage,
        &mut txn,
    );
    let r = rows(run("SELECT COUNT(*) FROM products", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::BigInt(2));

    // Note: UNIQUE constraints not yet enforced at executor level (deferred)
    // so duplicate inserts currently succeed. Verified above via other tests.

    // Truncate + auto_increment reset
    run("TRUNCATE TABLE products", &mut storage, &mut txn);
    run(
        "INSERT INTO products (name, price) VALUES ('Reset', 1)",
        &mut storage,
        &mut txn,
    );
    let r = rows(run("SELECT id FROM products", &mut storage, &mut txn));
    assert_eq!(r[0][0], Value::Int(1)); // reset to 1 after TRUNCATE
}
