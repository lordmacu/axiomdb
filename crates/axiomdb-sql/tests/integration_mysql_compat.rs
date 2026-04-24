//! Integration tests for MySQL compatibility gap items (4.G1–4.G4).
//!
//! Covers:
//!   G1: float div→NULL, ROUND half-up, CONCAT NULL, SUBSTR negative index
//!   G2: IS TRUE/FALSE predicates, LIKE ESCAPE
//!   G3: LOCK/UNLOCK/FLUSH/KILL→Noop, LIMIT comma syntax, CREATE TABLE options,
//!       inline INDEX/KEY, column attrs (UNSIGNED/ZEROFILL/COLLATE/etc.), BEGIN WORK
//!   G4: REGEXP/RLIKE parser, XOR parser, <=> parser, hex literals, SET SESSION/GLOBAL

use axiomdb_sql::{
    ast::{SelectItem, SetValue, Stmt},
    expr::{BinaryOp, Expr},
    parse, parse_with_sql_mode, SqlModeFlags,
};
use axiomdb_types::Value;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn parse_ok(sql: &str) -> Stmt {
    parse(sql, None).unwrap_or_else(|e| panic!("parse failed for {:?}: {}", sql, e))
}

fn select_first_expr(sql: &str) -> Expr {
    match parse_ok(sql) {
        Stmt::Select(s) => match &s.columns[0] {
            SelectItem::Expr { expr, .. } => expr.clone(),
            other => panic!("expected Expr item, got {other:?}"),
        },
        other => panic!("expected SELECT, got {other:?}"),
    }
}

fn select_first_expr_with_ansi_quotes(sql: &str) -> Expr {
    match parse_with_sql_mode(sql, None, SqlModeFlags { ansi_quotes: true })
        .unwrap_or_else(|e| panic!("parse failed for {:?}: {}", sql, e))
    {
        Stmt::Select(s) => match &s.columns[0] {
            SelectItem::Expr { expr, .. } => expr.clone(),
            other => panic!("expected Expr item, got {other:?}"),
        },
        other => panic!("expected SELECT, got {other:?}"),
    }
}

// ── G1: Float division by zero → NULL (4.17e) ────────────────────────────────
// Tested in integration_eval.rs (BinaryOp::Div with Real operands).
// Parser-level: 1.0 / 0 is a valid expression tree.

#[test]
fn test_float_div_zero_parses() {
    let e = select_first_expr("SELECT 1.0 / 0");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::Div,
            ..
        }
    ));
}

// ── G1: ROUND half-up (4.19k) ────────────────────────────────────────────────
// Semantic tests live in eval/functions/numeric.rs unit tests.
// Here we verify the function call parses correctly.

#[test]
fn test_round_parses() {
    let e = select_first_expr("SELECT ROUND(2.5, 0)");
    assert!(matches!(e, Expr::Function { ref name, .. } if name.eq_ignore_ascii_case("round")));
}

// ── G2: IS TRUE / IS FALSE / IS NOT TRUE / IS NOT FALSE (4.17d) ──────────────

#[test]
fn test_is_true_parses() {
    let e = select_first_expr("SELECT col IS TRUE FROM t");
    assert!(matches!(
        e,
        Expr::IsBoolean {
            value: true,
            negated: false,
            ..
        }
    ));
}

#[test]
fn test_is_false_parses() {
    let e = select_first_expr("SELECT col IS FALSE FROM t");
    assert!(matches!(
        e,
        Expr::IsBoolean {
            value: false,
            negated: false,
            ..
        }
    ));
}

#[test]
fn test_is_not_true_parses() {
    let e = select_first_expr("SELECT col IS NOT TRUE FROM t");
    assert!(matches!(
        e,
        Expr::IsBoolean {
            value: true,
            negated: true,
            ..
        }
    ));
}

#[test]
fn test_is_not_false_parses() {
    let e = select_first_expr("SELECT col IS NOT FALSE FROM t");
    assert!(matches!(
        e,
        Expr::IsBoolean {
            value: false,
            negated: true,
            ..
        }
    ));
}

// ── G2: LIKE ESCAPE (4.4d) ───────────────────────────────────────────────────

