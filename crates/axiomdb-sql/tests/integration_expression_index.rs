//! Integration tests for expression indexes (Phase 21.8F).
//!
//! Tests cover:
//! - CREATE INDEX with expression (LOWER, UPPER, arithmetic) stores in catalog
//! - Planner uses expression index for predicate pushdown (equality and LIKE)
//! - DML (INSERT/UPDATE/DELETE) maintains expression index
//! - Combined partial + expression index
//! - Multi-column expression (concat)
//! - Rejection of disallowed constructs (subquery, aggregate, window function)
//! - UNIQUE expression index with duplicate detection

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── Test helper ───────────────────────────────────────────────────────────────

struct Db {
    storage: MemoryStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    ctx: SessionContext,
    _dir: tempfile::TempDir,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("t.wal");
        let storage = MemoryStorage::new();
        CatalogBootstrap::init(&storage).unwrap();
        let txn = TxnManager::create(&wal).unwrap();
        Self {
            storage,
            txn,
            bloom: BloomRegistry::new(),
            ctx: SessionContext::new(),
            _dir: dir,
        }
    }

    fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let stmt = parse(sql, None)?;
        let snap = if let Some(ref ct) = self.ctx.conn_txn {
            self.txn.active_snapshot(ct)
        } else {
            self.txn.snapshot()
        };
        let analyzed = analyze(stmt, &self.storage, snap)?;
        execute_with_ctx(
            analyzed,
            &self.storage,
            &self.txn,
            &self.bloom,
            &mut self.ctx,
        )
    }

    fn ok(&mut self, sql: &str) -> QueryResult {
        self.run(sql)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e}"))
    }

    fn err(&mut self, sql: &str) -> DbError {
        self.run(sql)
            .expect_err(&format!("expected error for: {sql}"))
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        match self.ok(sql) {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }
}

macro_rules! setup {
    ($db:expr, $($sql:expr),+) => { $( $db.ok($sql); )+ };
}

// ── DDL: catalog storage ─────────────────────────────────────────────────────

#[test]
fn expr_index_stores_expression_in_catalog() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)");
    db.ok("CREATE INDEX idx_lower_email ON t (LOWER(email))");

    let snap = db.txn.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "t").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let idx = indexes
        .iter()
        .find(|i| i.name == "idx_lower_email")
        .unwrap();
    assert!(
        idx.columns.len() == 1,
        "expression index has exactly 1 column"
    );
    let col = &idx.columns[0];
    assert!(
        col.expr.as_deref() == Some("LOWER(email)"),
        "expression stored in catalog: {:?}",
        col.expr
    );
}

#[test]
fn expr_index_arithmetic_expression_stored() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE orders (id INT PRIMARY KEY, price DOUBLE, qty INT)"
    );
    db.ok("CREATE INDEX idx_total ON orders (price * qty)");

    let snap = db.txn.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "orders").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let idx = indexes.iter().find(|i| i.name == "idx_total").unwrap();
    let col = &idx.columns[0];
    assert!(
        col.expr.as_deref() == Some("price * qty"),
        "arithmetic expression stored: {:?}",
        col.expr
    );
}

// ── Planner: equality predicate pushdown ─────────────────────────────────────

#[test]
fn expr_index_lower_equality_used_by_planner() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)",
        "CREATE INDEX idx_lower_email ON users (LOWER(email))",
        "INSERT INTO users VALUES (1, 'Alice@Example.COM')",
        "INSERT INTO users VALUES (2, 'bob@example.com')",
        "INSERT INTO users VALUES (3, 'CAROL@EXAMPLE.COM')"
    );

    // Query that matches the expression index: LOWER(email) = 'alice@example.com'
    let result = db.rows("SELECT id, email FROM users WHERE LOWER(email) = 'alice@example.com'");
    assert_eq!(
        result.len(),
        1,
        "planner uses expression index for equality — exactly 1 match"
    );
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[0][1], Value::Text("Alice@Example.COM".into()));
}

#[test]
fn expr_index_upper_equality_used_by_planner() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
        "CREATE INDEX idx_upper_name ON t (UPPER(name))",
        "INSERT INTO t VALUES (1, 'alice')",
        "INSERT INTO t VALUES (2, 'Bob')"
    );

    let result = db.rows("SELECT id FROM t WHERE UPPER(name) = 'ALICE'");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));
}

