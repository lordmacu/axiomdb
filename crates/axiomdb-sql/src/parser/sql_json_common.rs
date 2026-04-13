//! Shared SQL/JSON grammar helpers used by `JSON_VALUE` / `JSON_QUERY` /
//! `JSON_EXISTS` (Phase 11.19) and `JSON_TABLE` (Phase 11.20).
//!
//! These parsers are intentionally small and self-contained: they start at
//! the keyword that introduces each clause (`WITH` / `WITHOUT` / `KEEP` /
//! `OMIT` / `PASSING`) and return the compiled AST fragment. Context-
//! sensitive restrictions (e.g. "WRAPPER is only valid on JSON_QUERY") are
//! enforced by the caller.

use axiomdb_core::error::DbError;

use crate::expr::{Expr, SqlJsonQuotes, SqlJsonWrapper};
use crate::lexer::Token;
use crate::parser::expr::parse_expr;
use crate::parser::Parser;

/// Parses an optional `WITH [CONDITIONAL|UNCONDITIONAL] [ARRAY] WRAPPER`
/// or `WITHOUT [ARRAY] WRAPPER` clause. Returns `None` if neither keyword
/// is present (so the caller can keep its own default — usually `Without`
/// for JSON_QUERY and JSON_TABLE regular columns).
pub(crate) fn parse_optional_wrapper(
    p: &mut Parser<'_>,
) -> Result<Option<SqlJsonWrapper>, DbError> {
    if matches!(p.peek(), Token::With) {
        p.advance();
        let kind = if p.eat_ident_ci("CONDITIONAL") {
            SqlJsonWrapper::Conditional
        } else {
            let _ = p.eat_ident_ci("UNCONDITIONAL");
            SqlJsonWrapper::Unconditional
        };
        let _ = p.eat_ident_ci("ARRAY");
        if !p.eat_ident_ci("WRAPPER") {
            return Err(DbError::ParseError {
                message: "expected WRAPPER after WITH [CONDITIONAL|UNCONDITIONAL] [ARRAY]".into(),
                position: Some(p.current_pos()),
            });
        }
        return Ok(Some(kind));
    }
    if p.eat_ident_ci("WITHOUT") {
        let _ = p.eat_ident_ci("ARRAY");
        if !p.eat_ident_ci("WRAPPER") {
            return Err(DbError::ParseError {
                message: "expected WRAPPER after WITHOUT [ARRAY]".into(),
                position: Some(p.current_pos()),
            });
        }
        return Ok(Some(SqlJsonWrapper::Without));
    }
    Ok(None)
}

/// Parses an optional `KEEP QUOTES [ON SCALAR STRING]` or `OMIT QUOTES
/// [ON SCALAR STRING]` clause. Returns `None` if neither keyword is
/// present.
pub(crate) fn parse_optional_quotes(p: &mut Parser<'_>) -> Result<Option<SqlJsonQuotes>, DbError> {
    let keep = p.eat_ident_ci("KEEP");
    let omit = !keep && p.eat_ident_ci("OMIT");
    if !keep && !omit {
        return Ok(None);
    }
    if !p.eat_ident_ci("QUOTES") {
        return Err(DbError::ParseError {
            message: format!(
                "expected QUOTES after {}",
                if keep { "KEEP" } else { "OMIT" }
            ),
            position: Some(p.current_pos()),
        });
    }
    if matches!(p.peek(), Token::On) {
        p.advance();
        if !p.eat_ident_ci("SCALAR") || !p.eat_ident_ci("STRING") {
            return Err(DbError::ParseError {
                message: "expected SCALAR STRING after ON in QUOTES clause".into(),
                position: Some(p.current_pos()),
            });
        }
    }
    Ok(Some(if keep {
        SqlJsonQuotes::Keep
    } else {
        SqlJsonQuotes::Omit
    }))
}

/// Parses a `PASSING expr AS name [, expr AS name]*` clause, assuming the
/// `PASSING` keyword has NOT yet been consumed. Returns an empty vector
/// when no `PASSING` keyword is found (caller's default).
pub(crate) fn parse_optional_passing(p: &mut Parser<'_>) -> Result<Vec<(Expr, String)>, DbError> {
    if !p.eat_ident_ci("PASSING") {
        return Ok(Vec::new());
    }
    let mut bindings = Vec::new();
    loop {
        let val_expr = parse_expr(p)?;
        if !matches!(p.peek(), Token::As) {
            return Err(DbError::ParseError {
                message: "expected AS after PASSING expression".into(),
                position: Some(p.current_pos()),
            });
        }
        p.advance();
        let var_name = match p.peek().clone() {
            Token::Ident(n) | Token::QuotedIdent(n) => {
                p.advance();
                n.to_string()
            }
            Token::DqIdent(n) => {
                p.advance();
                n
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!("expected identifier after AS, got {other:?}"),
                    position: Some(p.current_pos()),
                });
            }
        };
        bindings.push((val_expr, var_name));
        if !matches!(p.peek(), Token::Comma) {
            break;
        }
        p.advance();
    }
    Ok(bindings)
}
