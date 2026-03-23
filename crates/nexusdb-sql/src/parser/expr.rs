//! Expression sub-parser — parses [`Expr`] from the token stream.
//!
//! ## Operator precedence (highest to lowest)
//!
//! ```text
//! atom       ::= literal | identifier | '(' expr ')'
//! unary      ::= '-' unary | NOT unary | atom
//! comparison ::= unary (cmp_op unary)?
//! and_expr   ::= comparison (AND comparison)*
//! or_expr    ::= and_expr (OR and_expr)*
//! expr       ::= or_expr
//! ```
//!
//! This covers DEFAULT values and CHECK expressions in DDL (Phase 4.3).
//! Extended in Phase 4.4 with BETWEEN, LIKE, IN, arithmetic, function calls.

use nexusdb_core::error::DbError;
use nexusdb_types::Value;

use crate::{
    expr::{BinaryOp, Expr, UnaryOp},
    lexer::Token,
};

use super::Parser;

/// Parse a full SQL expression.
pub(crate) fn parse_expr(p: &mut Parser) -> Result<Expr, DbError> {
    parse_or(p)
}

// ── Recursive descent ─────────────────────────────────────────────────────────

fn parse_or(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_and(p)?;
    while p.eat(&Token::Or) {
        let right = parse_and(p)?;
        left = Expr::BinaryOp {
            op: BinaryOp::Or,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_and(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_not(p)?;
    while p.eat(&Token::And) {
        let right = parse_not(p)?;
        left = Expr::BinaryOp {
            op: BinaryOp::And,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

fn parse_not(p: &mut Parser) -> Result<Expr, DbError> {
    if p.eat(&Token::Not) {
        let operand = parse_not(p)?;
        return Ok(Expr::UnaryOp {
            op: UnaryOp::Not,
            operand: Box::new(operand),
        });
    }
    parse_comparison(p)
}

fn parse_comparison(p: &mut Parser) -> Result<Expr, DbError> {
    let left = parse_unary(p)?;
    let op = match p.peek() {
        Token::Eq => BinaryOp::Eq,
        Token::NotEq => BinaryOp::NotEq,
        Token::Lt => BinaryOp::Lt,
        Token::LtEq => BinaryOp::LtEq,
        Token::Gt => BinaryOp::Gt,
        Token::GtEq => BinaryOp::GtEq,
        _ => return Ok(left),
    };
    p.advance();
    let right = parse_unary(p)?;
    Ok(Expr::BinaryOp {
        op,
        left: Box::new(left),
        right: Box::new(right),
    })
}

fn parse_unary(p: &mut Parser) -> Result<Expr, DbError> {
    if p.eat(&Token::Minus) {
        let operand = parse_unary(p)?;
        return Ok(Expr::UnaryOp {
            op: UnaryOp::Neg,
            operand: Box::new(operand),
        });
    }
    parse_atom(p)
}

fn parse_atom(p: &mut Parser) -> Result<Expr, DbError> {
    let pos = p.current_pos();

    match p.peek().clone() {
        Token::Integer(n) => {
            p.advance();
            // Produce Int if fits in i32, BigInt otherwise.
            if n >= i32::MIN as i64 && n <= i32::MAX as i64 {
                Ok(Expr::Literal(Value::Int(n as i32)))
            } else {
                Ok(Expr::Literal(Value::BigInt(n)))
            }
        }

        Token::Float(f) => {
            p.advance();
            Ok(Expr::Literal(Value::Real(f)))
        }

        Token::StringLit(s) => {
            p.advance();
            Ok(Expr::Literal(Value::Text(s)))
        }

        Token::True => {
            p.advance();
            Ok(Expr::Literal(Value::Bool(true)))
        }

        Token::False => {
            p.advance();
            Ok(Expr::Literal(Value::Bool(false)))
        }

        Token::Null => {
            p.advance();
            Ok(Expr::Literal(Value::Null))
        }

        Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_) => {
            let name = p.parse_identifier()?;
            // col_idx is 0 here — resolved by semantic analyzer (Phase 4.18)
            Ok(Expr::Column { col_idx: 0, name })
        }

        Token::LParen => {
            p.advance();
            let expr = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            Ok(expr)
        }

        Token::Star => {
            // SELECT * uses Star; in expressions it's multiply (Phase 4.4).
            // For Phase 4.3 (DDL only), Star in an expression is unexpected.
            Err(DbError::ParseError {
                message: format!(
                    "unexpected '*' in expression at position {} — arithmetic multiply not yet supported in DDL",
                    pos
                ),
            })
        }

        other => Err(DbError::ParseError {
            message: format!(
                "unexpected token {:?} in expression at position {}",
                other, pos
            ),
        }),
    }
}