// ── Planner: LIKE prefix range ───────────────────────────────────────────────

#[test]
fn expr_index_like_prefix_used_by_planner() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, name TEXT)",
        "CREATE INDEX idx_lower_name ON t (LOWER(name))",
        "INSERT INTO t VALUES (1, 'Alice')",
        "INSERT INTO t VALUES (2, 'Bob')",
        "INSERT INTO t VALUES (3, 'Alicia')",
        "INSERT INTO t VALUES (4, 'albert')"
    );

    // Prefix LIKE: LOWER(name) LIKE 'ali%' should use IndexRange
    let result = db.rows("SELECT id, name FROM t WHERE LOWER(name) LIKE 'ali%' ORDER BY id");
    assert_eq!(
        result.len(),
        2,
        "planner uses expression index for LIKE prefix — 2 rows match 'ali*'"
    );
    assert_eq!(result[0][0], Value::Int(1)); // Alice
    assert_eq!(result[1][0], Value::Int(3)); // Alicia
}

// ── DML maintenance ───────────────────────────────────────────────────────────

#[test]
fn expr_index_insert_maintains_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX idx_lower_email ON t (LOWER(email))"
    );

    db.ok("INSERT INTO t VALUES (1, 'Alice@Example.COM')");
    db.ok("INSERT INTO t VALUES (2, 'bob@example.com')");

    // Duplicate email (case-insensitive) — must be caught by expression index
    let err = db.err("INSERT INTO t VALUES (3, 'ALICE@EXAMPLE.COM')");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "duplicate LOWER(email) should violate unique expression index: {err}"
    );
}

#[test]
fn expr_index_delete_maintains_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX idx_lower_email ON t (LOWER(email))",
        "INSERT INTO t VALUES (1, 'alice@example.com')",
        "INSERT INTO t VALUES (2, 'bob@example.com')"
    );

    // Delete the existing alice — frees the unique constraint slot
    db.ok("DELETE FROM t WHERE id = 1");

    // Now we can insert a new alice
    db.ok("INSERT INTO t VALUES (3, 'ALICE@EXAMPLE.COM')");

    let rows = db.rows("SELECT id FROM t WHERE LOWER(email) = 'alice@example.com' ORDER BY id");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(3));
}

#[test]
fn expr_index_update_maintains_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX idx_lower_email ON t (LOWER(email))",
        "INSERT INTO t VALUES (1, 'alice@example.com')",
        "INSERT INTO t VALUES (2, 'bob@example.com')"
    );

    // Update bob to alice's email — must fail unique constraint
    let err = db.err("UPDATE t SET email = 'Alice@Example.COM' WHERE id = 2");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "UPDATE to duplicate LOWER(email) violates unique expression index: {err}"
    );

    // Update to a new unique email — should succeed
    db.ok("UPDATE t SET email = 'charlie@example.com' WHERE id = 2");

    let rows = db.rows("SELECT id FROM t WHERE LOWER(email) = 'charlie@example.com'");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(2));
}

// ── Arithmetic expression index ─────────────────────────────────────────────

#[test]
fn expr_index_arithmetic_equality() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE orders (id INT PRIMARY KEY, price DOUBLE, qty INT)",
        "CREATE INDEX idx_total ON orders (price * qty)",
        "INSERT INTO orders VALUES (1, 10.0, 100)", // total = 1000
        "INSERT INTO orders VALUES (2, 5.0, 50)",   // total = 250
        "INSERT INTO orders VALUES (3, 20.0, 30)"   // total = 600
    );

    let result = db.rows("SELECT id FROM orders WHERE price * qty = 1000.0");
    assert_eq!(
        result.len(),
        1,
        "expression index used for price * qty = 1000"
    );
    assert_eq!(result[0][0], Value::Int(1));
}

