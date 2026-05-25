//! Procedure-body sub-parser (Phase 16.7).
//!
//! Turns a stored procedure's raw body text (captured by `parse_create_procedure`)
//! into a [`ProcBody`] — `DECLARE`d locals followed by ordered [`ProcStmt`]s.
//!
//! v1 supports: `DECLARE`, assignment (`var := expr` in PL/pgSQL, `SET var = expr`
//! in MySQL), and embedded `INSERT` / `UPDATE` / `DELETE` / `CALL`. Control flow
//! (`IF`/`LOOP`/`WHILE`/`FOR`), `RAISE`, cursors, `RETURN`, and result-set-returning
//! bare `SELECT` are rejected with `NotImplemented` (deferred to later subphases).
//! To compute a value into a variable, use a scalar subquery RHS:
//! `v := (SELECT count(*) FROM t)`.

use axiomdb_catalog::ProcLanguage;
use axiomdb_core::error::DbError;

use crate::ast::{ProcBody, ProcStmt, ProcVarDecl};
use crate::lexer::{tokenize_with_sql_mode, Token};
use crate::session::SqlModeFlags;

use super::{ddl::parse_data_type, expr::parse_expr, Parser};

/// Parses a procedure body into a [`ProcBody`].
pub(crate) fn parse_proc_body(
    body_sql: &str,
    language: ProcLanguage,
    sql_mode: SqlModeFlags,
) -> Result<ProcBody, DbError> {
    let tokens = tokenize_with_sql_mode(body_sql, None, sql_mode)?;
    let mut p = Parser::new(&tokens, body_sql);

    let mut declares = Vec::new();

    // PL/pgSQL: the DECLARE section appears BEFORE the BEGIN block.
    if language == ProcLanguage::PlPgSql {
        parse_declares(&mut p, &mut declares)?;
    }

    p.expect(&Token::Begin)?;

    // MySQL: DECLARE statements appear at the START of the BEGIN block.
    if language == ProcLanguage::MySql {
        parse_declares(&mut p, &mut declares)?;
    }

    let mut statements = Vec::new();
    loop {
        match p.peek() {
            Token::End => break,
            Token::Eof => {
                return Err(DbError::ParseError {
                    message: "unterminated procedure body (missing END)".into(),
                    position: Some(p.current_pos()),
                });
            }
            // Tolerate stray semicolons (empty statements).
            Token::Semicolon => {
                p.advance();
            }
            _ => {
                statements.push(parse_one_proc_stmt(&mut p)?);
                // A statement is terminated by `;` (optional right before END).
                let _ = p.eat(&Token::Semicolon);
            }
        }
    }
    p.expect(&Token::End)?;
    let _ = p.eat(&Token::Semicolon); // optional trailing `;` after END

    Ok(ProcBody {
        declares,
        statements,
    })
}

/// True if the current token is the case-insensitive identifier `kw`.
fn is_ident_ci(tok: &Token, kw: &str) -> bool {
    matches!(tok, Token::Ident(s) if s.eq_ignore_ascii_case(kw))
}

/// Parses a run of `DECLARE name type [DEFAULT expr | := expr];` statements.
fn parse_declares(p: &mut Parser, out: &mut Vec<ProcVarDecl>) -> Result<(), DbError> {
    while is_ident_ci(p.peek(), "declare") {
        p.advance(); // DECLARE
        let name = p.parse_identifier()?;
        let ty = parse_data_type(p)?.data_type;
        let init = if p.eat(&Token::Default) {
            Some(parse_expr(p)?)
        } else if matches!(p.peek(), Token::Colon) {
            // `:=` is lexed as Colon then Eq.
            p.advance();
            p.expect(&Token::Eq)?;
            Some(parse_expr(p)?)
        } else {
            None
        };
        p.expect(&Token::Semicolon)?;
        out.push(ProcVarDecl { name, ty, init });
    }
    Ok(())
}