#[test]
fn test_like_escape_parses() {
    let e = select_first_expr("SELECT col LIKE '%/%' ESCAPE '/' FROM t");
    match e {
        Expr::Like {
            escape,
            negated: false,
            ..
        } => {
            assert!(escape.is_some(), "escape clause should be Some");
        }
        other => panic!("expected Like, got {other:?}"),
    }
}

#[test]
fn test_like_without_escape_parses() {
    let e = select_first_expr("SELECT col LIKE '%hello%' FROM t");
    match e {
        Expr::Like { escape, .. } => assert!(escape.is_none()),
        other => panic!("expected Like, got {other:?}"),
    }
}

// ── G3: LOCK/UNLOCK/FLUSH/KILL → Noop (4.3b) ─────────────────────────────────

#[test]
fn test_lock_tables_parses_as_noop() {
    assert!(matches!(parse_ok("LOCK TABLES t READ"), Stmt::Noop));
}

#[test]
fn test_unlock_tables_parses_as_noop() {
    assert!(matches!(parse_ok("UNLOCK TABLES"), Stmt::Noop));
}

#[test]
fn test_flush_tables_parses_as_noop() {
    assert!(matches!(parse_ok("FLUSH TABLES"), Stmt::Noop));
}

#[test]
fn test_kill_query_parses_as_noop() {
    assert!(matches!(parse_ok("KILL QUERY 42"), Stmt::Noop));
}

#[test]
fn test_kill_connection_parses_as_noop() {
    assert!(matches!(parse_ok("KILL CONNECTION 42"), Stmt::Noop));
}

// ── G3: LIMIT comma syntax — LIMIT offset, count (4.2c) ──────────────────────

