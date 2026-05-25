//! Stored procedures (Phase 16.7) — parser tests (Step 6).
//!
//! Execution/end-to-end tests are added in later steps. Here we only assert that
//! `CREATE/DROP PROCEDURE` parse correctly in both dialects and capture the body.

mod common;

use axiomdb_catalog::{ProcLanguage, ProcParamMode};
use axiomdb_core::error::DbError;
use axiomdb_sql::ast::Stmt;
use axiomdb_sql::parse;
use axiomdb_types::DataType;
use axiomdb_types::Value;
use common::{rows, run_ctx, setup_ctx};

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

// ── Execution: CREATE / DROP + CALL safety fix (Step 9) ──────────────────────────

#[test]
fn create_procedure_then_duplicate_errors_then_drop() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();

    run_ctx(
        "CREATE PROCEDURE p() BEGIN INSERT INTO t VALUES (1); END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .expect("create should succeed");

    // Re-creating without OR REPLACE errors → proves it persisted.
    let dup = run_ctx(
        "CREATE PROCEDURE p() BEGIN END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .expect_err("duplicate create should error");
    assert!(
        matches!(dup, DbError::ProcedureAlreadyExists { .. }),
        "expected ProcedureAlreadyExists, got {dup:?}"
    );

    // DROP removes it.
    run_ctx("DROP PROCEDURE p", &mut s, &mut t, &mut b, &mut ctx).expect("drop should succeed");

    // Dropping again errors (gone).
    let gone = run_ctx("DROP PROCEDURE p", &mut s, &mut t, &mut b, &mut ctx)
        .expect_err("drop of missing should error");
    assert!(matches!(gone, DbError::ProcedureNotFound { .. }));
}

#[test]
fn create_or_replace_succeeds_over_existing() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE PROCEDURE p() BEGIN END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE OR REPLACE PROCEDURE p() BEGIN INSERT INTO t VALUES (1); END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .expect("OR REPLACE should succeed over an existing procedure");
}

#[test]
fn drop_if_exists_missing_is_ok() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    run_ctx(
        "DROP PROCEDURE IF EXISTS nope",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .expect("DROP IF EXISTS on a missing procedure is a no-op success");
}

#[test]
fn call_unknown_procedure_errors_not_silent() {
    // The safety fix: CALL to an unknown procedure must error, not silently succeed.
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    let e = run_ctx("CALL nope()", &mut s, &mut t, &mut b, &mut ctx)
        .expect_err("CALL of an unknown procedure must error");
    assert!(
        matches!(e, DbError::ProcedureNotFound { .. }),
        "expected ProcedureNotFound, got {e:?}"
    );
}

#[test]
fn call_existing_procedure_resolves_not_procedure_not_found() {
    // After CREATE, CALL must resolve the procedure (it does not error with
    // ProcedureNotFound). Body execution lands in Step 10.
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE PROCEDURE p() BEGIN END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    let r = run_ctx("CALL p()", &mut s, &mut t, &mut b, &mut ctx);
    assert!(
        !matches!(r, Err(DbError::ProcedureNotFound { .. })),
        "CALL of an existing procedure must resolve it, got {r:?}"
    );
}

// ── Execution: tree-walking interpreter (Step 10) ────────────────────────────────

/// Convenience: run SQL and return its result rows (`Vec<Vec<Value>>`).
fn query(
    sql: &str,
    s: &mut axiomdb_storage::MemoryStorage,
    t: &mut axiomdb_wal::TxnManager,
    b: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) -> Vec<Vec<Value>> {
    rows(run_ctx(sql, s, t, b, ctx).unwrap_or_else(|e| panic!("query failed: {sql}\n  {e:?}")))
}

fn create_t(
    s: &mut axiomdb_storage::MemoryStorage,
    t: &mut axiomdb_wal::TxnManager,
    b: &mut axiomdb_sql::BloomRegistry,
    ctx: &mut axiomdb_sql::SessionContext,
) {
    run_ctx("CREATE TABLE t (id INT NOT NULL PRIMARY KEY)", s, t, b, ctx).unwrap();
}

#[test]
fn proc_in_param_drives_insert() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    create_t(&mut s, &mut t, &mut b, &mut ctx);
    run_ctx(
        "CREATE PROCEDURE add_row(IN x INT) BEGIN INSERT INTO t VALUES (x); END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    run_ctx("CALL add_row(5)", &mut s, &mut t, &mut b, &mut ctx).unwrap();
    let r = query("SELECT id FROM t", &mut s, &mut t, &mut b, &mut ctx);
    assert_eq!(r, vec![vec![Value::Int(5)]]);
}

