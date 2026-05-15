//! Parser integration tests for COPY FROM / TO (subphase 20.5).

use axiomdb_sql::{
    ast::{CopyFormat, CopyFromStmt, CopyToStmt, Stmt},
    parse,
};

fn parse_copy_from(sql: &str) -> CopyFromStmt {
    match parse(sql, None).unwrap() {
        Stmt::CopyFrom(c) => c,
        other => panic!("expected CopyFrom, got {other:?}"),
    }
}

fn parse_copy_to(sql: &str) -> CopyToStmt {
    match parse(sql, None).unwrap() {
        Stmt::CopyTo(c) => c,
        other => panic!("expected CopyTo, got {other:?}"),
    }
}

#[test]
fn parse_copy_from_csv_with_options() {
    let c = parse_copy_from(
        "COPY t FROM '/tmp/data.csv' WITH (FORMAT CSV, HEADER TRUE, DELIMITER ',')",
    );
    assert_eq!(c.table, "t");
    assert_eq!(c.path, "/tmp/data.csv");
    assert_eq!(c.options.format, Some(CopyFormat::Csv));
    assert_eq!(c.options.header, Some(true));
    assert_eq!(c.options.delimiter, Some(','));
}

#[test]
fn parse_copy_to_no_options() {
    let c = parse_copy_to("COPY orders TO '/tmp/out.jsonl'");
    assert_eq!(c.table, "orders");
    assert_eq!(c.path, "/tmp/out.jsonl");
    assert_eq!(c.options.format, None);
    assert_eq!(c.options.header, None);
}

#[test]
fn parse_copy_from_json_format() {
    let c = parse_copy_from("COPY items FROM '/tmp/in.json' WITH (FORMAT JSON)");
    assert_eq!(c.options.format, Some(CopyFormat::Json));
}

#[test]
fn parse_copy_from_jsonl_format() {
    let c = parse_copy_from("COPY items FROM '/tmp/in.jsonl' WITH (FORMAT JSONL)");
    assert_eq!(c.options.format, Some(CopyFormat::Jsonl));
}

#[test]
fn parse_copy_header_false() {
    let c = parse_copy_from("COPY t FROM '/tmp/rows.csv' WITH (FORMAT CSV, HEADER FALSE)");
    assert_eq!(c.options.header, Some(false));
}

#[test]
fn parse_copy_null_option() {
    let c = parse_copy_from("COPY t FROM '/p.csv' WITH (FORMAT CSV, NULL 'NULL')");
    assert_eq!(c.options.null_str, Some("NULL".into()));
}

#[test]
fn parse_copy_to_csv_with_header() {
    let c = parse_copy_to("COPY products TO '/out.csv' WITH (FORMAT CSV, HEADER TRUE)");
    assert_eq!(c.options.format, Some(CopyFormat::Csv));
    assert_eq!(c.options.header, Some(true));
}

#[test]
fn parse_copy_from_no_with_clause() {
    let c = parse_copy_from("COPY employees FROM '/data/emp.csv'");
    assert_eq!(c.table, "employees");
    assert_eq!(c.path, "/data/emp.csv");
    assert_eq!(c.options.format, None);
    assert_eq!(c.options.header, None);
    assert_eq!(c.options.delimiter, None);
}

#[test]
fn parse_copy_header_bare_true() {
    // HEADER without value = TRUE (PostgreSQL compat)
    let c = parse_copy_from("COPY t FROM '/f.csv' WITH (FORMAT CSV, HEADER)");
    assert_eq!(c.options.header, Some(true));
}

#[test]
fn parse_copy_unknown_format_errors() {
    // PARQUET is now a valid format; use a genuinely unknown format instead
    let err = parse("COPY t FROM '/f.csv' WITH (FORMAT FOOBAR)", None);
    assert!(err.is_err(), "expected error for unknown format FOOBAR");
    let msg = err.unwrap_err().to_string();
    assert!(msg.contains("FOOBAR"), "error should mention FOOBAR: {msg}");
}
