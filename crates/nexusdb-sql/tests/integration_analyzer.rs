//! Integration tests for the semantic analyzer (subfase 4.18).
//!
//! Uses MemoryStorage + CatalogBootstrap to create a real catalog,
//! then verifies that analyze() resolves col_idx correctly and rejects
//! invalid references.

use nexusdb_catalog::{
    bootstrap::CatalogBootstrap,
    schema::{ColumnDef, ColumnType},
    writer::CatalogWriter,
};
use nexusdb_core::{DbError, TransactionSnapshot};
use nexusdb_sql::{analyze, parse};
use nexusdb_storage::MemoryStorage;
use nexusdb_wal::TxnManager;

// ── Test fixture ──────────────────────────────────────────────────────────────

struct Fixture {
    storage: MemoryStorage,
    txn: TxnManager,
}

impl Fixture {
    fn new() -> Self {
        let mut storage = MemoryStorage::new();
        CatalogBootstrap::init(&mut storage).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("test.wal");
        let txn = TxnManager::create(&wal).unwrap();
        std::mem::forget(dir);
        Fixture { storage, txn }
    }

    /// Create a table with given columns and return table_id.
    fn create_table(&mut self, name: &str, cols: &[(&str, ColumnType)]) -> u32 {
        self.txn.begin().unwrap();
        let table_id = {
            let mut w = CatalogWriter::new(&mut self.storage, &mut self.txn).unwrap();
            let tid = w.create_table("public", name).unwrap();
            for (i, (col_name, col_type)) in cols.iter().enumerate() {
                w.create_column(ColumnDef {
                    table_id: tid,
                    col_idx: i as u16,
                    name: col_name.to_string(),
                    col_type: *col_type,
                    nullable: true,
                })
                .unwrap();
            }
            tid
        };
        self.txn.commit().unwrap();
        table_id
    }

    fn snapshot(&self) -> TransactionSnapshot {
        self.txn.snapshot()
    }

    fn analyze(&self, sql: &str) -> Result<nexusdb_sql::ast::Stmt, DbError> {
        let stmt = parse(sql, None).unwrap();
        analyze(stmt, &self.storage, self.snapshot())
    }

    fn analyze_err(&self, sql: &str) -> DbError {
        let stmt = parse(sql, None).unwrap();
        analyze(stmt, &self.storage, self.snapshot()).unwrap_err()
    }
}

// ── Single-table col_idx resolution ───────────────────────────────────────────

#[test]
fn test_single_table_resolves_col_idx() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[
            ("id", ColumnType::BigInt),
            ("name", ColumnType::Text),
            ("age", ColumnType::Int),
            ("email", ColumnType::Text),
        ],
    );

    // SELECT name FROM users WHERE age > 18
    let stmt = f.analyze("SELECT name FROM users WHERE age > 18").unwrap();
    if let nexusdb_sql::ast::Stmt::Select(s) = stmt {
        // WHERE: age > 18 — age is col 2
        if let Some(nexusdb_sql::expr::Expr::BinaryOp { left, .. }) = s.where_clause {
            if let nexusdb_sql::expr::Expr::Column { col_idx, name } = *left {
                assert_eq!(col_idx, 2, "age should be at index 2");
                assert_eq!(name, "age");
            } else {
                panic!("expected Column in WHERE left");
            }
        } else {
            panic!("expected BinaryOp in WHERE");
        }
        // SELECT: name is col 1
        if let nexusdb_sql::ast::SelectItem::Expr {
            expr: nexusdb_sql::expr::Expr::Column { col_idx, name },
            ..
        } = &s.columns[0]
        {
            assert_eq!(*col_idx, 1, "name should be at index 1");
            assert_eq!(name, "name");
        } else {
            panic!("expected Column in SELECT");
        }
    } else {
        panic!("expected Select stmt");
    }
}

// ── JOIN combined-row col_idx ─────────────────────────────────────────────────