#[test]
fn expr_index_arithmetic_range() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE orders (id INT PRIMARY KEY, price DOUBLE, qty INT)",
        "CREATE INDEX idx_total ON orders (price * qty)",
        "INSERT INTO orders VALUES (1, 10.0, 100)", // 1000
        "INSERT INTO orders VALUES (2, 5.0, 50)",   // 250
        "INSERT INTO orders VALUES (3, 20.0, 30)",  // 600
        "INSERT INTO orders VALUES (4, 3.0, 500)"   // 1500
    );

    // Range scan: price * qty > 1000 → only id=4 (1500) qualifies.
    // id=1 yields exactly 1000 which is not strictly greater than 1000.
    let result = db.rows("SELECT id FROM orders WHERE price * qty > 1000 ORDER BY id");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(4)); // 1500
}

// ── Combined partial + expression index ──────────────────────────────────────

#[test]
fn expr_index_combined_with_partial() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, active BOOL)",
        "CREATE INDEX idx_lower_email_active ON users (LOWER(email)) WHERE active = TRUE",
        "INSERT INTO users VALUES (1, 'alice@example.com', TRUE)",
        "INSERT INTO users VALUES (2, 'alice@example.com', FALSE)", // inactive — not in index
        "INSERT INTO users VALUES (3, 'bob@example.com', TRUE)"
    );

    // Query matching both the expression and the partial predicate
    let result =
        db.rows("SELECT id FROM users WHERE LOWER(email) = 'alice@example.com' AND active = TRUE");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Int(1));

    // Query without the partial predicate — must fall back to full scan or partial index
    // Both inactive and active alice rows should be visible (full scan)
    let all_alice =
        db.rows("SELECT id FROM users WHERE LOWER(email) = 'alice@example.com' ORDER BY id");
    assert_eq!(
        all_alice.len(),
        2,
        "query without predicate returns both active and inactive"
    );
}

// ── Multi-column expression ──────────────────────────────────────────────────

#[test]
fn expr_index_concatenation() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE contacts (id INT PRIMARY KEY, first_name TEXT, last_name TEXT)",
        "CREATE INDEX idx_full_name ON contacts (UPPER(first_name) || ' ' || UPPER(last_name))",
        "INSERT INTO contacts VALUES (1, 'John', 'Doe')",
        "INSERT INTO contacts VALUES (2, 'Jane', 'Smith')",
        "INSERT INTO contacts VALUES (3, 'john', 'doe')" // same after UPPER
    );

    // Exact match on concatenated, uppercased full name
    let result = db.rows(
        "SELECT id FROM contacts WHERE UPPER(first_name) || ' ' || UPPER(last_name) = 'JOHN DOE'",
    );
    assert_eq!(result.len(), 2, "both john doe entries match after UPPER");
}

// ── UNIQUE expression index ──────────────────────────────────────────────────

#[test]
fn expr_index_unique_violation() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX idx_unique_lower_email ON t (LOWER(email))",
        "INSERT INTO t VALUES (1, 'Test@Example.COM')"
    );

    let err = db.err("INSERT INTO t VALUES (2, 'TEST@EXAMPLE.COM')");
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "UNIQUE expression index catches case-insensitive duplicate: {err}"
    );
}

#[test]
fn expr_index_unique_allows_case_variants_when_no_duplicate() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "CREATE UNIQUE INDEX idx_unique_lower_email ON t (LOWER(email))",
        "INSERT INTO t VALUES (1, 'alice@example.com')",
        "INSERT INTO t VALUES (2, 'bob@example.com')"
    );

    // Two different emails that happen to lowercase to the same string — only one allowed
    let err = db.err("INSERT INTO t VALUES (3, 'ALICE@EXAMPLE.COM')");
    assert!(matches!(err, DbError::UniqueViolation { .. }));

    // Different emails are fine
    db.ok("INSERT INTO t VALUES (3, 'charlie@Example.COM')");
    let rows = db.rows("SELECT COUNT(*) FROM t");
    // Three rows were successfully inserted: alice, bob, charlie (the ALICE
    // duplicate was rejected above). COUNT(*) is promoted to BigInt.
    assert_eq!(rows[0][0], Value::BigInt(3));
}

// ── Expression index with build from existing rows ───────────────────────────

