//! Integration tests for the DDL parser (subfase 4.3 + 4.3a–4.3d).

use axiomdb_core::DbError;
use axiomdb_sql::expr::{BinaryOp, Expr, UnaryOp};
use axiomdb_sql::{
    ast::{ColumnConstraint, ForeignKeyAction, SortOrder, Stmt, TableConstraint},
    parse,
};
use axiomdb_types::{DataType, Value};

// ── Helpers ───────────────────────────────────────────────────────────────────

fn create_table(sql: &str) -> axiomdb_sql::ast::CreateTableStmt {
    match parse(sql, None).unwrap() {
        Stmt::CreateTable(ct) => ct,
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

fn parse_err(sql: &str) -> DbError {
    parse(sql, None).unwrap_err()
}

// ── Basic CREATE TABLE ────────────────────────────────────────────────────────

#[test]
fn test_create_table_simple() {
    let ct = create_table("CREATE TABLE users (id BIGINT, name TEXT)");
    assert_eq!(ct.table.name, "users");
    assert_eq!(ct.columns.len(), 2);
    assert_eq!(ct.columns[0].name, "id");
    assert_eq!(ct.columns[0].data_type, DataType::BigInt);
    assert_eq!(ct.columns[1].name, "name");
    assert_eq!(ct.columns[1].data_type, DataType::Text);
    assert!(ct.table_constraints.is_empty());
}

#[test]
fn test_create_table_if_not_exists() {
    let ct = create_table("CREATE TABLE IF NOT EXISTS t (id INT)");
    assert!(ct.if_not_exists);
    assert_eq!(ct.table.name, "t");
}

#[test]
fn test_create_table_without_if_not_exists() {
    let ct = create_table("CREATE TABLE t (id INT)");
    assert!(!ct.if_not_exists);
}

// ── All data types ────────────────────────────────────────────────────────────

#[test]
fn test_data_types_int() {
    let ct = create_table("CREATE TABLE t (a INT, b INTEGER)");
    assert_eq!(ct.columns[0].data_type, DataType::Int);
    assert_eq!(ct.columns[1].data_type, DataType::Int);
}

#[test]
fn test_data_types_bigint() {
    let ct = create_table("CREATE TABLE t (a BIGINT)");
    assert_eq!(ct.columns[0].data_type, DataType::BigInt);
}

#[test]
fn test_data_types_real() {
    let ct = create_table("CREATE TABLE t (a REAL, b DOUBLE, c FLOAT)");
    assert_eq!(ct.columns[0].data_type, DataType::Real);
    assert_eq!(ct.columns[1].data_type, DataType::Real);
    assert_eq!(ct.columns[2].data_type, DataType::Real);
}

#[test]
fn test_data_types_decimal() {
    let ct = create_table("CREATE TABLE t (a DECIMAL, b NUMERIC)");
    assert_eq!(ct.columns[0].data_type, DataType::Decimal);
    assert_eq!(ct.columns[1].data_type, DataType::Decimal);
}

#[test]
fn test_data_type_decimal_with_precision_scale() {
    // DECIMAL(10,2) — params parsed and discarded
    let ct = create_table("CREATE TABLE t (price DECIMAL(10, 2))");
    assert_eq!(ct.columns[0].data_type, DataType::Decimal);
}

#[test]
fn test_data_types_bool() {
    let ct = create_table("CREATE TABLE t (a BOOL, b BOOLEAN)");
    assert_eq!(ct.columns[0].data_type, DataType::Bool);
    assert_eq!(ct.columns[1].data_type, DataType::Bool);
}

#[test]
fn test_data_types_text() {
    let ct = create_table("CREATE TABLE t (a TEXT, b VARCHAR(255), c CHAR(10))");
    assert_eq!(ct.columns[0].data_type, DataType::Text);
    assert_eq!(ct.columns[1].data_type, DataType::Text); // VARCHAR(n) → Text
    assert_eq!(ct.columns[2].data_type, DataType::Text); // CHAR(n) → Text
}

#[test]
fn test_data_types_bytes() {
    let ct = create_table("CREATE TABLE t (a BLOB, b BYTEA)");
    assert_eq!(ct.columns[0].data_type, DataType::Bytes);
    assert_eq!(ct.columns[1].data_type, DataType::Bytes);
}

#[test]
fn test_data_types_date_timestamp() {
    let ct = create_table("CREATE TABLE t (a DATE, b TIMESTAMP, c DATETIME)");
    assert_eq!(ct.columns[0].data_type, DataType::Date);
    assert_eq!(ct.columns[1].data_type, DataType::Timestamp);
    assert_eq!(ct.columns[2].data_type, DataType::Timestamp);
}

#[test]
fn test_data_type_uuid() {
    let ct = create_table("CREATE TABLE t (a UUID)");
    assert_eq!(ct.columns[0].data_type, DataType::Uuid);
}

// ── Column constraints (4.3a) ─────────────────────────────────────────────────

#[test]
fn test_not_null() {
    let ct = create_table("CREATE TABLE t (id INT NOT NULL)");
    assert!(ct.columns[0]
        .constraints
        .contains(&ColumnConstraint::NotNull));
}

#[test]
fn test_null_explicit() {
    let ct = create_table("CREATE TABLE t (note TEXT NULL)");
    assert!(ct.columns[0].constraints.contains(&ColumnConstraint::Null));
}

#[test]
fn test_primary_key_column() {
    let ct = create_table("CREATE TABLE t (id BIGINT PRIMARY KEY)");
    assert!(ct.columns[0]
        .constraints
        .contains(&ColumnConstraint::PrimaryKey));
}

#[test]
fn test_unique_column() {
    let ct = create_table("CREATE TABLE t (email TEXT UNIQUE)");
    assert!(ct.columns[0]
        .constraints
        .contains(&ColumnConstraint::Unique));
}

#[test]
fn test_default_int() {
    let ct = create_table("CREATE TABLE t (score INT DEFAULT 0)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Default(Expr::Literal(Value::Int(0)))
    ));
}

#[test]
fn test_default_negative_int() {
    let ct = create_table("CREATE TABLE t (balance INT DEFAULT -1)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Default(Expr::UnaryOp {
            op: UnaryOp::Neg,
            ..
        })
    ));
}