#[test]
fn test_join_resolves_combined_col_idx() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[
            ("id", ColumnType::BigInt),  // col 0
            ("name", ColumnType::Text),  // col 1
            ("age", ColumnType::Int),    // col 2
            ("email", ColumnType::Text), // col 3
        ],
    );
    f.create_table(
        "orders",
        &[
            ("id", ColumnType::BigInt),      // col 0 in table; col 4 combined
            ("user_id", ColumnType::BigInt), // col 1 in table; col 5 combined
            ("total", ColumnType::Float),    // col 2 in table; col 6 combined
            ("status", ColumnType::Text),    // col 3 in table; col 7 combined
        ],
    );

    let sql = "SELECT u.name, o.total FROM users AS u JOIN orders AS o ON u.id = o.user_id";
    let stmt = f.analyze(sql).unwrap();

    if let nexusdb_sql::ast::Stmt::Select(s) = stmt {
        // u.name → col 1 (users offset=0, name at pos 1)
        if let nexusdb_sql::ast::SelectItem::Expr {
            expr: nexusdb_sql::expr::Expr::Column { col_idx, .. },
            ..
        } = &s.columns[0]
        {
            assert_eq!(*col_idx, 1, "u.name should be at combined index 1");
        } else {
            panic!("expected Column for u.name");
        }

        // o.total → col 6 (orders offset=4, total at pos 2 → 4+2=6)
        if let nexusdb_sql::ast::SelectItem::Expr {
            expr: nexusdb_sql::expr::Expr::Column { col_idx, .. },
            ..
        } = &s.columns[1]
        {
            assert_eq!(*col_idx, 6, "o.total should be at combined index 6");
        } else {
            panic!("expected Column for o.total");
        }

        // ON condition: u.id (0) = o.user_id (5)
        if let nexusdb_sql::ast::JoinCondition::On(nexusdb_sql::expr::Expr::BinaryOp {
            left,
            right,
            ..
        }) = &s.joins[0].condition
        {
            if let nexusdb_sql::expr::Expr::Column { col_idx, .. } = left.as_ref() {
                assert_eq!(*col_idx, 0, "u.id should be at combined index 0");
            }
            if let nexusdb_sql::expr::Expr::Column { col_idx, .. } = right.as_ref() {
                assert_eq!(*col_idx, 5, "o.user_id should be at combined index 5");
            }
        }
    } else {
        panic!("expected Select");
    }
}

// ── Alias resolution ──────────────────────────────────────────────────────────

#[test]
fn test_alias_resolves_correctly() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("age", ColumnType::Int)],
    );

    let stmt = f
        .analyze("SELECT u.age FROM users AS u WHERE u.id = 1")
        .unwrap();
    if let nexusdb_sql::ast::Stmt::Select(s) = stmt {
        if let nexusdb_sql::ast::SelectItem::Expr {
            expr: nexusdb_sql::expr::Expr::Column { col_idx, .. },
            ..
        } = &s.columns[0]
        {
            assert_eq!(*col_idx, 1, "u.age should resolve to index 1");
        }
    }
}

#[test]
fn test_implicit_alias_resolves_correctly() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("name", ColumnType::Text)],
    );

    // FROM users u (implicit alias, no AS)
    let stmt = f.analyze("SELECT u.name FROM users u").unwrap();
    if let nexusdb_sql::ast::Stmt::Select(s) = stmt {
        if let nexusdb_sql::ast::SelectItem::Expr {
            expr: nexusdb_sql::expr::Expr::Column { col_idx, .. },
            ..
        } = &s.columns[0]
        {
            assert_eq!(*col_idx, 1, "u.name should resolve to index 1");
        }
    }
}

// ── SELECT without FROM ───────────────────────────────────────────────────────

#[test]
fn test_select_without_from_literal_ok() {
    let f = Fixture::new();
    // SELECT 1 — no tables needed, literal resolves fine
    let stmt = f.analyze("SELECT 1").unwrap();
    assert!(matches!(stmt, nexusdb_sql::ast::Stmt::Select(_)));
}

// ── Wildcard ──────────────────────────────────────────────────────────────────

#[test]
fn test_select_wildcard_no_error() {
    let mut f = Fixture::new();
    f.create_table("users", &[("id", ColumnType::BigInt)]);
    let stmt = f.analyze("SELECT * FROM users").unwrap();
    assert!(matches!(stmt, nexusdb_sql::ast::Stmt::Select(_)));
}

// ── Error: unknown table ──────────────────────────────────────────────────────

#[test]
fn test_unknown_table_returns_error() {
    let f = Fixture::new();
    let err = f.analyze_err("SELECT * FROM usres");
    assert!(matches!(err, DbError::TableNotFound { .. }), "got: {err}");
}

#[test]
fn test_drop_table_unknown_without_if_exists_returns_error() {
    let f = Fixture::new();
    let err = f.analyze_err("DROP TABLE usres");
    assert!(matches!(err, DbError::TableNotFound { .. }), "got: {err}");
}

#[test]
fn test_drop_table_unknown_with_if_exists_ok() {
    let f = Fixture::new();
    // IF EXISTS: never fails even if table doesn't exist
    let stmt = f.analyze("DROP TABLE IF EXISTS nonexistent").unwrap();
    assert!(matches!(stmt, nexusdb_sql::ast::Stmt::DropTable(_)));
}

// ── Error: unknown column ─────────────────────────────────────────────────────

#[test]
fn test_unknown_column_returns_error() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[
            ("id", ColumnType::BigInt),
            ("name", ColumnType::Text),
            ("email", ColumnType::Text),
        ],
    );
    let err = f.analyze_err("SELECT eamil FROM users");
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "got: {err}");
}

#[test]
fn test_unknown_column_error_contains_table_name() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("email", ColumnType::Text)],
    );
    let err = f.analyze_err("SELECT eamil FROM users");
    if let DbError::ColumnNotFound { name, table } = err {
        assert_eq!(name, "eamil");
        assert!(
            table.contains("users"),
            "error should mention table: {table}"
        );
    } else {
        panic!("expected ColumnNotFound");
    }
}