#[test]
fn proc_declare_assign_and_use_variable() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    create_t(&mut s, &mut t, &mut b, &mut ctx);
    run_ctx(
        "CREATE PROCEDURE p(IN x INT) BEGIN DECLARE v INT; SET v = x + 10; INSERT INTO t VALUES (v); END",
        &mut s, &mut t, &mut b, &mut ctx,
    ).unwrap();
    run_ctx("CALL p(5)", &mut s, &mut t, &mut b, &mut ctx).unwrap();
    let r = query("SELECT id FROM t", &mut s, &mut t, &mut b, &mut ctx);
    assert_eq!(r, vec![vec![Value::Int(15)]]);
}

#[test]
fn proc_declare_init_and_multiple_statements() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    create_t(&mut s, &mut t, &mut b, &mut ctx);
    run_ctx(
        "CREATE PROCEDURE p() BEGIN DECLARE a INT DEFAULT 1; DECLARE bb INT := a + 1; INSERT INTO t VALUES (a); INSERT INTO t VALUES (bb); END",
        &mut s, &mut t, &mut b, &mut ctx,
    ).unwrap();
    run_ctx("CALL p()", &mut s, &mut t, &mut b, &mut ctx).unwrap();
    let r = query(
        "SELECT id FROM t ORDER BY id",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    );
    assert_eq!(r, vec![vec![Value::Int(1)], vec![Value::Int(2)]]);
}

#[test]
fn proc_out_param_returned_as_row() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE PROCEDURE dbl(IN x INT, OUT y INT) BEGIN SET y = x * 2; END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    // OUT placeholder argument is ignored.
    let r = query("CALL dbl(3, NULL)", &mut s, &mut t, &mut b, &mut ctx);
    assert_eq!(r, vec![vec![Value::Int(6)]]);
}

#[test]
fn proc_inout_bound_and_returned() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE PROCEDURE inc(INOUT x INT) BEGIN SET x = x + 1; END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    let r = query("CALL inc(10)", &mut s, &mut t, &mut b, &mut ctx);
    assert_eq!(r, vec![vec![Value::Int(11)]]);
}

#[test]
fn proc_assign_from_scalar_subquery_into_out() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    create_t(&mut s, &mut t, &mut b, &mut ctx);
    run_ctx(
        "INSERT INTO t VALUES (1), (2), (3)",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    run_ctx(
        "CREATE PROCEDURE cnt(OUT n INT) BEGIN SET n = (SELECT COUNT(*) FROM t); END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    let r = query("CALL cnt(NULL)", &mut s, &mut t, &mut b, &mut ctx);
    assert_eq!(r, vec![vec![Value::Int(3)]]);
}

#[test]
fn proc_no_out_params_returns_empty() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    create_t(&mut s, &mut t, &mut b, &mut ctx);
    run_ctx(
        "CREATE PROCEDURE p() BEGIN INSERT INTO t VALUES (7); END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    let res = run_ctx("CALL p()", &mut s, &mut t, &mut b, &mut ctx).unwrap();
    assert!(matches!(res, axiomdb_sql::QueryResult::Empty));
}

#[test]
fn proc_assign_to_in_param_errors() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE PROCEDURE p(IN x INT) BEGIN SET x = 1; END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    let e = run_ctx("CALL p(5)", &mut s, &mut t, &mut b, &mut ctx)
        .expect_err("assigning to an IN parameter must error");
    assert!(matches!(e, DbError::InvalidValue { .. }), "got {e:?}");
}

#[test]
fn proc_arity_mismatch_errors() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    run_ctx(
        "CREATE PROCEDURE p(IN a INT, IN b INT) BEGIN END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    let e = run_ctx("CALL p(1)", &mut s, &mut t, &mut b, &mut ctx)
        .expect_err("arity mismatch must error");
    assert!(matches!(e, DbError::InvalidValue { .. }), "got {e:?}");
}

#[test]
fn proc_error_midbody_propagates() {
    let (mut s, mut t, mut b, mut ctx) = setup_ctx();
    create_t(&mut s, &mut t, &mut b, &mut ctx);
    // Second insert duplicates the PK → the CALL must surface the error.
    run_ctx(
        "CREATE PROCEDURE p() BEGIN INSERT INTO t VALUES (1); INSERT INTO t VALUES (1); END",
        &mut s,
        &mut t,
        &mut b,
        &mut ctx,
    )
    .unwrap();
    let e = run_ctx("CALL p()", &mut s, &mut t, &mut b, &mut ctx);
    assert!(
        e.is_err(),
        "duplicate-PK mid-body must propagate, got {e:?}"
    );
}