#[test]
fn test_default_string() {
    let ct = create_table("CREATE TABLE t (status TEXT DEFAULT 'active')");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Default(Expr::Literal(Value::Text(s))) if s == "active"
    ));
}

#[test]
fn test_default_true() {
    let ct = create_table("CREATE TABLE t (active BOOL DEFAULT TRUE)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Default(Expr::Literal(Value::Bool(true)))
    ));
}

#[test]
fn test_default_false() {
    let ct = create_table("CREATE TABLE t (active BOOL DEFAULT FALSE)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Default(Expr::Literal(Value::Bool(false)))
    ));
}

#[test]
fn test_default_null() {
    let ct = create_table("CREATE TABLE t (note TEXT DEFAULT NULL)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Default(Expr::Literal(Value::Null))
    ));
}

// ── AUTO_INCREMENT / SERIAL (4.3c) ────────────────────────────────────────────

#[test]
fn test_auto_increment() {
    let ct = create_table("CREATE TABLE t (id BIGINT AUTO_INCREMENT)");
    assert!(ct.columns[0]
        .constraints
        .contains(&ColumnConstraint::AutoIncrement));
}

#[test]
fn test_serial_synonym_for_auto_increment() {
    let ct = create_table("CREATE TABLE t (id INT SERIAL)");
    assert!(ct.columns[0]
        .constraints
        .contains(&ColumnConstraint::AutoIncrement));
}

// ── REFERENCES (4.3a) ─────────────────────────────────────────────────────────

#[test]
fn test_references_basic() {
    let ct = create_table("CREATE TABLE orders (user_id BIGINT REFERENCES users)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::References { table, column: None, .. } if table == "users"
    ));
}

#[test]
fn test_references_with_column() {
    let ct = create_table("CREATE TABLE orders (user_id BIGINT REFERENCES users(id))");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::References { table, column: Some(col), .. }
            if table == "users" && col == "id"
    ));
}