/// Parses a single body statement (the v1 supported subset).
fn parse_one_proc_stmt(p: &mut Parser) -> Result<ProcStmt, DbError> {
    // MySQL assignment: `SET var = expr`.
    if matches!(p.peek(), Token::Set) {
        p.advance();
        let target = p.parse_identifier()?;
        p.expect(&Token::Eq)?;
        let value = parse_expr(p)?;
        return Ok(ProcStmt::Assign { target, value });
    }

    // PL/pgSQL assignment: `var := expr` (lexed as Ident, Colon, Eq).
    if matches!(p.peek(), Token::Ident(_)) && matches!(p.peek_at(1), Token::Colon) {
        let target = p.parse_identifier()?;
        p.advance(); // ':'
        p.expect(&Token::Eq)?;
        let value = parse_expr(p)?;
        return Ok(ProcStmt::Assign { target, value });
    }

    // Embedded DML / nested CALL — parse with the main statement parser.
    if matches!(
        p.peek(),
        Token::Insert | Token::Update | Token::Delete | Token::Call
    ) {
        let stmt = p.parse_stmt()?;
        return Ok(ProcStmt::Sql(Box::new(stmt)));
    }

    // Everything else (SELECT, IF, WHILE, LOOP, FOR, RAISE, cursors, RETURN, …)
    // is not supported in v1.
    Err(DbError::NotImplemented {
        feature: format!(
            "statement starting with {:?} is not yet supported in procedure bodies; \
             v1 supports DECLARE, assignment (:= / SET), and INSERT/UPDATE/DELETE/CALL \
             (use `v := (SELECT …)` to read a value) — control flow, RAISE, cursors, and \
             result-set SELECT are deferred to Phase 16.7.x/16.8",
            p.peek()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Stmt;

    fn body(sql: &str, lang: ProcLanguage) -> ProcBody {
        parse_proc_body(sql, lang, SqlModeFlags::default())
            .unwrap_or_else(|e| panic!("parse_proc_body failed for {sql}\n  error: {e:?}"))
    }

    #[test]
    fn mysql_declare_then_statements() {
        let b = body(
            "BEGIN DECLARE v INT DEFAULT 1; SET v = v + 1; INSERT INTO t VALUES (v); END",
            ProcLanguage::MySql,
        );
        assert_eq!(b.declares.len(), 1);
        assert_eq!(b.declares[0].name, "v");
        assert!(b.declares[0].init.is_some());
        assert_eq!(b.statements.len(), 2);
        assert!(matches!(b.statements[0], ProcStmt::Assign { .. }));
        assert!(matches!(b.statements[1], ProcStmt::Sql(_)));
    }

    #[test]
    fn pg_declare_before_begin_with_walrus_init() {
        let b = body(
            " DECLARE v INT := 5; BEGIN v := v * 2; UPDATE t SET x = v; END ",
            ProcLanguage::PlPgSql,
        );
        assert_eq!(b.declares.len(), 1);
        assert!(b.declares[0].init.is_some());
        assert_eq!(b.statements.len(), 2);
        match &b.statements[0] {
            ProcStmt::Assign { target, .. } => assert_eq!(target, "v"),
            other => panic!("expected Assign, got {other:?}"),
        }
    }

    #[test]
    fn assign_from_scalar_subquery() {
        let b = body(
            "BEGIN SET total = (SELECT count(*) FROM t); END",
            ProcLanguage::MySql,
        );
        assert_eq!(b.statements.len(), 1);
        assert!(matches!(b.statements[0], ProcStmt::Assign { .. }));
    }

    #[test]
    fn empty_body_ok() {
        let b = body("BEGIN END", ProcLanguage::MySql);
        assert!(b.declares.is_empty());
        assert!(b.statements.is_empty());
    }

    #[test]
    fn embedded_call_is_sql() {
        let b = body("BEGIN CALL other(1); END", ProcLanguage::MySql);
        assert!(matches!(&b.statements[0], ProcStmt::Sql(s) if matches!(**s, Stmt::Call { .. })));
    }

    #[test]
    fn bare_select_is_not_implemented() {
        let r = parse_proc_body(
            "BEGIN SELECT 1; END",
            ProcLanguage::MySql,
            SqlModeFlags::default(),
        );
        assert!(matches!(r, Err(DbError::NotImplemented { .. })));
    }

    #[test]
    fn control_flow_is_not_implemented() {
        let r = parse_proc_body(
            "BEGIN IF x THEN INSERT INTO t VALUES (1); END IF; END",
            ProcLanguage::MySql,
            SqlModeFlags::default(),
        );
        assert!(matches!(r, Err(DbError::NotImplemented { .. })));
    }

    #[test]
    fn multiple_declares() {
        let b = body(
            "BEGIN DECLARE a INT; DECLARE b TEXT DEFAULT 'x'; INSERT INTO t VALUES (a); END",
            ProcLanguage::MySql,
        );
        assert_eq!(b.declares.len(), 2);
        assert_eq!(b.declares[0].name, "a");
        assert!(b.declares[0].init.is_none());
        assert_eq!(b.declares[1].name, "b");
        assert!(b.declares[1].init.is_some());
    }
}
