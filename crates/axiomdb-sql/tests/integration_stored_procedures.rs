//! Stored procedures (Phase 16.7) — parser tests (Step 6).
//!
//! Execution/end-to-end tests are added in later steps. Here we only assert that
//! `CREATE/DROP PROCEDURE` parse correctly in both dialects and capture the body.

use axiomdb_catalog::{ProcLanguage, ProcParamMode};
use axiomdb_sql::ast::Stmt;
use axiomdb_sql::parse;
use axiomdb_types::DataType;

fn parse_stmt(sql: &str) -> Stmt {
    parse(sql, None).unwrap_or_else(|e| panic!("parse failed for {sql}\n  error: {e:?}"))
}

#[test]
fn parse_pg_procedure_with_params() {
    let stmt = parse_stmt(
        "CREATE PROCEDURE p(IN a INT, OUT b TEXT) LANGUAGE plpgsql AS $$ BEGIN b := 'x'; END $$",
    );
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure, got {stmt:?}");
    };
    assert!(!s.or_replace);
    assert_eq!(s.name.name, "p");
    assert_eq!(s.language, ProcLanguage::PlPgSql);
    assert_eq!(s.params.len(), 2);
    assert_eq!(s.params[0].mode, ProcParamMode::In);
    assert_eq!(s.params[0].name, "a");
    assert_eq!(s.params[0].ty, DataType::Int);
    assert_eq!(s.params[1].mode, ProcParamMode::Out);
    assert_eq!(s.params[1].name, "b");
    assert_eq!(s.params[1].ty, DataType::Text);
    assert!(s.body_sql.contains("BEGIN"));
    assert!(s.body_sql.contains("b := 'x'"));
}

#[test]
fn parse_pg_procedure_language_before_as() {
    // LANGUAGE may precede or follow AS.
    let stmt = parse_stmt("CREATE PROCEDURE p() AS $$ BEGIN END $$ LANGUAGE plpgsql");
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure");
    };
    assert_eq!(s.language, ProcLanguage::PlPgSql);
    assert!(s.params.is_empty());
}

#[test]
fn parse_mysql_procedure_begin_end() {
    let stmt = parse_stmt("CREATE PROCEDURE p(IN a INT) BEGIN INSERT INTO t VALUES (a); END");
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure");
    };
    assert_eq!(s.language, ProcLanguage::MySql);
    assert_eq!(s.params.len(), 1);
    assert_eq!(s.params[0].mode, ProcParamMode::In);
    // Body captured verbatim, including internal `;`.
    assert!(s.body_sql.starts_with("BEGIN"));
    assert!(s.body_sql.trim_end().ends_with("END"));
    assert!(s.body_sql.contains("INSERT INTO t VALUES (a);"));
}

#[test]
fn parse_default_param_mode_is_in() {
    let stmt = parse_stmt("CREATE PROCEDURE p(a INT, INOUT b INT) BEGIN END");
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure");
    };
    assert_eq!(s.params[0].mode, ProcParamMode::In); // no keyword ⇒ IN
    assert_eq!(s.params[1].mode, ProcParamMode::InOut);
}

#[test]
fn parse_mysql_nested_begin_end_captures_to_outer_end() {
    let stmt = parse_stmt("CREATE PROCEDURE p() BEGIN BEGIN INSERT INTO t VALUES (1); END; END");
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure");
    };
    // The captured body must include BOTH the inner and the outer END.
    assert_eq!(s.body_sql.matches("END").count(), 2);
    assert!(s.body_sql.trim_end().ends_with("END"));
}

#[test]
fn parse_mysql_case_end_does_not_truncate_body() {
    // A `CASE … END` inside a body statement must NOT prematurely close the block.
    let stmt = parse_stmt(
        "CREATE PROCEDURE p() BEGIN UPDATE t SET x = CASE WHEN x > 0 THEN 1 ELSE 0 END; INSERT INTO t VALUES (9); END",
    );
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure");
    };
    // The statement after the CASE must be inside the captured body.
    assert!(
        s.body_sql.contains("INSERT INTO t VALUES (9)"),
        "body truncated at CASE…END: {}",
        s.body_sql
    );
}

#[test]
fn parse_or_replace_procedure() {
    let stmt = parse_stmt("CREATE OR REPLACE PROCEDURE p() BEGIN END");
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure");
    };
    assert!(s.or_replace);
}

#[test]
fn parse_schema_qualified_name() {
    let stmt = parse_stmt("CREATE PROCEDURE sales.p() BEGIN END");
    let Stmt::CreateProcedure(s) = stmt else {
        panic!("expected CreateProcedure");
    };
    assert_eq!(s.name.schema.as_deref(), Some("sales"));
    assert_eq!(s.name.name, "p");
}

#[test]
fn parse_drop_procedure() {
    let stmt = parse_stmt("DROP PROCEDURE p");
    let Stmt::DropProcedure(s) = stmt else {
        panic!("expected DropProcedure, got {stmt:?}");
    };
    assert!(!s.if_exists);
    assert_eq!(s.name.name, "p");
}

#[test]
fn parse_drop_procedure_if_exists() {
    let stmt = parse_stmt("DROP PROCEDURE IF EXISTS sales.p");
    let Stmt::DropProcedure(s) = stmt else {
        panic!("expected DropProcedure");
    };
    assert!(s.if_exists);
    assert_eq!(s.name.schema.as_deref(), Some("sales"));
}

#[test]
fn parse_unterminated_begin_end_errors() {
    assert!(parse("CREATE PROCEDURE p() BEGIN INSERT INTO t VALUES (1);", None).is_err());
}