#[test]
fn test_limit_comma_syntax() {
    match parse_ok("SELECT id FROM t LIMIT 5, 10") {
        Stmt::Select(s) => {
            // LIMIT 5, 10 → offset=5, limit=10
            assert!(s.limit.is_some(), "limit should be Some");
            assert!(s.offset.is_some(), "offset should be Some");
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

#[test]
fn test_limit_comma_zero_offset() {
    match parse_ok("SELECT id FROM t LIMIT 0, 20") {
        Stmt::Select(s) => {
            assert!(s.limit.is_some());
            assert!(s.offset.is_some());
        }
        other => panic!("expected SELECT, got {other:?}"),
    }
}

// ── G4: ANSI_QUOTES OFF / ON behavior (4.2f) ────────────────────────────────

#[test]
fn test_double_quoted_string_parses_in_default_mode() {
    let e = select_first_expr(r#"SELECT "hello""#);
    assert_eq!(e, Expr::Literal(Value::Text("hello".into())));
}

#[test]
fn test_double_quoted_identifier_parses_when_ansi_quotes_enabled() {
    let e = select_first_expr_with_ansi_quotes(r#"SELECT "name" FROM "users""#);
    match e {
        Expr::Column { name, .. } => assert_eq!(name, "name"),
        other => panic!("expected quoted identifier column, got {other:?}"),
    }
}

// ── G3: CREATE TABLE with table options ENGINE=, CHARSET=, etc. (4.1b) ───────

#[test]
fn test_create_table_engine_option_parses() {
    // Should parse without error, ignoring table options
    match parse_ok("CREATE TABLE t (id INT) ENGINE=InnoDB") {
        Stmt::CreateTable(ct) => assert_eq!(ct.table.name, "t"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_create_table_multiple_options_parse() {
    match parse_ok(
        "CREATE TABLE t (id INT) ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_general_ci",
    ) {
        Stmt::CreateTable(ct) => assert_eq!(ct.table.name, "t"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_create_table_row_format_option() {
    match parse_ok("CREATE TABLE t (id INT) ROW_FORMAT=DYNAMIC") {
        Stmt::CreateTable(ct) => assert_eq!(ct.table.name, "t"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_create_table_auto_increment_option() {
    match parse_ok("CREATE TABLE t (id INT) AUTO_INCREMENT=100") {
        Stmt::CreateTable(ct) => assert_eq!(ct.table.name, "t"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

// ── G3: CREATE TABLE inline INDEX/KEY (4.1d) ─────────────────────────────────

#[test]
fn test_create_table_inline_index_parses() {
    use axiomdb_sql::ast::TableConstraint;
    match parse_ok("CREATE TABLE t (id INT, name TEXT, INDEX idx_name (name))") {
        Stmt::CreateTable(ct) => {
            let has_index = ct
                .table_constraints
                .iter()
                .any(|c| matches!(c, TableConstraint::Index { .. }));
            assert!(
                has_index,
                "inline INDEX should produce TableConstraint::Index"
            );
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_create_table_inline_key_parses() {
    use axiomdb_sql::ast::TableConstraint;
    match parse_ok("CREATE TABLE t (id INT, email TEXT, KEY idx_email (email))") {
        Stmt::CreateTable(ct) => {
            let has_index = ct
                .table_constraints
                .iter()
                .any(|c| matches!(c, TableConstraint::Index { .. }));
            assert!(has_index, "KEY should produce TableConstraint::Index");
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

// ── G3: Column attrs (UNSIGNED, ZEROFILL, COLLATE, CHARACTER SET, etc.) (4.1c)

#[test]
fn test_column_unsigned_parses() {
    match parse_ok("CREATE TABLE t (n INT UNSIGNED)") {
        Stmt::CreateTable(ct) => assert_eq!(ct.columns[0].name, "n"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_column_collate_parses() {
    match parse_ok("CREATE TABLE t (s VARCHAR(100) COLLATE utf8mb4_unicode_ci)") {
        Stmt::CreateTable(ct) => {
            assert_eq!(ct.columns[0].name, "s");
            assert_eq!(ct.columns[0].collation.as_deref(), Some("es"));
        }
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_create_database_and_table_collate_parse_to_canonical_names() {
    match parse_ok("CREATE DATABASE appdb DEFAULT COLLATE utf8mb4_bin") {
        Stmt::CreateDatabase(db) => assert_eq!(db.collation.as_deref(), Some("binary")),
        other => panic!("expected CreateDatabase, got {other:?}"),
    }

    match parse_ok("CREATE TABLE t (s TEXT) COLLATE utf8mb4_unicode_ci") {
        Stmt::CreateTable(ct) => assert_eq!(ct.collation.as_deref(), Some("es")),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_expr_collate_parses() {
    match parse_ok("SELECT * FROM t WHERE name COLLATE utf8mb4_bin LIKE 'jose'") {
        Stmt::Select(sel) => assert!(sel.where_clause.is_some()),
        other => panic!("expected Select, got {other:?}"),
    }
}

#[test]
fn test_column_character_set_parses() {
    match parse_ok("CREATE TABLE t (s TEXT CHARACTER SET utf8mb4)") {
        Stmt::CreateTable(ct) => assert_eq!(ct.columns[0].name, "s"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_column_comment_parses() {
    match parse_ok("CREATE TABLE t (id INT COMMENT 'primary key')") {
        Stmt::CreateTable(ct) => assert_eq!(ct.columns[0].name, "id"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

#[test]
fn test_column_on_update_current_timestamp() {
    match parse_ok(
        "CREATE TABLE t (updated_at DATETIME DEFAULT CURRENT_TIMESTAMP ON UPDATE CURRENT_TIMESTAMP)",
    ) {
        Stmt::CreateTable(ct) => assert_eq!(ct.columns[0].name, "updated_at"),
        other => panic!("expected CreateTable, got {other:?}"),
    }
}

// ── G3: BEGIN WORK + START TRANSACTION modifiers (4.5d) ──────────────────────

#[test]
fn test_begin_work_parses() {
    assert!(matches!(parse_ok("BEGIN WORK"), Stmt::Begin));
}

#[test]
fn test_start_transaction_read_only_parses() {
    assert!(matches!(
        parse_ok("START TRANSACTION READ ONLY"),
        Stmt::Begin
    ));
}

#[test]
fn test_start_transaction_read_write_parses() {
    assert!(matches!(
        parse_ok("START TRANSACTION READ WRITE"),
        Stmt::Begin
    ));
}

// ── G4: REGEXP / RLIKE (4.4b) ────────────────────────────────────────────────

#[test]
fn test_regexp_operator_parses() {
    let e = select_first_expr("SELECT email REGEXP '^[a-z]+@' FROM t");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::Regexp,
            ..
        }
    ));
}

#[test]
fn test_rlike_operator_parses() {
    let e = select_first_expr("SELECT email RLIKE '^[a-z]+@' FROM t");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::Regexp,
            ..
        }
    ));
}

#[test]
fn test_not_regexp_parses() {
    // NOT REGEXP wraps in UnaryOp::Not
    let e = select_first_expr("SELECT email NOT REGEXP '^[0-9]' FROM t");
    assert!(matches!(e, Expr::UnaryOp { .. }));
}

// ── G4: XOR operator (4.4b) ──────────────────────────────────────────────────

#[test]
fn test_xor_operator_parses() {
    let e = select_first_expr("SELECT a XOR b FROM t");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::Xor,
            ..
        }
    ));
}

// ── G4: <=> null-safe equality (4.4b) ────────────────────────────────────────

#[test]
fn test_null_safe_eq_parses() {
    let e = select_first_expr("SELECT a <=> b FROM t");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::NullSafe,
            ..
        }
    ));
}

// ── G4: DIV integer division (4.4b) ──────────────────────────────────────────

#[test]
fn test_int_div_operator_parses() {
    let e = select_first_expr("SELECT 7 DIV 2");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::IntDiv,
            ..
        }
    ));
}

// ── G4: Bitwise operators (4.4b) ─────────────────────────────────────────────

#[test]
fn test_bitwise_and_parses() {
    let e = select_first_expr("SELECT flags & 0x01 FROM t");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::BitAnd,
            ..
        }
    ));
}

#[test]
fn test_bitwise_or_parses() {
    let e = select_first_expr("SELECT a | b FROM t");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::BitOr,
            ..
        }
    ));
}

#[test]
fn test_shift_left_parses() {
    let e = select_first_expr("SELECT 1 << 4");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::ShiftLeft,
            ..
        }
    ));
}

#[test]
fn test_shift_right_parses() {
    let e = select_first_expr("SELECT 16 >> 2");
    assert!(matches!(
        e,
        Expr::BinaryOp {
            op: BinaryOp::ShiftRight,
            ..
        }
    ));
}

// ── G4: Hex literals (4.4b / 4.2e) ──────────────────────────────────────────

#[test]
fn test_hex_literal_0xff() {
    let e = select_first_expr("SELECT 0xFF");
    assert_eq!(e, Expr::Literal(Value::BigInt(255)));
}

#[test]
fn test_hex_literal_0x1a2b() {
    let e = select_first_expr("SELECT 0x1A2B");
    assert_eq!(e, Expr::Literal(Value::BigInt(0x1A2B)));
}

#[test]
fn test_hex_literal_zero() {
    let e = select_first_expr("SELECT 0x0");
    assert_eq!(e, Expr::Literal(Value::BigInt(0)));
}

// ── G4: SET SESSION/GLOBAL/LOCAL (4.4c) ──────────────────────────────────────

#[test]
fn test_set_session_prefix_parses() {
    // Should parse without error
    parse_ok("SET SESSION autocommit = 1");
}

#[test]
fn test_set_global_prefix_parses() {
    parse_ok("SET GLOBAL max_connections = 100");
}

#[test]
fn test_set_local_prefix_parses() {
    parse_ok("SET LOCAL sql_mode = 'STRICT_ALL_TABLES'");
}

#[test]
fn test_set_at_at_variable_parses() {
    // @@varname (without qualifier.prefix) strips the @@ prefix
    parse_ok("SET @@autocommit = 0");
}

#[test]
fn test_set_session_qualified_variable_parses() {
    match parse_ok("SET @@session.time_zone = 'UTC'") {
        Stmt::Set(stmt) => {
            assert_eq!(stmt.variable, "time_zone");
            assert!(
                matches!(stmt.value, SetValue::Expr(Expr::Literal(Value::Text(ref s))) if s == "UTC")
            );
        }
        other => panic!("expected SET, got {other:?}"),
    }
}

#[test]
fn test_set_multiple_vars_parses() {
    parse_ok("SET autocommit = 1, sql_mode = 'NO_ENGINE_SUBSTITUTION'");
}
