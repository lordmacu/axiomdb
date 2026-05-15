use axiomdb_sql::{parse, Expr, FromClause, GenerateSeriesClause};
use axiomdb_types::Value;

fn parse_gs(sql: &str) -> GenerateSeriesClause {
    let stmt = parse(sql, None).expect("parse failed");
    let select = match stmt {
        axiomdb_sql::ast::Stmt::Select(s) => s,
        other => panic!("expected SELECT, got {:?}", other),
    };
    match select.from.expect("no FROM clause") {
        FromClause::GenerateSeries(gs) => *gs,
        other => panic!("expected GenerateSeries, got {:?}", other),
    }
}

fn int_lit(n: i64) -> Expr {
    Expr::Literal(Value::Int(n as i32))
}

fn str_lit(s: &str) -> Expr {
    Expr::Literal(Value::Text(s.to_string()))
}

// 1. Basic two-argument integer series.
#[test]
fn test_parse_generate_series_two_args() {
    let gs = parse_gs("SELECT * FROM GENERATE_SERIES(1, 10)");
    assert_eq!(gs.start, int_lit(1));
    assert_eq!(gs.stop, int_lit(10));
    assert!(gs.step.is_none());
    assert!(gs.alias.is_none());
    assert!(gs.column_name.is_none());
}

// 2. Three-argument integer series with explicit step.
#[test]
fn test_parse_generate_series_three_args_step() {
    let gs = parse_gs("SELECT * FROM GENERATE_SERIES(1, 10, 2)");
    assert_eq!(gs.start, int_lit(1));
    assert_eq!(gs.stop, int_lit(10));
    assert_eq!(gs.step, Some(int_lit(2)));
}

// 3. Negative step.
#[test]
fn test_parse_generate_series_negative_step() {
    let gs = parse_gs("SELECT * FROM GENERATE_SERIES(10, 1, -1)");
    assert_eq!(gs.start, int_lit(10));
    assert_eq!(gs.stop, int_lit(1));
    // -1 may parse as UnaryMinus(1) or Literal(-1) depending on lexer.
    assert!(gs.step.is_some());
}

// 4. Alias only: AS g.
#[test]
fn test_parse_generate_series_alias_only() {
    let gs = parse_gs("SELECT * FROM GENERATE_SERIES(1, 5) AS g");
    assert_eq!(gs.alias, Some("g".into()));
    assert!(gs.column_name.is_none());
}

// 5. Alias + column name: AS g(n).
#[test]
fn test_parse_generate_series_alias_and_col() {
    let gs = parse_gs("SELECT * FROM GENERATE_SERIES(1, 5) AS g(n)");
    assert_eq!(gs.alias, Some("g".into()));
    assert_eq!(gs.column_name, Some("n".into()));
}

// 6. No alias — alias and column_name are None.
#[test]
fn test_parse_generate_series_no_alias() {
    let gs = parse_gs("SELECT * FROM GENERATE_SERIES(1, 5)");
    assert!(gs.alias.is_none());
    assert!(gs.column_name.is_none());
}

// 7. Date series with string step.
#[test]
fn test_parse_generate_series_date_string_step() {
    let gs = parse_gs(
        "SELECT * FROM GENERATE_SERIES('2024-01-01'::date, '2024-12-31'::date, '1 month')",
    );
    assert!(gs.step.is_some());
    // step should parse as a string literal '1 month'
    assert_eq!(gs.step, Some(str_lit("1 month")));
}

// 8. Case-insensitive keyword: generate_series (lowercase).
#[test]
fn test_parse_generate_series_lowercase() {
    let gs = parse_gs("SELECT * FROM generate_series(1, 100) AS g(n)");
    assert_eq!(gs.start, int_lit(1));
    assert_eq!(gs.stop, int_lit(100));
    assert_eq!(gs.alias, Some("g".into()));
    assert_eq!(gs.column_name, Some("n".into()));
}
