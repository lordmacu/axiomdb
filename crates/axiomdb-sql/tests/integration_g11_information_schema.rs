//! Integration tests for Phase 4 G11 — INFORMATION_SCHEMA virtual tables (4.20c).
//!
//! Covers all 6 virtual tables:
//!   TABLES, COLUMNS, KEY_COLUMN_USAGE, TABLE_CONSTRAINTS,
//!   REFERENTIAL_CONSTRAINTS, STATISTICS

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

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
        let mut storage = MemoryStorage::new();
        CatalogBootstrap::init(&mut storage).unwrap();
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
            &mut self.storage,
            &mut self.txn,
            &mut self.bloom,
            &mut self.ctx,
        )
    }

    fn exec(&mut self, sql: &str) {
        self.run(sql)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        match self.run(sql).unwrap() {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected Rows, got {other:?}"),
        }
    }

    fn cols(&mut self, sql: &str) -> Vec<String> {
        match self.run(sql).unwrap() {
            QueryResult::Rows { columns, .. } => columns.into_iter().map(|c| c.name).collect(),
            other => panic!("expected Rows, got {other:?}"),
        }
    }
}

fn str_val(v: &Value) -> &str {
    match v {
        Value::Text(s) => s.as_str(),
        Value::Null => "",
        other => panic!("expected Text, got {other:?}"),
    }
}

// ── INFORMATION_SCHEMA.TABLES ────────────────────────────────────────────────

#[test]
fn test_is_tables_returns_rows() {
    let mut db = Db::new();
    db.exec("CREATE TABLE users (id INT PRIMARY KEY, name TEXT)");
    db.exec("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT)");

    let rows =
        db.rows("SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'axiomdb'");
    let names: Vec<&str> = rows.iter().map(|r| str_val(&r[0])).collect();
    assert!(names.contains(&"users"), "users not in TABLES: {names:?}");
    assert!(names.contains(&"orders"), "orders not in TABLES: {names:?}");
}

#[test]
fn test_is_tables_table_type_base_table() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t1 (id INT PRIMARY KEY)");

    let rows = db.rows(
        "SELECT TABLE_NAME, TABLE_TYPE FROM information_schema.TABLES WHERE TABLE_NAME = 'T1' OR TABLE_NAME = 't1'"
    );
    assert!(!rows.is_empty(), "t1 not found in TABLES");
    let ttype = str_val(&rows[0][1]);
    assert_eq!(ttype, "BASE TABLE", "TABLE_TYPE should be BASE TABLE");
}

#[test]
fn test_is_tables_engine_column() {
    let mut db = Db::new();
    db.exec("CREATE TABLE eng_test (id INT)");

    let rows =
        db.rows("SELECT ENGINE FROM information_schema.TABLES WHERE TABLE_NAME = 'eng_test'");
    assert!(!rows.is_empty(), "eng_test not found in TABLES");
    // ENGINE should be non-null — AxiomDB reports "AxiomDB"
    let engine = str_val(&rows[0][0]);
    assert!(!engine.is_empty(), "ENGINE should not be empty");
}

#[test]
fn test_is_tables_has_21_columns() {
    let mut db = Db::new();
    let cols = db.cols("SELECT * FROM information_schema.TABLES LIMIT 0");
    assert_eq!(
        cols.len(),
        21,
        "TABLES should have 21 columns, got {}",
        cols.len()
    );
}

#[test]
fn test_is_tables_where_table_schema_filter() {
    let mut db = Db::new();
    db.exec("CREATE TABLE filtered_tbl (id INT)");

    // Filter by a non-existent schema — should return 0 rows
    let rows = db.rows(
        "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'nonexistent_schema_xyz'"
    );
    assert!(
        rows.is_empty(),
        "Expected 0 rows for unknown schema, got {}",
        rows.len()
    );
}

// ── INFORMATION_SCHEMA.COLUMNS ───────────────────────────────────────────────

#[test]
fn test_is_columns_lists_columns() {
    let mut db = Db::new();
    db.exec("CREATE TABLE col_test (id INT, name TEXT, score FLOAT)");

    let rows = db.rows(
        "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_NAME = 'col_test' ORDER BY ORDINAL_POSITION"
    );
    let names: Vec<&str> = rows.iter().map(|r| str_val(&r[0])).collect();
    assert_eq!(names, vec!["id", "name", "score"]);
}