// ── Error: ambiguous column ───────────────────────────────────────────────────

#[test]
fn test_ambiguous_column_returns_error() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("name", ColumnType::Text)],
    );
    f.create_table(
        "orders",
        &[("id", ColumnType::BigInt), ("total", ColumnType::Float)],
    );

    // "id" exists in both users and orders — ambiguous
    let err = f.analyze_err("SELECT id FROM users JOIN orders ON users.id = orders.id");
    assert!(matches!(err, DbError::AmbiguousColumn { .. }), "got: {err}");
}

#[test]
fn test_qualified_column_resolves_ambiguity() {
    let mut f = Fixture::new();
    f.create_table("users", &[("id", ColumnType::BigInt)]);
    f.create_table(
        "orders",
        &[("id", ColumnType::BigInt), ("total", ColumnType::Float)],
    );

    // users.id qualifies — no ambiguity
    let stmt = f
        .analyze("SELECT users.id FROM users JOIN orders ON users.id = orders.id")
        .unwrap();
    assert!(matches!(stmt, nexusdb_sql::ast::Stmt::Select(_)));
}

// ── INSERT validation ─────────────────────────────────────────────────────────

#[test]
fn test_insert_validates_column_list() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("email", ColumnType::Text)],
    );

    let err = f.analyze_err("INSERT INTO users (id, eamil) VALUES (1, 'x')");
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "got: {err}");
}

#[test]
fn test_insert_valid_column_list_ok() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("email", ColumnType::Text)],
    );

    let stmt = f
        .analyze("INSERT INTO users (id, email) VALUES (1, 'alice@x.com')")
        .unwrap();
    assert!(matches!(stmt, nexusdb_sql::ast::Stmt::Insert(_)));
}

#[test]
fn test_insert_unknown_table_returns_error() {
    let f = Fixture::new();
    let err = f.analyze_err("INSERT INTO usres (id) VALUES (1)");
    assert!(matches!(err, DbError::TableNotFound { .. }), "got: {err}");
}

// ── UPDATE validation ─────────────────────────────────────────────────────────

#[test]
fn test_update_validates_set_column() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("name", ColumnType::Text)],
    );

    let err = f.analyze_err("UPDATE users SET eamil = 'x' WHERE id = 1");
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "got: {err}");
}

#[test]
fn test_update_resolves_where_col_idx() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[
            ("id", ColumnType::BigInt),
            ("name", ColumnType::Text),
            ("age", ColumnType::Int),
        ],
    );

    let stmt = f
        .analyze("UPDATE users SET name = 'Alice' WHERE age > 18")
        .unwrap();
    if let nexusdb_sql::ast::Stmt::Update(u) = stmt {
        if let Some(nexusdb_sql::expr::Expr::BinaryOp { left, .. }) = u.where_clause {
            if let nexusdb_sql::expr::Expr::Column { col_idx, .. } = *left {
                assert_eq!(col_idx, 2, "age should be at index 2");
            }
        }
    }
}

// ── DELETE validation ─────────────────────────────────────────────────────────

#[test]
fn test_delete_resolves_where_col_idx() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[
            ("id", ColumnType::BigInt),
            ("name", ColumnType::Text),
            ("active", ColumnType::Bool),
        ],
    );

    let stmt = f.analyze("DELETE FROM users WHERE active = TRUE").unwrap();
    if let nexusdb_sql::ast::Stmt::Delete(d) = stmt {
        if let Some(nexusdb_sql::expr::Expr::BinaryOp { left, .. }) = d.where_clause {
            if let nexusdb_sql::expr::Expr::Column { col_idx, .. } = *left {
                assert_eq!(col_idx, 2, "active should be at index 2");
            }
        }
    }
}

// ── CREATE INDEX validation ───────────────────────────────────────────────────

#[test]
fn test_create_index_validates_columns() {
    let mut f = Fixture::new();
    f.create_table(
        "users",
        &[("id", ColumnType::BigInt), ("email", ColumnType::Text)],
    );

    let err = f.analyze_err("CREATE INDEX idx ON users (eamil)");
    assert!(matches!(err, DbError::ColumnNotFound { .. }), "got: {err}");
}

#[test]
fn test_create_index_validates_table() {
    let f = Fixture::new();
    let err = f.analyze_err("CREATE INDEX idx ON usres (id)");
    assert!(matches!(err, DbError::TableNotFound { .. }), "got: {err}");
}

// ── Transaction stmts pass through ───────────────────────────────────────────

#[test]
fn test_transaction_stmts_pass_through() {
    let f = Fixture::new();
    assert!(matches!(
        f.analyze("BEGIN").unwrap(),
        nexusdb_sql::ast::Stmt::Begin
    ));
    assert!(matches!(
        f.analyze("COMMIT").unwrap(),
        nexusdb_sql::ast::Stmt::Commit
    ));
    assert!(matches!(
        f.analyze("ROLLBACK").unwrap(),
        nexusdb_sql::ast::Stmt::Rollback
    ));
}