#[test]
fn test_references_on_delete_cascade() {
    let ct =
        create_table("CREATE TABLE orders (user_id BIGINT REFERENCES users ON DELETE CASCADE)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::References {
            on_delete: ForeignKeyAction::Cascade,
            ..
        }
    ));
}

#[test]
fn test_references_on_delete_set_null() {
    let ct = create_table("CREATE TABLE t (uid INT REFERENCES users ON DELETE SET NULL)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::References {
            on_delete: ForeignKeyAction::SetNull,
            ..
        }
    ));
}

#[test]
fn test_references_on_update_restrict() {
    let ct = create_table("CREATE TABLE t (uid INT REFERENCES users ON UPDATE RESTRICT)");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::References {
            on_update: ForeignKeyAction::Restrict,
            ..
        }
    ));
}

#[test]
fn test_references_on_delete_and_update() {
    let ct = create_table(
        "CREATE TABLE t (uid INT REFERENCES users ON DELETE CASCADE ON UPDATE RESTRICT)",
    );
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::References {
            on_delete: ForeignKeyAction::Cascade,
            on_update: ForeignKeyAction::Restrict,
            ..
        }
    ));
}

// ── CHECK constraint (4.3b) ───────────────────────────────────────────────────

#[test]
fn test_check_column_simple() {
    let ct = create_table("CREATE TABLE t (price REAL CHECK (price > 0))");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Check(Expr::BinaryOp {
            op: BinaryOp::Gt,
            ..
        })
    ));
}

#[test]
fn test_check_column_and_expression() {
    let ct = create_table("CREATE TABLE t (age INT CHECK (age >= 0 AND age <= 150))");
    assert!(matches!(
        &ct.columns[0].constraints[0],
        ColumnConstraint::Check(Expr::BinaryOp {
            op: BinaryOp::And,
            ..
        })
    ));
}

// ── Table-level constraints ───────────────────────────────────────────────────

#[test]
fn test_table_primary_key() {
    let ct = create_table("CREATE TABLE t (a INT, b INT, PRIMARY KEY (a, b))");
    assert_eq!(ct.table_constraints.len(), 1);
    assert!(matches!(
        &ct.table_constraints[0],
        TableConstraint::PrimaryKey { name: None, columns } if columns == &["a", "b"]
    ));
}

#[test]
fn test_table_unique() {
    let ct = create_table("CREATE TABLE t (email TEXT, UNIQUE (email))");
    assert!(matches!(
        &ct.table_constraints[0],
        TableConstraint::Unique { columns, .. } if columns == &["email"]
    ));
}

#[test]
fn test_table_foreign_key() {
    let ct = create_table(
        "CREATE TABLE orders (user_id BIGINT, FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE)",
    );
    assert!(matches!(
        &ct.table_constraints[0],
        TableConstraint::ForeignKey {
            ref_table,
            on_delete: ForeignKeyAction::Cascade,
            ..
        } if ref_table == "users"
    ));
}

#[test]
fn test_named_constraint() {
    let ct = create_table("CREATE TABLE t (id INT, CONSTRAINT pk PRIMARY KEY (id))");
    assert!(matches!(
        &ct.table_constraints[0],
        TableConstraint::PrimaryKey { name: Some(n), .. } if n == "pk"
    ));
}

#[test]
fn test_table_check_constraint() {
    let ct = create_table("CREATE TABLE t (price REAL, CHECK (price > 0))");
    assert!(matches!(
        &ct.table_constraints[0],
        TableConstraint::Check { .. }
    ));
}

// ── Multiple constraints on one column ────────────────────────────────────────