#[test]
fn test_is_columns_data_type_mapping() {
    let mut db = Db::new();
    db.exec("CREATE TABLE types_test (a INT, b BIGINT, c TEXT, d FLOAT, e BOOL)");

    let rows = db.rows(
        "SELECT COLUMN_NAME, DATA_TYPE FROM information_schema.COLUMNS WHERE TABLE_NAME = 'types_test' ORDER BY ORDINAL_POSITION"
    );
    assert_eq!(rows.len(), 5);
    assert_eq!(str_val(&rows[0][1]), "int");
    assert_eq!(str_val(&rows[1][1]), "bigint");
    assert_eq!(str_val(&rows[2][1]), "text");
    // AxiomDB's ColumnType::Float maps to "double" (64-bit), MySQL uses "float" for FLOAT
    let float_dt = str_val(&rows[3][1]);
    assert!(
        float_dt == "float" || float_dt == "double",
        "unexpected FLOAT data type: {float_dt}"
    );
    // bool maps to tinyint in MySQL compatibility
    let bool_dt = str_val(&rows[4][1]);
    assert!(
        bool_dt == "tinyint" || bool_dt == "bool" || bool_dt == "boolean",
        "unexpected BOOL data type: {bool_dt}"
    );
}

#[test]
fn test_is_columns_ordinal_position() {
    let mut db = Db::new();
    db.exec("CREATE TABLE ord_test (x INT, y INT, z INT)");

    let rows = db.rows(
        "SELECT COLUMN_NAME, ORDINAL_POSITION FROM information_schema.COLUMNS WHERE TABLE_NAME = 'ord_test' ORDER BY ORDINAL_POSITION"
    );
    assert_eq!(rows.len(), 3);
    // ORDINAL_POSITION is 1-based
    for (i, row) in rows.iter().enumerate() {
        match &row[1] {
            Value::Int(n) => assert_eq!(*n as usize, i + 1, "ORDINAL_POSITION should be 1-based"),
            Value::BigInt(n) => assert_eq!(*n as usize, i + 1),
            Value::Text(s) => {
                let n: usize = s.parse().expect("ORDINAL_POSITION should be numeric");
                assert_eq!(n, i + 1);
            }
            other => panic!("unexpected ORDINAL_POSITION type: {other:?}"),
        }
    }
}

#[test]
fn test_is_columns_has_22_columns() {
    let mut db = Db::new();
    let cols = db.cols("SELECT * FROM information_schema.COLUMNS LIMIT 0");
    assert_eq!(
        cols.len(),
        22,
        "COLUMNS should have 22 columns, got {}",
        cols.len()
    );
}

#[test]
fn test_is_columns_nullable() {
    let mut db = Db::new();
    db.exec("CREATE TABLE nullable_test (id INT NOT NULL, opt TEXT)");

    let rows = db.rows(
        "SELECT COLUMN_NAME, IS_NULLABLE FROM information_schema.COLUMNS WHERE TABLE_NAME = 'nullable_test' ORDER BY ORDINAL_POSITION"
    );
    assert_eq!(rows.len(), 2);
    let id_nullable = str_val(&rows[0][1]);
    let opt_nullable = str_val(&rows[1][1]);
    assert_eq!(
        id_nullable, "NO",
        "id is NOT NULL so IS_NULLABLE should be NO"
    );
    assert_eq!(
        opt_nullable, "YES",
        "opt is nullable so IS_NULLABLE should be YES"
    );
}

// ── INFORMATION_SCHEMA.KEY_COLUMN_USAGE ──────────────────────────────────────

#[test]
fn test_is_kcu_primary_key() {
    let mut db = Db::new();
    db.exec("CREATE TABLE kcu_test (id INT PRIMARY KEY, val TEXT)");

    let rows = db.rows(
        "SELECT TABLE_NAME, COLUMN_NAME, CONSTRAINT_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_NAME = 'kcu_test'"
    );
    assert!(!rows.is_empty(), "No KEY_COLUMN_USAGE rows for kcu_test");
    let constraint = str_val(&rows[0][2]);
    assert_eq!(
        constraint, "PRIMARY",
        "PK constraint name should be PRIMARY"
    );
    let col = str_val(&rows[0][1]);
    assert_eq!(col, "id");
}