#[test]
fn expr_index_build_from_existing_rows() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, email TEXT)",
        "INSERT INTO t VALUES (1, 'Alice@Example.COM')",
        "INSERT INTO t VALUES (2, 'bob@example.com')"
    );

    // Create index AFTER rows exist — build must evaluate LOWER on existing data
    db.ok("CREATE UNIQUE INDEX idx_lower ON t (LOWER(email))");

    // Duplicate check should work immediately
    let err = db.err("INSERT INTO t VALUES (3, 'ALICE@EXAMPLE.COM')");
    assert!(matches!(err, DbError::UniqueViolation { .. }));
}

// ── Rejection of disallowed constructs ─────────────────────────────────────

#[test]
fn expr_index_rejects_subquery() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT, y INT)");

    // Subquery in expression — must be rejected at parse/compile time
    let err = db.err("CREATE INDEX idx_sub ON t ((SELECT MAX(x) FROM t))");
    // Error should mention subquery is not allowed
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("subquery") || msg.contains("select") || msg.contains("not allowed"),
        "subquery rejection should mention subquery: {err}"
    );
}

#[test]
fn expr_index_rejects_aggregate() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");

    let err = db.err("CREATE INDEX idx_agg ON t (COUNT(x))");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("aggregate") || msg.contains("count") || msg.contains("not allowed"),
        "aggregate rejection should mention aggregate: {err}"
    );
}

#[test]
fn expr_index_rejects_window_function() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");

    let err = db.err("CREATE INDEX idx_win ON t (RANK() OVER (ORDER BY x))");
    let msg = err.to_string().to_lowercase();
    assert!(
        msg.contains("window") || msg.contains("over") || msg.contains("not allowed"),
        "window function rejection should mention window function: {err}"
    );
}

// ── Non-expression index unchanged ─────────────────────────────────────────

#[test]
fn regular_index_still_works() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, x INT, y TEXT)",
        "CREATE INDEX idx_x ON t (x)",
        "INSERT INTO t VALUES (1, 10, 'a')",
        "INSERT INTO t VALUES (2, 20, 'b')",
        "INSERT INTO t VALUES (3, 10, 'c')"
    );

    // Regular column index still works
    let result = db.rows("SELECT id FROM t WHERE x = 10 ORDER BY id");
    assert_eq!(result.len(), 2);
    assert_eq!(result[0][0], Value::Int(1));
    assert_eq!(result[1][0], Value::Int(3));
}

// ── Catalog roundtrip serde ─────────────────────────────────────────────────

#[test]
fn expr_index_def_roundtrip() {
    use axiomdb_catalog::schema::{IndexColumnDef, IndexDef, SortOrder};

    let def = IndexDef {
        index_id: 7,
        table_id: 3,
        name: "idx_lower_email".to_string(),
        root_page_id: 42,
        is_unique: true,
        is_primary: false,
        columns: vec![IndexColumnDef {
            col_idx: 1,
            order: SortOrder::Asc,
            expr: Some("LOWER(email)".to_string()),
        }],
        predicate: None,
        fillfactor: 90,
        is_fk_index: false,
        include_columns: vec![],
        index_type: 0,
        pages_per_range: 128,
    };

    let bytes = def.to_bytes();
    let (decoded, consumed) = IndexDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded.columns[0].expr.as_deref(), Some("LOWER(email)"));
}

#[test]
fn expr_index_roundtrip_with_partial() {
    use axiomdb_catalog::schema::{IndexColumnDef, IndexDef, SortOrder};

    let def = IndexDef {
        index_id: 9,
        table_id: 5,
        name: "idx_lower_active".to_string(),
        root_page_id: 77,
        is_unique: false,
        is_primary: false,
        columns: vec![IndexColumnDef {
            col_idx: 1,
            order: SortOrder::Asc,
            expr: Some("LOWER(email)".to_string()),
        }],
        predicate: Some("active = TRUE".to_string()),
        fillfactor: 90,
        is_fk_index: false,
        include_columns: vec![],
        index_type: 0,
        pages_per_range: 128,
    };

    let bytes = def.to_bytes();
    let (decoded, consumed) = IndexDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded.columns[0].expr.as_deref(), Some("LOWER(email)"));
    assert_eq!(decoded.predicate.as_deref(), Some("active = TRUE"));
}