#[test]
fn test_multiple_column_constraints() {
    let ct = create_table("CREATE TABLE t (id BIGINT PRIMARY KEY AUTO_INCREMENT NOT NULL)");
    let c = &ct.columns[0].constraints;
    assert!(c.contains(&ColumnConstraint::PrimaryKey));
    assert!(c.contains(&ColumnConstraint::AutoIncrement));
    assert!(c.contains(&ColumnConstraint::NotNull));
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

#[test]
fn test_create_index_basic() {
    match parse("CREATE INDEX idx_name ON users (name)", None).unwrap() {
        Stmt::CreateIndex(ci) => {
            assert!(!ci.unique);
            assert_eq!(ci.name, "idx_name");
            assert_eq!(ci.table.name, "users");
            assert_eq!(ci.columns.len(), 1);
            assert_eq!(ci.columns[0].name, "name");
        }
        other => panic!("expected CreateIndex, got {other:?}"),
    }
}

#[test]
fn test_create_unique_index() {
    match parse("CREATE UNIQUE INDEX idx_email ON users (email)", None).unwrap() {
        Stmt::CreateIndex(ci) => assert!(ci.unique),
        other => panic!("expected CreateIndex, got {other:?}"),
    }
}

#[test]
fn test_create_index_if_not_exists() {
    match parse("CREATE INDEX IF NOT EXISTS idx ON t (col)", None).unwrap() {
        Stmt::CreateIndex(ci) => assert!(ci.if_not_exists),
        other => panic!("expected CreateIndex, got {other:?}"),
    }
}

#[test]
fn test_create_index_asc_desc() {
    match parse("CREATE INDEX idx ON t (a ASC, b DESC)", None).unwrap() {
        Stmt::CreateIndex(ci) => {
            assert_eq!(ci.columns[0].order, SortOrder::Asc);
            assert_eq!(ci.columns[1].order, SortOrder::Desc);
        }
        other => panic!("expected CreateIndex, got {other:?}"),
    }
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

#[test]
fn test_drop_table_basic() {
    match parse("DROP TABLE users", None).unwrap() {
        Stmt::DropTable(dt) => {
            assert_eq!(dt.tables.len(), 1);
            assert_eq!(dt.tables[0].name, "users");
            assert!(!dt.if_exists);
            assert!(!dt.cascade);
        }
        other => panic!("expected DropTable, got {other:?}"),
    }
}

#[test]
fn test_drop_table_if_exists() {
    match parse("DROP TABLE IF EXISTS users", None).unwrap() {
        Stmt::DropTable(dt) => assert!(dt.if_exists),
        other => panic!("expected DropTable, got {other:?}"),
    }
}

#[test]
fn test_drop_table_multiple() {
    match parse("DROP TABLE a, b, c", None).unwrap() {
        Stmt::DropTable(dt) => assert_eq!(dt.tables.len(), 3),
        other => panic!("expected DropTable, got {other:?}"),
    }
}

#[test]
fn test_drop_table_cascade() {
    match parse("DROP TABLE users CASCADE", None).unwrap() {
        Stmt::DropTable(dt) => assert!(dt.cascade),
        other => panic!("expected DropTable, got {other:?}"),
    }
}

// ── DROP INDEX ────────────────────────────────────────────────────────────────

#[test]
fn test_drop_index_basic() {
    match parse("DROP INDEX idx_email", None).unwrap() {
        Stmt::DropIndex(di) => {
            assert_eq!(di.name, "idx_email");
            assert!(!di.if_exists);
            assert!(di.table.is_none());
        }
        other => panic!("expected DropIndex, got {other:?}"),
    }
}

#[test]
fn test_drop_index_if_exists() {
    match parse("DROP INDEX IF EXISTS idx", None).unwrap() {
        Stmt::DropIndex(di) => assert!(di.if_exists),
        other => panic!("expected DropIndex, got {other:?}"),
    }
}

#[test]
fn test_drop_index_on_table() {
    match parse("DROP INDEX idx_email ON users", None).unwrap() {
        Stmt::DropIndex(di) => {
            assert_eq!(di.name, "idx_email");
            assert_eq!(di.table.as_ref().unwrap().name, "users");
        }
        other => panic!("expected DropIndex, got {other:?}"),
    }
}

// ── Transaction stmts ─────────────────────────────────────────────────────────

#[test]
fn test_begin() {
    assert!(matches!(parse("BEGIN", None).unwrap(), Stmt::Begin));
    assert!(matches!(
        parse("BEGIN TRANSACTION", None).unwrap(),
        Stmt::Begin
    ));
    assert!(matches!(
        parse("START TRANSACTION", None).unwrap(),
        Stmt::Begin
    ));
}

#[test]
fn test_commit() {
    assert!(matches!(parse("COMMIT", None).unwrap(), Stmt::Commit));
}

#[test]
fn test_rollback() {
    assert!(matches!(parse("ROLLBACK", None).unwrap(), Stmt::Rollback));
}

// ── 4.3d — Identifier length ──────────────────────────────────────────────────

#[test]
fn test_identifier_exactly_64_chars_ok() {
    let name = "a".repeat(64);
    let sql = format!("CREATE TABLE {name} (id INT)");
    assert!(parse(&sql, None).is_ok());
}

#[test]
fn test_identifier_65_chars_error() {
    let name = "a".repeat(65);
    let sql = format!("CREATE TABLE {name} (id INT)");
    let e = parse_err(&sql);
    assert!(matches!(e, DbError::ParseError { .. }));
    if let DbError::ParseError { message, .. } = e {
        assert!(
            message.contains("exceeds maximum length"),
            "error should mention length: {message}"
        );
    }
}

#[test]
fn test_column_name_too_long_error() {
    let col = "c".repeat(65);
    let sql = format!("CREATE TABLE t ({col} INT)");
    let e = parse_err(&sql);
    assert!(matches!(e, DbError::ParseError { .. }));
}

// ── Error cases ───────────────────────────────────────────────────────────────

#[test]
fn test_error_empty_input() {
    let e = parse_err("");
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_error_missing_table_name() {
    let e = parse_err("CREATE TABLE");
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_error_missing_paren() {
    let e = parse_err("CREATE TABLE t id INT");
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_error_unknown_data_type() {
    let e = parse_err("CREATE TABLE t (id UNKNOWNTYPE)");
    assert!(matches!(e, DbError::ParseError { .. }));
}

#[test]
fn test_error_trailing_garbage() {
    let e = parse_err("CREATE TABLE t (id INT) garbage");
    assert!(matches!(e, DbError::ParseError { .. }));
}

// ── Full realistic DDL ────────────────────────────────────────────────────────

#[test]
fn test_full_users_table() {
    let sql = "
        CREATE TABLE IF NOT EXISTS users (
            id       BIGINT PRIMARY KEY AUTO_INCREMENT,
            email    VARCHAR(255) NOT NULL UNIQUE,
            name     TEXT NOT NULL,
            score    INT DEFAULT 0,
            active   BOOL DEFAULT TRUE,
            created  TIMESTAMP
        )
    ";
    let ct = create_table(sql);
    assert!(ct.if_not_exists);
    assert_eq!(ct.columns.len(), 6);
    assert_eq!(ct.columns[0].name, "id");
    assert_eq!(ct.columns[0].data_type, DataType::BigInt);
    assert_eq!(ct.columns[1].name, "email");
    assert_eq!(ct.columns[4].name, "active");
}

#[test]
fn test_full_orders_table_with_fk() {
    let sql = "
        CREATE TABLE orders (
            id       BIGINT NOT NULL,
            user_id  BIGINT NOT NULL,
            total    REAL DEFAULT 0.0,
            PRIMARY KEY (id),
            FOREIGN KEY (user_id) REFERENCES users (id) ON DELETE CASCADE ON UPDATE RESTRICT
        )
    ";
    let ct = create_table(sql);
    assert_eq!(ct.columns.len(), 3);
    assert_eq!(ct.table_constraints.len(), 2);
    assert!(matches!(
        &ct.table_constraints[0],
        TableConstraint::PrimaryKey { .. }
    ));
    assert!(matches!(
        &ct.table_constraints[1],
        TableConstraint::ForeignKey {
            on_delete: ForeignKeyAction::Cascade,
            on_update: ForeignKeyAction::Restrict,
            ..
        }
    ));
}