#[test]
fn test_is_kcu_unique_index() {
    let mut db = Db::new();
    db.exec("CREATE TABLE kcu_uniq (id INT PRIMARY KEY, email TEXT UNIQUE)");

    let rows = db.rows(
        "SELECT COLUMN_NAME, CONSTRAINT_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE TABLE_NAME = 'kcu_uniq' ORDER BY CONSTRAINT_NAME"
    );
    assert!(
        rows.len() >= 2,
        "Expected at least 2 constraints (PK + UNIQUE), got {}",
        rows.len()
    );
    let constraints: Vec<&str> = rows.iter().map(|r| str_val(&r[1])).collect();
    assert!(
        constraints.contains(&"PRIMARY"),
        "PRIMARY constraint not found"
    );
}

#[test]
fn test_is_kcu_has_12_columns() {
    let mut db = Db::new();
    let cols = db.cols("SELECT * FROM information_schema.KEY_COLUMN_USAGE LIMIT 0");
    assert_eq!(
        cols.len(),
        12,
        "KEY_COLUMN_USAGE should have 12 columns, got {}",
        cols.len()
    );
}

// ── INFORMATION_SCHEMA.TABLE_CONSTRAINTS ─────────────────────────────────────

#[test]
fn test_is_table_constraints_pk() {
    let mut db = Db::new();
    db.exec("CREATE TABLE tc_test (id INT PRIMARY KEY, name TEXT)");

    let rows = db.rows(
        "SELECT TABLE_NAME, CONSTRAINT_NAME, CONSTRAINT_TYPE FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_NAME = 'tc_test'"
    );
    assert!(!rows.is_empty(), "No TABLE_CONSTRAINTS rows for tc_test");
    let found_pk = rows.iter().any(|r| str_val(&r[2]) == "PRIMARY KEY");
    assert!(found_pk, "No PRIMARY KEY constraint in TABLE_CONSTRAINTS");
}

#[test]
fn test_is_table_constraints_unique() {
    let mut db = Db::new();
    db.exec("CREATE TABLE tc_uniq (id INT PRIMARY KEY, code TEXT UNIQUE)");

    let rows = db.rows(
        "SELECT CONSTRAINT_TYPE FROM information_schema.TABLE_CONSTRAINTS WHERE TABLE_NAME = 'tc_uniq'"
    );
    let types: Vec<&str> = rows.iter().map(|r| str_val(&r[0])).collect();
    assert!(
        types.contains(&"PRIMARY KEY"),
        "PRIMARY KEY not found: {types:?}"
    );
    assert!(types.contains(&"UNIQUE"), "UNIQUE not found: {types:?}");
}

#[test]
fn test_is_table_constraints_has_7_columns() {
    let mut db = Db::new();
    let cols = db.cols("SELECT * FROM information_schema.TABLE_CONSTRAINTS LIMIT 0");
    assert_eq!(
        cols.len(),
        7,
        "TABLE_CONSTRAINTS should have 7 columns, got {}",
        cols.len()
    );
}

// ── INFORMATION_SCHEMA.REFERENTIAL_CONSTRAINTS ───────────────────────────────

#[test]
fn test_is_referential_constraints_empty() {
    let mut db = Db::new();
    db.exec("CREATE TABLE rc_parent (id INT PRIMARY KEY)");
    db.exec("CREATE TABLE rc_child (id INT PRIMARY KEY, parent_id INT)");

    // FK not yet surfaced — should return empty rows (not an error)
    let rows = db.rows("SELECT * FROM information_schema.REFERENTIAL_CONSTRAINTS");
    // May be empty or may have rows if FK support added later
    let _ = rows; // just verify no error
}

#[test]
fn test_is_referential_constraints_has_11_columns() {
    let mut db = Db::new();
    let cols = db.cols("SELECT * FROM information_schema.REFERENTIAL_CONSTRAINTS LIMIT 0");
    assert_eq!(
        cols.len(),
        11,
        "REFERENTIAL_CONSTRAINTS should have 11 columns, got {}",
        cols.len()
    );
}

// ── INFORMATION_SCHEMA.STATISTICS ────────────────────────────────────────────

#[test]
fn test_is_statistics_pk_index() {
    let mut db = Db::new();
    db.exec("CREATE TABLE stat_test (id INT PRIMARY KEY, name TEXT)");

    let rows = db.rows(
        "SELECT TABLE_NAME, INDEX_NAME, COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_NAME = 'stat_test'"
    );
    assert!(!rows.is_empty(), "No STATISTICS rows for stat_test");
    let found_pk = rows.iter().any(|r| str_val(&r[1]) == "PRIMARY");
    assert!(found_pk, "PRIMARY index not found in STATISTICS");
}

#[test]
fn test_is_statistics_secondary_index() {
    let mut db = Db::new();
    db.exec("CREATE TABLE stat_idx (id INT PRIMARY KEY, email TEXT)");
    db.exec("CREATE INDEX idx_email ON stat_idx (email)");

    let rows = db.rows(
        "SELECT INDEX_NAME, COLUMN_NAME FROM information_schema.STATISTICS WHERE TABLE_NAME = 'stat_idx' ORDER BY INDEX_NAME"
    );
    let index_names: Vec<&str> = rows.iter().map(|r| str_val(&r[0])).collect();
    assert!(
        index_names.contains(&"PRIMARY"),
        "PRIMARY missing from STATISTICS: {index_names:?}"
    );
    assert!(
        index_names.contains(&"idx_email"),
        "idx_email missing from STATISTICS: {index_names:?}"
    );
}

#[test]
fn test_is_statistics_has_18_columns() {
    let mut db = Db::new();
    let cols = db.cols("SELECT * FROM information_schema.STATISTICS LIMIT 0");
    assert_eq!(
        cols.len(),
        18,
        "STATISTICS should have 18 columns, got {}",
        cols.len()
    );
}

// ── Cross-cutting: ORDER BY, WHERE, LIMIT work on virtual tables ──────────────

#[test]
fn test_is_tables_order_by_table_name() {
    let mut db = Db::new();
    db.exec("CREATE TABLE zzz_last (id INT)");
    db.exec("CREATE TABLE aaa_first (id INT)");

    let rows = db.rows(
        "SELECT TABLE_NAME FROM information_schema.TABLES WHERE TABLE_SCHEMA = 'axiomdb' ORDER BY TABLE_NAME"
    );
    assert!(rows.len() >= 2);
    let names: Vec<&str> = rows.iter().map(|r| str_val(&r[0])).collect();
    // aaa_first must come before zzz_last
    let pos_aaa = names
        .iter()
        .position(|&n| n == "aaa_first")
        .expect("aaa_first not found");
    let pos_zzz = names
        .iter()
        .position(|&n| n == "zzz_last")
        .expect("zzz_last not found");
    assert!(
        pos_aaa < pos_zzz,
        "ORDER BY TABLE_NAME: aaa_first should come before zzz_last"
    );
}

#[test]
fn test_is_columns_limit() {
    let mut db = Db::new();
    db.exec("CREATE TABLE limit_test (a INT, b INT, c INT, d INT, e INT)");

    let all = db
        .rows("SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_NAME = 'limit_test'");
    let limited = db.rows(
        "SELECT COLUMN_NAME FROM information_schema.COLUMNS WHERE TABLE_NAME = 'limit_test' LIMIT 2"
    );
    assert_eq!(all.len(), 5);
    assert_eq!(limited.len(), 2, "LIMIT 2 should return exactly 2 rows");
}

#[test]
fn test_is_columns_count_aggregate() {
    let mut db = Db::new();
    db.exec("CREATE TABLE agg_test (a INT, b INT, c INT)");

    let rows =
        db.rows("SELECT COUNT(*) FROM information_schema.COLUMNS WHERE TABLE_NAME = 'agg_test'");
    assert_eq!(rows.len(), 1);
    let count = match &rows[0][0] {
        Value::Int(n) => *n as u64,
        Value::BigInt(n) => *n as u64,
        other => panic!("unexpected count type: {other:?}"),
    };
    assert_eq!(count, 3, "COUNT(*) should return 3 for agg_test");
}
